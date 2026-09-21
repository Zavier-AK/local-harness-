import { FormEvent, useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

type Bounds = {
  x: number;
  y: number;
  width: number;
  height: number;
};

type DevServer = {
  url: string;
  port: number;
  hinted_by: string[];
};

type Props = {
  active: boolean;
  obscured: boolean;
  projectRoot: string;
  workerRoots: string[];
};

export default function PreviewPanel({
  active,
  obscured,
  projectRoot,
  workerRoots,
}: Props) {
  const viewport = useRef<HTMLDivElement>(null);
  const created = useRef(false);
  const creating = useRef<Promise<void> | null>(null);
  const shouldShow = useRef(false);
  const addressRef = useRef("http://localhost:3000/");
  const scanned = useRef(false);

  const [address, setAddress] = useState(addressRef.current);
  const [servers, setServers] = useState<DevServer[]>([]);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const bounds = useCallback((): Bounds | null => {
    const element = viewport.current;
    if (!element) return null;
    const rect = element.getBoundingClientRect();
    if (rect.width < 1 || rect.height < 1) return null;
    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
  }, []);

  const ensurePreview = useCallback(
    (nextBounds: Bounds): Promise<void> => {
      if (created.current) {
        return invoke<void>("set_preview_bounds", { bounds: nextBounds });
      }
      if (creating.current) return creating.current;

      const pending = invoke<string>("create_preview", {
        url: addressRef.current,
        bounds: nextBounds,
      })
        .then((normalized) => {
          created.current = true;
          addressRef.current = normalized;
          setAddress(normalized);
          setError(null);
          if (!shouldShow.current) {
            return invoke<void>("set_preview_visible", { visible: false });
          }
        })
        .catch((cause) => {
          setError(String(cause));
          throw cause;
        })
        .finally(() => {
          creating.current = null;
        });
      creating.current = pending;
      return pending;
    },
    [],
  );

  const scan = useCallback(async () => {
    setScanning(true);
    try {
      const ownPort =
        window.location.protocol === "http:" && window.location.port
          ? [Number(window.location.port)]
          : [];
      setServers(
        await invoke<DevServer[]>("probe_preview_servers", {
          projectRoot,
          workerRoots,
          excludedPorts: ownPort,
        }),
      );
      setError(null);
    } catch (cause) {
      setError(String(cause));
    } finally {
      setScanning(false);
    }
  }, [projectRoot, workerRoots]);

  useEffect(() => {
    if (active && !scanned.current) {
      scanned.current = true;
      void scan();
    }
  }, [active, scan]);

  // A native child webview always paints above HTML. Hide it before a drawer or the
  // Chat tab needs that same part of the window.
  useLayoutEffect(() => {
    shouldShow.current = active && !obscured;
    if (!shouldShow.current) {
      void invoke("set_preview_visible", { visible: false });
      return;
    }

    const nextBounds = bounds();
    if (nextBounds) {
      void ensurePreview(nextBounds).then(() =>
        invoke("set_preview_visible", { visible: shouldShow.current }),
      );
    }
  }, [active, obscured, bounds, ensurePreview]);

  useEffect(() => {
    const element = viewport.current;
    if (!element) return;

    let frame = 0;
    const syncBounds = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        const nextBounds = bounds();
        if (!nextBounds || !shouldShow.current) return;
        if (created.current) {
          void invoke("set_preview_bounds", { bounds: nextBounds });
        } else {
          void ensurePreview(nextBounds);
        }
      });
    };
    const observer = new ResizeObserver(syncBounds);
    observer.observe(element);
    window.addEventListener("resize", syncBounds);
    syncBounds();

    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
      window.removeEventListener("resize", syncBounds);
    };
  }, [bounds, ensurePreview]);

  async function navigate(nextAddress: string) {
    addressRef.current = nextAddress;
    setAddress(nextAddress);
    setError(null);
    try {
      if (!created.current) {
        const nextBounds = bounds();
        if (!nextBounds) return;
        await ensurePreview(nextBounds);
      } else {
        const normalized = await invoke<string>("navigate_preview", { url: nextAddress });
        addressRef.current = normalized;
        setAddress(normalized);
      }
    } catch (cause) {
      setError(String(cause));
    }
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    void navigate(address);
  }

  async function reload() {
    if (!created.current) return;
    try {
      await invoke("reload_preview");
      setError(null);
    } catch (cause) {
      setError(String(cause));
    }
  }

  return (
    <section className="preview">
      <form className="preview-toolbar" onSubmit={submit}>
        <input
          value={address}
          onChange={(event) => setAddress(event.target.value)}
          aria-label="Preview URL"
          placeholder="http://localhost:3000"
          spellCheck={false}
        />
        <button type="submit">Go</button>
        <button type="button" onClick={() => void reload()} disabled={!created.current} title="Reload">
          ↻
        </button>
        <select
          aria-label="Detected localhost servers"
          value=""
          onChange={(event) => {
            if (event.target.value) void navigate(event.target.value);
          }}
          disabled={scanning || servers.length === 0}
        >
          <option value="">
            {scanning
              ? "Scanning…"
              : servers.length === 0
                ? "No servers found"
                : `${servers.length} server${servers.length === 1 ? "" : "s"} found`}
          </option>
          {servers.map((server) => (
            <option
              key={server.url}
              value={server.url}
              title={
                server.hinted_by.length > 0
                  ? `Hinted by ${server.hinted_by.join(", ")}`
                  : "Found on a common development port"
              }
            >
              localhost:{server.port}
              {server.hinted_by.length > 0 ? " · project hint" : ""}
            </option>
          ))}
        </select>
        <button type="button" onClick={() => void scan()} disabled={scanning}>
          Scan
        </button>
      </form>
      {error && <div className="preview-error">{error}</div>}
      <div className="preview-viewport" ref={viewport}>
        <p className="muted">Open a detected local server or enter its loopback HTTP URL.</p>
      </div>
    </section>
  );
}
