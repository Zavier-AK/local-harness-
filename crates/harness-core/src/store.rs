//! SQLite persistence: sessions, runs, the event log, and usage rollups.
//!
//! The usage tables are the point. The binding constraint on this harness is the
//! subscription's rolling window, not dollars, so every run's token counts are recorded
//! per provider and can be summed over an arbitrary window.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use time::OffsetDateTime;

use crate::event::{HarnessEvent, Usage};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id             TEXT PRIMARY KEY,
    title          TEXT,
    project_root   TEXT NOT NULL,
    created_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS runs (
    id                  TEXT PRIMARY KEY,
    session_id          TEXT NOT NULL REFERENCES sessions(id),
    role                TEXT NOT NULL,
    provider            TEXT NOT NULL,
    model               TEXT,
    isolation           TEXT NOT NULL,
    backend_session_id  TEXT,
    status              TEXT NOT NULL,
    started_at          INTEGER NOT NULL,
    finished_at         INTEGER
);

CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL REFERENCES sessions(id),
    stream_key  TEXT NOT NULL,
    at          INTEGER NOT NULL,
    payload     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS usage (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id           TEXT NOT NULL REFERENCES sessions(id),
    run_id               TEXT NOT NULL,
    provider             TEXT NOT NULL,
    model                TEXT,
    at                   INTEGER NOT NULL,
    input_tokens         INTEGER NOT NULL,
    output_tokens        INTEGER NOT NULL,
    cache_creation_tokens INTEGER NOT NULL,
    cache_read_tokens    INTEGER NOT NULL,
    cost_usd             REAL
);

CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id, id);
CREATE INDEX IF NOT EXISTS idx_usage_window   ON usage(provider, at);
CREATE INDEX IF NOT EXISTS idx_runs_session   ON runs(session_id);
"#;

pub struct Store {
    conn: Connection,
}

/// Token totals for one provider over some window.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ProviderUsage {
    pub provider: String,
    pub usage: Usage,
    pub cost_usd: f64,
    pub runs: u64,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).context("opening harness database")?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .context("configuring sqlite")?;
        conn.execute_batch(SCHEMA).context("applying schema")?;
        Ok(Self { conn })
    }

    fn now() -> i64 {
        OffsetDateTime::now_utc().unix_timestamp()
    }

    pub fn create_session(&self, id: &str, title: Option<&str>, project_root: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (id, title, project_root, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, title, project_root, Self::now()],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_run(
        &self,
        id: &str,
        session_id: &str,
        role: &str,
        provider: &str,
        model: Option<&str>,
        isolation: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO runs (id, session_id, role, provider, model, isolation, status, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7)",
            params![id, session_id, role, provider, model, isolation, Self::now()],
        )?;
        Ok(())
    }

    pub fn finish_run(&self, run_id: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET status = ?2, finished_at = ?3 WHERE id = ?1",
            params![run_id, status, Self::now()],
        )?;
        Ok(())
    }

    pub fn set_backend_session_id(&self, run_id: &str, backend_session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET backend_session_id = ?2 WHERE id = ?1",
            params![run_id, backend_session_id],
        )?;
        Ok(())
    }

    /// The backend session id we can hand `--resume` / `codex exec resume` if a process dies.
    pub fn backend_session_id(&self, run_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT backend_session_id FROM runs WHERE id = ?1",
                params![run_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub fn append_event(&self, session_id: &str, event: &HarnessEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (session_id, stream_key, at, payload) VALUES (?1, ?2, ?3, ?4)",
            params![
                session_id,
                event.stream_key(),
                Self::now(),
                serde_json::to_string(event)?
            ],
        )?;
        Ok(())
    }

    pub fn record_usage(
        &self,
        session_id: &str,
        run_id: &str,
        provider: &str,
        model: Option<&str>,
        usage: &Usage,
        cost_usd: Option<f64>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO usage (session_id, run_id, provider, model, at, input_tokens, output_tokens,
                                cache_creation_tokens, cache_read_tokens, cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session_id,
                run_id,
                provider,
                model,
                Self::now(),
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_creation_input_tokens,
                usage.cache_read_input_tokens,
                cost_usd,
            ],
        )?;
        Ok(())
    }

    /// Usage per provider over the last `seconds`. Pass 5 * 3600 for the rolling window
    /// that actually governs a Pro subscription.
    pub fn usage_window(&self, seconds: i64) -> Result<Vec<ProviderUsage>> {
        let cutoff = Self::now() - seconds;
        let mut stmt = self.conn.prepare(
            "SELECT provider,
                    SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_creation_tokens), SUM(cache_read_tokens),
                    COALESCE(SUM(cost_usd), 0.0), COUNT(*)
             FROM usage WHERE at >= ?1 GROUP BY provider ORDER BY provider",
        )?;

        let rows = stmt.query_map(params![cutoff], |row| {
            Ok(ProviderUsage {
                provider: row.get(0)?,
                usage: Usage {
                    input_tokens: row.get(1)?,
                    output_tokens: row.get(2)?,
                    cache_creation_input_tokens: row.get(3)?,
                    cache_read_input_tokens: row.get(4)?,
                },
                cost_usd: row.get(5)?,
                runs: row.get(6)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn session_usage(&self, session_id: &str) -> Result<ProviderUsage> {
        let mut total = ProviderUsage {
            provider: "all".into(),
            ..Default::default()
        };
        let mut stmt = self.conn.prepare(
            "SELECT SUM(input_tokens), SUM(output_tokens), SUM(cache_creation_tokens),
                    SUM(cache_read_tokens), COALESCE(SUM(cost_usd), 0.0), COUNT(*)
             FROM usage WHERE session_id = ?1",
        )?;
        stmt.query_row(params![session_id], |row| {
            total.usage = Usage {
                input_tokens: row.get::<_, Option<u64>>(0)?.unwrap_or(0),
                output_tokens: row.get::<_, Option<u64>>(1)?.unwrap_or(0),
                cache_creation_input_tokens: row.get::<_, Option<u64>>(2)?.unwrap_or(0),
                cache_read_input_tokens: row.get::<_, Option<u64>>(3)?.unwrap_or(0),
            };
            total.cost_usd = row.get(4)?;
            total.runs = row.get(5)?;
            Ok(())
        })?;
        Ok(total)
    }

    /// Replay a session's event log, for reopening a session in the UI.
    pub fn replay(&self, session_id: &str) -> Result<Vec<HarnessEvent>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM events WHERE session_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(params![session_id], |row| row.get::<_, String>(0))?;

        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_session() -> Store {
        let store = Store::in_memory().unwrap();
        store.create_session("s1", Some("test"), "/tmp/project").unwrap();
        store
    }

    #[test]
    fn records_and_replays_events() {
        let store = store_with_session();
        let event = HarnessEvent::AssistantText {
            run_id: "r1".into(),
            text: "hello".into(),
            partial: false,
        };
        store.append_event("s1", &event).unwrap();

        let replayed = store.replay("s1").unwrap();
        assert_eq!(replayed, vec![event]);
    }

    #[test]
    fn rolls_usage_up_per_provider() {
        let store = store_with_session();
        store.create_run("r1", "s1", "planner", "claude", Some("opus"), "readonly").unwrap();
        store.create_run("r2", "s1", "tester", "codex", None, "readonly").unwrap();

        let claude_usage = Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_creation_input_tokens: 15_105,
            cache_read_input_tokens: 18_766,
        };
        store.record_usage("s1", "r1", "claude", Some("opus"), &claude_usage, Some(0.06)).unwrap();
        store.record_usage("s1", "r2", "codex", None, &Usage { input_tokens: 5, output_tokens: 7, ..Default::default() }, None).unwrap();

        let window = store.usage_window(3600).unwrap();
        assert_eq!(window.len(), 2);

        let claude = window.iter().find(|p| p.provider == "claude").unwrap();
        assert_eq!(claude.usage.output_tokens, 20);
        // The floor is the number worth watching, so make sure it survives the round trip.
        assert_eq!(claude.usage.cache_creation_input_tokens, 15_105);
        assert_eq!(claude.usage.total_input(), 10 + 15_105 + 18_766);
        assert_eq!(claude.runs, 1);

        let session_total = store.session_usage("s1").unwrap();
        assert_eq!(session_total.runs, 2);
        assert_eq!(session_total.usage.output_tokens, 27);
    }

    #[test]
    fn usage_window_excludes_older_rows() {
        let store = store_with_session();
        store.create_run("r1", "s1", "planner", "claude", None, "readonly").unwrap();
        store.record_usage("s1", "r1", "claude", None, &Usage { output_tokens: 1, ..Default::default() }, None).unwrap();

        // Backdate it past the window.
        store.conn.execute("UPDATE usage SET at = at - 7200", []).unwrap();
        assert!(store.usage_window(3600).unwrap().is_empty());
        // ...but it still counts toward the session total.
        assert_eq!(store.session_usage("s1").unwrap().runs, 1);
    }

    #[test]
    fn round_trips_backend_session_id() {
        let store = store_with_session();
        store.create_run("r1", "s1", "planner", "claude", None, "readonly").unwrap();
        assert_eq!(store.backend_session_id("r1").unwrap(), None);

        store.set_backend_session_id("r1", "abc-123").unwrap();
        assert_eq!(store.backend_session_id("r1").unwrap(), Some("abc-123".into()));
    }
}
