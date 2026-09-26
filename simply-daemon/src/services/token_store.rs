//! Per-user, per-MCP-server OAuth token store.
//!
//! Tokens are kept in-memory and, when a persistence path is configured,
//! mirrored to a JSON file under the data dir so they survive daemon restarts.
//! The file is written with 0600 perms; it holds the same kind of secrets
//! already kept in plaintext in settings.toml/lumina.toml (API keys, bot token),
//! so it shares that trust boundary. Without a path the store is purely
//! in-memory (cleared on restart).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use simply_core::storage::ids::UserId;

/// Current wall-clock time as unix epoch seconds.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Key for the token store: (user_id, server_id).
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
struct TokenKey {
    user_id: String,
    server_id: String,
}

/// A stored OAuth token.
#[derive(Clone, Debug)]
pub struct McpUserToken {
    pub access_token: String,
    /// Long-lived refresh token, when the provider returns one. Used to mint a
    /// fresh access token after the current one expires.
    pub refresh_token: Option<String>,
    /// Expiry as unix epoch seconds (wall-clock, so it survives a restart).
    pub expires_at: Option<i64>,
    /// Display identity from the OAuth provider (e.g. email).
    pub identity: Option<String>,
}

impl McpUserToken {
    pub fn is_expired(&self) -> bool {
        self.expires_at.map(|e| now_unix() >= e).unwrap_or(false)
    }

    pub fn expires_in_secs(&self) -> Option<u64> {
        self.expires_at.map(|e| (e - now_unix()).max(0) as u64)
    }
}

/// Flat on-disk representation of one token (JSON maps can't key on a struct).
#[derive(Serialize, Deserialize)]
struct PersistedEntry {
    user_id: String,
    server_id: String,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    identity: Option<String>,
}

/// Token store: in-memory, optionally mirrored to disk.
pub struct TransientTokenStore {
    tokens: Mutex<HashMap<TokenKey, McpUserToken>>,
    persist_path: Option<PathBuf>,
}

impl Default for TransientTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TransientTokenStore {
    /// In-memory only — nothing written to disk.
    pub fn new() -> Self {
        Self { tokens: Mutex::new(HashMap::new()), persist_path: None }
    }

    /// In-memory + persisted to `path`. Loads any existing tokens from `path`.
    pub fn with_persistence(path: PathBuf) -> Self {
        let store = Self { tokens: Mutex::new(HashMap::new()), persist_path: Some(path) };
        store.load();
        store
    }

    /// Store a token for (user_id, server_id).
    pub fn store(&self, user_id: &UserId, server_id: &str, token: McpUserToken) {
        let key = TokenKey { user_id: user_id.as_str().to_string(), server_id: server_id.to_string() };
        self.tokens.lock().unwrap().insert(key, token);
        self.save();
    }

    /// Get a valid (non-expired) token for (user_id, server_id).
    pub fn get(&self, user_id: &UserId, server_id: &str) -> Option<McpUserToken> {
        let key = TokenKey { user_id: user_id.as_str().to_string(), server_id: server_id.to_string() };
        let tokens = self.tokens.lock().unwrap();
        tokens.get(&key).filter(|t| !t.is_expired()).cloned()
    }

    /// Get a token regardless of expiry — used when refreshing an expired token.
    pub fn get_raw(&self, user_id: &UserId, server_id: &str) -> Option<McpUserToken> {
        let key = TokenKey { user_id: user_id.as_str().to_string(), server_id: server_id.to_string() };
        self.tokens.lock().unwrap().get(&key).cloned()
    }

    /// Check if a valid token exists.
    pub fn has_token(&self, user_id: &UserId, server_id: &str) -> bool {
        self.get(user_id, server_id).is_some()
    }

    /// Remove a token.
    pub fn remove(&self, user_id: &UserId, server_id: &str) {
        let key = TokenKey { user_id: user_id.as_str().to_string(), server_id: server_id.to_string() };
        self.tokens.lock().unwrap().remove(&key);
        self.save();
    }

    fn load(&self) {
        let Some(path) = &self.persist_path else { return };
        let Ok(data) = std::fs::read_to_string(path) else { return };
        let entries: Vec<PersistedEntry> = match serde_json::from_str(&data) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "could not parse persisted token store; ignoring");
                return;
            }
        };
        let mut tokens = self.tokens.lock().unwrap();
        for e in entries {
            tokens.insert(
                TokenKey { user_id: e.user_id, server_id: e.server_id },
                McpUserToken {
                    access_token: e.access_token,
                    refresh_token: e.refresh_token,
                    expires_at: e.expires_at,
                    identity: e.identity,
                },
            );
        }
        tracing::info!(count = tokens.len(), "loaded persisted OAuth tokens");
    }

    fn save(&self) {
        let Some(path) = &self.persist_path else { return };
        let entries: Vec<PersistedEntry> = {
            let tokens = self.tokens.lock().unwrap();
            tokens
                .iter()
                .map(|(k, t)| PersistedEntry {
                    user_id: k.user_id.clone(),
                    server_id: k.server_id.clone(),
                    access_token: t.access_token.clone(),
                    refresh_token: t.refresh_token.clone(),
                    expires_at: t.expires_at,
                    identity: t.identity.clone(),
                })
                .collect()
        };
        let json = match serde_json::to_string_pretty(&entries) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "could not serialize token store");
                return;
            }
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = write_private(path, &json) {
            tracing::warn!(error = %e, path = %path.display(), "could not write token store");
        }
    }
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
}
