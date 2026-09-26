//! Lumina-owned per-Discord-message tool-state store.
//!
//! When Lumina posts a tool-call or tool-result embed, we stash the
//! corresponding structured `ToolCall` / `ToolResult` JSON here keyed by
//! the Discord message id. On channel history replay, [`get_many`]
//! batch-fetches payloads for every bot message with embeds so the
//! agent sees real `InputContent::ToolCall` / `ToolResult` turns
//! instead of lossy text approximations — and without attaching any
//! JSON files to Discord messages.
//!
//! Lives in its own SQLite file under `lumina_data_dir()` so nuking
//! replay state doesn't touch the daemon's `noema.db`.
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

/// Kind tag used to disambiguate payload JSON.
pub const KIND_TOOL_CALL: &str = "tool_call";
pub const KIND_TOOL_RESULT: &str = "tool_result";

pub struct ToolStateStore {
    conn: Mutex<Connection>,
}

impl ToolStateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).context("open lumina tool_state db")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS tool_state (
                message_id INTEGER PRIMARY KEY,
                kind       TEXT    NOT NULL,
                payload    TEXT    NOT NULL,
                created_at INTEGER NOT NULL
            );
            "#,
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Stash a tool call/result payload under the given Discord message id.
    /// Overwrites on conflict — a resend of the same message should not
    /// happen in normal flow but isn't an error.
    pub fn put(&self, message_id: u64, kind: &str, payload: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        conn.execute(
            "INSERT OR REPLACE INTO tool_state (message_id, kind, payload, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![message_id as i64, kind, payload, now],
        )
        .context("put tool_state")?;
        Ok(())
    }

    /// Batch-fetch stashed payloads for multiple message ids. Returns a
    /// map of `message_id → (kind, payload)` for ids present; missing
    /// ids are absent from the map. Single IN-query, not N round-trips.
    pub fn get_many(&self, message_ids: &[u64]) -> Result<HashMap<u64, (String, String)>> {
        if message_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.conn.lock().unwrap();
        let placeholders = std::iter::repeat_n("?", message_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT message_id, kind, payload FROM tool_state WHERE message_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut out: HashMap<u64, (String, String)> = HashMap::new();
        let rows = stmt.query_map(
            rusqlite::params_from_iter(message_ids.iter().map(|id| *id as i64)),
            |row| {
                let id: i64 = row.get(0)?;
                let kind: String = row.get(1)?;
                let payload: String = row.get(2)?;
                Ok((id as u64, (kind, payload)))
            },
        )?;
        for (id, kv) in rows.flatten() {
            out.insert(id, kv);
        }
        Ok(out)
    }
}
