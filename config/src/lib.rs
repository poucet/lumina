pub mod crypto;
pub mod lumina;
pub mod paths;
pub mod settings;
pub mod telegram;

pub use crypto::{decrypt_string, encrypt_string};
pub use lumina::{LuminaConfig, VoiceConfig};
pub use paths::PathManager;
pub use settings::{EmbeddingConfig, Settings, DEFAULT_DAEMON_PORT};
pub use telegram::{TelegramBotConfig, TelegramConfig};

/// OAuth2 scopes requested when inviting the Lumina bot to a Discord server.
/// The `applications.commands` scope is what grants slash-command registration.
pub const DISCORD_INVITE_SCOPES: &str = "bot applications.commands";

/// Discord permission bits requested in the Lumina invite URL. This is the
/// canonical value; `lumina::invite::required_permissions()` derives the same
/// set from serenity and a test there asserts the two stay in sync. Shared so
/// the daemon's setup page can build the invite link without depending on the
/// lumina crate.
pub const DISCORD_INVITE_PERMISSIONS: u64 = 37_080_128;

/// Load environment variables from .env files.
/// First loads from ~/.env (home directory), then from ./.env (project directory).
/// Project directory values take precedence over home directory values.
/// Call this before parsing CLI args to ensure env vars are available.
pub fn load_env_file() {
    // Load from home directory first (lower precedence)
    if let Some(home) = dirs::home_dir() {
        let home_env_path = home.join(".env");
        dotenv::from_path(home_env_path).ok();
    }

    // Load from project directory (higher precedence - overwrites home values)
    // dotenv::dotenv() loads from current directory's .env
    dotenv::dotenv().ok();
}
