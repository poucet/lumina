//! Telegram bot configuration.

use crate::PathManager;
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct TelegramConfig {
    #[serde(default)]
    pub telegram: TelegramBotConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramBotConfig {
    /// Telegram bot token. Can also be set via TELEGRAM_BOT_TOKEN.
    #[serde(default)]
    pub bot_token: String,
    /// Optional allowlist of Telegram user IDs. Empty means any user can talk to the bot.
    #[serde(default)]
    pub allowed_user_ids: Vec<u64>,
    /// Optional allowlist of Telegram chat IDs. Empty means any chat can talk to the bot.
    #[serde(default)]
    pub allowed_chat_ids: Vec<i64>,
    /// Number of prior turns to seed into each daemon session.
    pub history_limit: Option<usize>,
    /// Default LLM model ID for Telegram sessions.
    pub model_id: Option<String>,
    /// Whether group chats should respond when @mentioned. Defaults to true.
    pub respond_in_groups: Option<bool>,
    /// Long-poll timeout in seconds. Defaults to 30.
    pub poll_timeout_secs: Option<u64>,
}


impl Default for TelegramBotConfig {
    fn default() -> Self {
        Self {
            bot_token: String::new(),
            allowed_user_ids: Vec::new(),
            allowed_chat_ids: Vec::new(),
            history_limit: Some(40),
            model_id: None,
            respond_in_groups: Some(true),
            poll_timeout_secs: Some(30),
        }
    }
}

impl TelegramConfig {
    /// Load from telegram.toml, falling back to defaults.
    /// TELEGRAM_BOT_TOKEN takes precedence over the file value.
    pub fn load() -> Self {
        let mut cfg: Self = PathManager::telegram_config_path()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();

        if let Ok(token) = std::env::var("TELEGRAM_BOT_TOKEN") {
            if !token.is_empty() {
                cfg.telegram.bot_token = token;
            }
        }

        cfg
    }

    pub fn save(&self) -> Result<(), String> {
        let path = PathManager::telegram_config_path().ok_or("Could not determine config path")?;
        let toml =
            toml::to_string_pretty(self).map_err(|e| format!("Failed to serialize config: {e}"))?;
        fs::write(&path, toml).map_err(|e| format!("Failed to write config: {e}"))?;
        Ok(())
    }

    pub fn bot_token(&self) -> Option<&str> {
        if self.telegram.bot_token.is_empty() {
            None
        } else {
            Some(&self.telegram.bot_token)
        }
    }

    pub fn history_limit(&self) -> usize {
        self.telegram.history_limit.unwrap_or(40)
    }

    pub fn poll_timeout_secs(&self) -> u64 {
        self.telegram.poll_timeout_secs.unwrap_or(30).max(1)
    }

    pub fn respond_in_groups(&self) -> bool {
        self.telegram.respond_in_groups.unwrap_or(true)
    }
}
