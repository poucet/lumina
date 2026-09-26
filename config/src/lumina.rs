//! Lumina bot configuration

use crate::PathManager;
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LuminaConfig {
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub voice: VoiceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VoiceConfig {
    /// STT provider ID (e.g. "whisper", "voxtral")
    pub stt_provider: Option<String>,
    /// TTS provider ID (e.g. "voxtral")
    pub tts_provider: Option<String>,
    /// TTS voice ID
    pub tts_voice: Option<String>,
    /// Seconds a voice stage may stay in flight before the watchdog DMs
    /// the owner. Default: `VoiceConfig::DEFAULT_WATCHDOG_SECS`.
    #[serde(default)]
    pub watchdog_secs: Option<u64>,
}

impl VoiceConfig {
    pub const DEFAULT_WATCHDOG_SECS: u64 = 30;

    pub fn watchdog_secs(&self) -> u64 {
        self.watchdog_secs.unwrap_or(Self::DEFAULT_WATCHDOG_SECS)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscordConfig {
    /// Discord bot token — can also be set via DISCORD_BOT_TOKEN env var
    #[serde(default)]
    pub bot_token: String,
    /// Bot owner's Discord user ID
    pub owner_id: Option<u64>,
    /// Guild IDs where slash commands are registered (guild-specific, not global)
    #[serde(default)]
    pub guild_ids: Vec<u64>,
    /// Channel ID for bot status messages (online/offline)
    pub status_channel_id: Option<u64>,
    /// Category ID for AI chat channels (created by /chat new)
    pub ai_chats_category_id: Option<u64>,
    /// Max messages to load from Discord channel history as context (default: 1000)
    pub history_limit: Option<u16>,
    /// Default LLM model ID for Lumina sessions
    pub model_id: Option<String>,
}

impl LuminaConfig {
    /// Load from lumina.toml, falling back to defaults.
    /// The DISCORD_BOT_TOKEN env var takes precedence over the file value.
    pub fn load() -> Self {
        let mut cfg: Self = PathManager::lumina_config_path()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();

        if let Ok(token) = std::env::var("DISCORD_BOT_TOKEN") {
            if !token.is_empty() {
                cfg.discord.bot_token = token;
            }
        }

        cfg
    }

    /// Save config back to lumina.toml.
    pub fn save(&self) -> Result<(), String> {
        let path = PathManager::lumina_config_path()
            .ok_or("Could not determine config path")?;
        let toml = toml::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {e}"))?;
        fs::write(&path, toml)
            .map_err(|e| format!("Failed to write config: {e}"))?;
        Ok(())
    }

    /// The bot owner's Discord user id. The template's `owner_id = 0`
    /// means unset.
    pub fn owner_id(&self) -> Option<u64> {
        self.discord.owner_id.filter(|&id| id != 0)
    }

    /// Whether a Discord user is the configured bot owner.
    pub fn is_owner(&self, discord_user_id: u64) -> bool {
        self.owner_id() == Some(discord_user_id)
    }

    pub fn bot_token(&self) -> Option<&str> {
        if self.discord.bot_token.is_empty() {
            None
        } else {
            Some(&self.discord.bot_token)
        }
    }
}
