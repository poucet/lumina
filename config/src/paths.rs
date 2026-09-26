use std::path::PathBuf;
use std::sync::OnceLock;

static DATA_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();
static LOG_FILE_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

pub struct PathManager;

impl PathManager {
    /// Set a custom data directory (useful for Android/iOS where standard detection fails)
    pub fn set_data_dir(path: PathBuf) {
        let _ = DATA_DIR_OVERRIDE.set(path);
    }

    /// Set a custom log file path (overrides the default log location)
    pub fn set_log_file(path: PathBuf) {
        let _ = LOG_FILE_OVERRIDE.set(path);
    }

    // Helper to get the base data directory
    fn base_data_dir() -> Option<PathBuf> {
        if let Some(d) = DATA_DIR_OVERRIDE.get() {
            return Some(d.clone());
        }
        if let Ok(dir) = std::env::var("NOEMA_DATA_DIR") {
            return Some(PathBuf::from(dir));
        }
        // Use ~/.local/share/noema on all desktop platforms
        dirs::home_dir().map(|h| h.join(".local/share/noema"))
    }

    pub fn data_dir() -> Option<PathBuf> {
        Self::base_data_dir()
    }

    pub fn config_dir() -> Option<PathBuf> {
        Self::data_dir().map(|d: PathBuf| d.join("config"))
    }
    /// Directory containing the SQLite database
    pub fn database_dir() -> Option<PathBuf> {
        Self::data_dir().map(|d| d.join("database"))
    }

    /// Path to the main SQLite database file
    pub fn db_path() -> Option<PathBuf> {
        Self::database_dir().map(|d| d.join("noema.db"))
    }

    /// Directory for content-addressable blob storage
    pub fn blob_storage_dir() -> Option<PathBuf> {
        Self::data_dir().map(|d: PathBuf| d.join("blob_storage"))
    }

    /// Default directory for user-authored Markdown vault files.
    pub fn default_vault_root() -> Option<PathBuf> {
        Self::data_dir().map(|d: PathBuf| d.join("vault"))
    }

    /// Get the path for a specific blob by its SHA-256 hash
    /// Files are sharded by the first 2 characters of the hash
    pub fn blob_path(hash: &str) -> Option<PathBuf> {
        if hash.len() < 2 {
            return None;
        }
        let shard = &hash[0..2];
        Self::blob_storage_dir().map(|d| d.join(shard).join(hash))
    }
    /// Path to the unified settings file
    pub fn settings_path() -> Option<PathBuf> {
        Self::config_dir().map(|d| d.join("settings.toml"))
    }

    /// Path to the secrets environment file
    pub fn env_path() -> Option<PathBuf> {
        Self::config_dir().map(|d| d.join(".env"))
    }

    pub fn logs_dir() -> Option<PathBuf> {
        Self::data_dir().map(|d| d.join("logs"))
    }

    pub fn log_file_path() -> Option<PathBuf> {
        // Check for override first
        if let Some(path) = LOG_FILE_OVERRIDE.get() {
            return Some(path.clone());
        }
        Self::logs_dir().map(|d| d.join("noema.log"))
    }

    pub fn models_dir() -> Option<PathBuf> {
        Self::data_dir().map(|d| d.join("models"))
    }

    pub fn whisper_model_path() -> Option<PathBuf> {
        Self::models_dir().map(|d| d.join("ggml-base.en.bin"))
    }

    pub fn lumina_config_path() -> Option<PathBuf> {
        Self::config_dir().map(|d| d.join("lumina.toml"))
    }

    pub fn telegram_config_path() -> Option<PathBuf> {
        Self::config_dir().map(|d| d.join("telegram.toml"))
    }

    /// Directory for Lumina-owned state (separate from the daemon DB so
    /// nuking Lumina state doesn't touch `noema.db`).
    pub fn lumina_data_dir() -> Option<PathBuf> {
        Self::data_dir().map(|d| d.join("lumina"))
    }

    /// SQLite DB holding per-Discord-message tool-call / tool-result
    /// payloads so we can rehydrate structured tool turns on channel
    /// history replay without attaching the JSON to Discord messages.
    pub fn lumina_tool_state_path() -> Option<PathBuf> {
        Self::lumina_data_dir().map(|d| d.join("tool_state.db"))
    }

    /// Directory for Telegram-owned state. Conversation memory is currently
    /// in-process, but the directory is created up front for future durable
    /// bridge state without touching `noema.db`.
    pub fn telegram_data_dir() -> Option<PathBuf> {
        Self::data_dir().map(|d| d.join("telegram"))
    }

    pub fn mcp_config_path() -> Option<PathBuf> {
        Self::config_dir().map(|d| d.join("mcp.toml"))
    }

    pub fn oauth_providers_path() -> Option<PathBuf> {
        Self::config_dir().map(|d| d.join("oauth_providers.toml"))
    }

    pub fn ensure_dirs_exist() -> std::io::Result<()> {
        if let Some(d) = Self::data_dir() {
            std::fs::create_dir_all(&d)?;
        }
        if let Some(d) = Self::config_dir() {
            std::fs::create_dir_all(&d)?;
        }
        if let Some(d) = Self::database_dir() {
            std::fs::create_dir_all(&d)?;
        }
        if let Some(d) = Self::blob_storage_dir() {
            std::fs::create_dir_all(&d)?;
        }
        if let Some(d) = Self::default_vault_root() {
            std::fs::create_dir_all(&d)?;
        }
        if let Some(d) = Self::logs_dir() {
            std::fs::create_dir_all(&d)?;
        }
        if let Some(d) = Self::models_dir() {
            std::fs::create_dir_all(&d)?;
        }
        if let Some(d) = Self::lumina_data_dir() {
            std::fs::create_dir_all(&d)?;
        }
        if let Some(d) = Self::telegram_data_dir() {
            std::fs::create_dir_all(&d)?;
        }
        Ok(())
    }
}
