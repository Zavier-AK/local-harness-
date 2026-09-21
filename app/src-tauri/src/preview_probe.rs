//! Read-only discovery and validation for local development servers.
//!
//! Discovery only reads a small set of project configuration files and attempts HTTP
//! connections to likely ports. It never launches, signals, or otherwise manages a process.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time::timeout;

const COMMON_PORTS: &[u16] = &[
    3000, 3001, 4000, 4173, 4200, 4321, 5000, 5173, 5174, 8000, 8080,
];

const CONFIG_FILES: &[&str] = &[
    "package.json",
    "vite.config.js",
    "vite.config.mjs",
    "vite.config.ts",
    "vite.config.mts",
    "webpack.config.js",
    "webpack.config.ts",
    "angular.json",
    "astro.config.js",
    "astro.config.mjs",
    "astro.config.ts",
    "next.config.js",
    "next.config.mjs",
    "next.config.ts",
    "Trunk.toml",
    "tauri.conf.json",
];

const MAX_CONFIG_BYTES: u64 = 512 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(700);

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DevServer {
    pub url: String,
    pub port: u16,
    pub hinted_by: Vec<String>,
}

/// Parse user input and require plain HTTP on a loopback host.
///
/// A missing scheme is treated as `http://` for ergonomic address-bar input.
pub fn parse_loopback_http_url(input: &str) -> Result<Url, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("enter a localhost URL".into());
    }

    let candidate = if input.contains("://") {
        input.to_owned()
    } else {
        format!("http://{input}")
    };
    let url = Url::parse(&candidate).map_err(|_| "enter a valid localhost URL".to_string())?;

    if !is_loopback_http_url(&url) {
        return Err("preview URLs must use http:// on localhost or a loopback IP".into());
    }
    Ok(url)
}

/// Shared by command validation and the child webview's navigation callback.
pub fn is_loopback_http_url(url: &Url) -> bool {
    if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }

    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Look for likely ports in project/worker configuration, then verify each with HTTP.
pub async fn discover_dev_servers(
    project_root: PathBuf,
    worker_roots: Vec<PathBuf>,
    excluded_ports: HashSet<u16>,
) -> Vec<DevServer> {
    let roots = discovery_roots(project_root, worker_roots);
    let mut hints: BTreeMap<u16, BTreeSet<PathBuf>> = BTreeMap::new();

    for root in &roots {
        for port in hinted_ports(root) {
            hints.entry(port).or_default().insert(root.clone());
        }
    }

    let mut ports = Vec::new();
    for port in hints.keys().chain(COMMON_PORTS.iter()) {
        if !excluded_ports.contains(port) && !ports.contains(port) {
            ports.push(*port);
        }
    }

    let open_ports = probe_ports(&ports).await;
    let mut servers: Vec<_> = open_ports
        .into_iter()
        .map(|port| DevServer {
            url: format!("http://localhost:{port}/"),
            port,
            hinted_by: hints
                .get(&port)
                .into_iter()
                .flatten()
                .map(|path| path.display().to_string())
                .collect(),
        })
        .collect();

    // Config-backed choices are more likely to belong to this project than generic ports.
    servers.sort_by_key(|server| (server.hinted_by.is_empty(), server.port));
    servers
}

fn discovery_roots(project_root: PathBuf, worker_roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in std::iter::once(project_root).chain(worker_roots) {
        if root.is_dir() && !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

fn hinted_ports(root: &Path) -> BTreeSet<u16> {
    let mut ports = BTreeSet::new();

    for file_name in CONFIG_FILES {
        let path = root.join(file_name);
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };

        ports.extend(extract_explicit_ports(&contents));
        add_framework_defaults(&contents, &mut ports);
    }

    ports
}

fn add_framework_defaults(contents: &str, ports: &mut BTreeSet<u16>) {
    let lower = contents.to_ascii_lowercase();
    for (needle, port) in [
        ("vite", 5173),
        ("next", 3000),
        ("react-scripts", 3000),
        ("@angular/", 4200),
        ("ng serve", 4200),
        ("astro", 4321),
        ("parcel", 1234),
        ("webpack", 8080),
        ("trunk serve", 8080),
    ] {
        if lower.contains(needle) {
            ports.insert(port);
        }
    }
}

fn extract_explicit_ports(contents: &str) -> BTreeSet<u16> {
    let lower = contents.to_ascii_lowercase();
    let mut ports = BTreeSet::new();

    for marker in [
        "--port",
        " port",
        "\"port\"",
        "'port'",
        "localhost:",
        "127.0.0.1:",
        "[::1]:",
    ] {
        let mut remainder = lower.as_str();
        while let Some(index) = remainder.find(marker) {
            remainder = &remainder[index + marker.len()..];
            let digits = remainder
                .trim_start_matches(|character: char| {
                    character.is_ascii_whitespace()
                        || matches!(character, ':' | '=' | '"' | '\'')
                })
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect::<String>();
            if let Ok(port) = digits.parse::<u16>() {
                if port != 0 {
                    ports.insert(port);
                }
            }
        }
    }

    ports
}

async fn probe_ports(ports: &[u16]) -> Vec<u16> {
    let mut tasks = JoinSet::new();
    for port in ports.iter().copied() {
        tasks.spawn(async move { verify_http(port).await.then_some(port) });
    }

    let mut open = Vec::new();
    while let Some(result) = tasks.join_next().await {
        if let Ok(Some(port)) = result {
            open.push(port);
        }
    }
    open.sort_unstable();
    open
}

async fn verify_http(port: u16) -> bool {
    let addresses = [
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
        SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
    ];

    for address in addresses {
        let Ok(Ok(mut stream)) = timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await else {
            continue;
        };
        let request =
            format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
        if !matches!(
            timeout(RESPONSE_TIMEOUT, stream.write_all(request.as_bytes())).await,
            Ok(Ok(()))
        ) {
            continue;
        }

        let mut response = [0_u8; 16];
        if let Ok(Ok(read)) = timeout(RESPONSE_TIMEOUT, stream.read(&mut response)).await {
            if read >= 8 && response[..read].starts_with(b"HTTP/1.") {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::net::TcpListener;

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn accepts_and_normalizes_loopback_http_urls() {
        let cases = [
            ("localhost:3000", "http://localhost:3000/"),
            ("HTTP://LOCALHOST:8080/path?q=1", "http://localhost:8080/path?q=1"),
            ("127.42.0.9:4000", "http://127.42.0.9:4000/"),
            ("http://[::1]:5173/app", "http://[::1]:5173/app"),
        ];

        for (input, expected) in cases {
            assert_eq!(parse_loopback_http_url(input).unwrap().as_str(), expected);
        }
    }

    #[test]
    fn rejects_non_http_non_loopback_and_credential_urls() {
        for input in [
            "",
            "https://localhost:3000",
            "ftp://localhost:3000",
            "http://example.com",
            "http://localhost.example.com",
            "http://192.168.1.10:3000",
            "http://user@localhost:3000",
            "javascript:alert(1)",
        ] {
            assert!(parse_loopback_http_url(input).is_err(), "{input} was accepted");
        }
    }

    #[test]
    fn extracts_common_config_port_syntax_and_ignores_invalid_ports() {
        let contents = r#"
          vite --port 4310
          next dev --port=4311
          const config = { port: 4312 };
          { "port": 4313, "url": "http://localhost:4314" }
          proxy=http://127.0.0.1:4315
          ipv6=http://[::1]:4316
          --port 0 --port 99999
        "#;

        assert_eq!(
            extract_explicit_ports(contents),
            BTreeSet::from([4310, 4311, 4312, 4313, 4314, 4315, 4316])
        );
    }

    #[test]
    fn combines_explicit_and_framework_port_hints() {
        let root = temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts":{"dev":"vite --port 6123"},"devDependencies":{"vite":"latest"}}"#,
        )
        .unwrap();

        assert_eq!(hinted_ports(&root), BTreeSet::from([5173, 6123]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn probing_requires_an_http_response() {
        let http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let http_port = http.local_addr().unwrap().port();
        let http_task = tokio::spawn(async move {
            let (mut socket, _) = http.accept().await.unwrap();
            let mut request = [0_u8; 128];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });

        let non_http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let non_http_port = non_http.local_addr().unwrap().port();
        let non_http_task = tokio::spawn(async move {
            let (mut socket, _) = non_http.accept().await.unwrap();
            let mut request = [0_u8; 128];
            let _ = socket.read(&mut request).await;
            socket.write_all(b"not http").await.unwrap();
        });

        assert_eq!(
            probe_ports(&[non_http_port, http_port]).await,
            vec![http_port]
        );
        http_task.await.unwrap();
        non_http_task.await.unwrap();
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "harness-preview-probe-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
