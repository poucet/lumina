//! /voice — Discord voice channel commands.

use std::sync::Arc;

use serenity::all::{
    AutocompleteChoice, ChannelId, CommandInteraction, CreateAutocompleteResponse,
    CreateInteractionResponse,
};
use serenity::builder::GetMessages;
use simply_daemon_api::*;

use super::{sub_string_arg, LuminaContext};
use crate::voice::{VoiceManagerKey, VoiceMode};

#[lumina_macros::command_group(description = "Voice channel commands")]
#[allow(clippy::module_inception)] // command_group names the slash command after the module
mod voice {
    use super::*;

    #[sub_command(description = "Join your voice channel and transcribe speech")]
    pub async fn transcribe(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
        let (guild_id, voice_channel) = join_user_channel(lx, cmd).await
            .map_err(|e| anyhow::anyhow!(e))?;

        let voice_mgr = get_voice_manager(lx).await?;
        // Post transcripts to the voice channel's text chat
        let text_channel = voice_channel;

        let manager = songbird::get(&lx.ctx).await
            .ok_or_else(|| anyhow::anyhow!("Songbird not initialized"))?;
        let call = manager.get(guild_id)
            .ok_or_else(|| anyhow::anyhow!("Not in voice channel"))?;

        voice_mgr.start_session(
            guild_id, voice_channel, text_channel,
            VoiceMode::Transcribe, None, None,
            call, Arc::clone(&lx.http),
        ).await?;

        lx.reply(cmd, "Joined voice. Transcribing to the voice channel...").await
    }

    #[sub_command(description = "Join a voice channel for a voice conversation")]
    pub async fn join(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
        let (guild_id, voice_channel) = resolve_voice_channel(lx, cmd)
            .map_err(|e| anyhow::anyhow!(e))?;

        let voice_mgr = get_voice_manager(lx).await?;
        let history = load_channel_seed(lx, cmd.channel_id).await;
        let base_ctx = lx.ctx_for(cmd.user.id.get()).await;

        let outcome = voice_mgr.join_voice(
            base_ctx, guild_id, voice_channel, voice_channel,
            history, Arc::clone(&lx.http),
        ).await?;

        let reply = match outcome {
            crate::voice::JoinOutcome::Joined => "Joined voice. Listening for conversation...",
            crate::voice::JoinOutcome::AlreadyJoined { .. } => "Already in a voice channel here.",
        };
        lx.reply_ephemeral(cmd, reply).await
    }

    #[sub_command(description = "Leave the voice channel")]
    pub async fn leave(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
        let guild_id = cmd.guild_id.ok_or_else(|| anyhow::anyhow!("Not in a guild"))?;
        let voice_mgr = get_voice_manager(lx).await?;
        let left = voice_mgr.leave_voice(guild_id).await?;
        lx.reply_ephemeral(cmd, if left { "Left voice channel." } else { "Not in a voice channel." }).await
    }

    #[sub_command(description = "Set STT or TTS provider")]
    pub async fn provider(
        lx: &LuminaContext,
        cmd: &CommandInteraction,
        #[describe("stt or tts")] kind: String,
        #[describe("Provider ID")] #[autocomplete] id: String,
    ) -> anyhow::Result<()> {
        let voice_mgr = get_voice_manager(lx).await?;
        match kind.as_str() {
            "stt" => {
                voice_mgr.set_stt_provider(id.clone()).await;
                save_voice_config(lx, &voice_mgr).await;
                lx.reply_ephemeral(cmd, &format!("STT provider set to **{id}**")).await
            }
            "tts" => {
                voice_mgr.set_tts_provider(id.clone()).await;
                save_voice_config(lx, &voice_mgr).await;
                lx.reply(cmd, &format!("TTS provider set to **{id}**")).await
            }
            _ => lx.reply_ephemeral(cmd, "Type must be 'stt' or 'tts'").await,
        }
    }

    #[sub_command(description = "Set the TTS voice")]
    pub async fn set_voice(
        lx: &LuminaContext,
        cmd: &CommandInteraction,
        #[describe("Voice ID")] #[autocomplete] id: String,
    ) -> anyhow::Result<()> {
        let voice_mgr = get_voice_manager(lx).await?;
        voice_mgr.set_tts_voice(id.clone()).await;
        save_voice_config(lx, &voice_mgr).await;
        lx.reply_ephemeral(cmd, &format!("TTS voice set to **{id}**")).await
    }

    #[sub_command(description = "List available providers and voices")]
    pub async fn list(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
        let voice_mgr = get_voice_manager(lx).await?;
        let providers = voice_mgr.daemon().voice().list_voice_providers().await?;

        let mut lines = vec!["**Providers:**".to_string()];
        for p in &providers {
            lines.push(format!("  `{}` — {} ({})", p.id, p.name, p.capabilities.join(", ")));
        }

        for p in providers.iter().filter(|p| p.capabilities.contains(&"tts".to_string())) {
            if let Ok(voices) = voice_mgr.daemon().voice().list_voices(&p.id).await {
                if !voices.is_empty() {
                    lines.push(format!("\n**Voices for {}:**", p.id));
                    for v in voices.iter().take(15) {
                        lines.push(format!("  `{}` — {}", v.id, v.name));
                    }
                    if voices.len() > 15 {
                        lines.push(format!("  ... and {} more", voices.len() - 15));
                    }
                }
            }
        }

        lx.reply(cmd, &lines.join("\n")).await
    }

    #[sub_command(description = "Show current voice settings")]
    pub async fn status(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
        tracing::info!("voice status: getting voice manager");
        let voice_mgr = get_voice_manager(lx).await?;
        tracing::info!("voice status: got voice manager");
        let stt = voice_mgr.stt_provider_id().await.unwrap_or_else(|| "(auto)".to_string());
        let tts = voice_mgr.tts_provider_id().await.unwrap_or_else(|| "(none)".to_string());
        let voice = voice_mgr.tts_voice_id().await.unwrap_or_else(|| "(auto)".to_string());

        let guild_id = cmd.guild_id.unwrap_or_default();
        let mode = voice_mgr.get_mode(&guild_id).await;
        let mode_str = match mode {
            Some(VoiceMode::Transcribe) => "Transcribing",
            Some(VoiceMode::Listen) => "Listening",
            None => "Not active",
        };

        lx.reply(cmd, &format!(
            "**Voice Settings**\n\
             STT Provider: `{stt}`\n\
             TTS Provider: `{tts}`\n\
             TTS Voice: `{voice}`\n\
             Session: {mode_str}"
        )).await
    }

    /// Autocomplete handler — called by the macro-generated `autocomplete` method.
    pub async fn autocomplete(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
        let voice_mgr = get_voice_manager(lx).await?;
        let options = cmd.data.options();
        let subcommand = options.first().map(|o| o.name).unwrap_or("");
        tracing::debug!(subcommand, "voice autocomplete");

        let choices: Vec<AutocompleteChoice> = match subcommand {
            "provider" => {
                // Once `kind` (stt/tts) is filled in, offer only providers with
                // that capability; before that, offer all of them.
                let kind = options.first()
                    .and_then(|sub| sub_string_arg(&sub.value, "kind"))
                    .map(|k| k.trim().to_ascii_lowercase())
                    .filter(|k| !k.is_empty());
                let providers = voice_mgr.daemon().voice().list_voice_providers().await.unwrap_or_default();
                providers.iter()
                    .filter(|p| kind.as_ref().is_none_or(|k| p.capabilities.contains(k)))
                    .map(|p| AutocompleteChoice::new(
                        format!("{} ({})", p.name, p.capabilities.join(", ")),
                        p.id.clone(),
                    ))
                    .collect()
            }
            "set-voice" => {
                let tts_id = voice_mgr.tts_provider_id().await.unwrap_or_default();
                tracing::debug!(tts_provider = %tts_id, "fetching voices for autocomplete");
                if tts_id.is_empty() {
                    vec![AutocompleteChoice::new("Set a TTS provider first (/voice provider tts ...)", "")]
                } else {
                    let voices = voice_mgr.daemon().voice().list_voices(&tts_id).await.unwrap_or_default();
                    tracing::debug!(count = voices.len(), "got voices for autocomplete");
                    voices.iter()
                        .map(|v| AutocompleteChoice::new(&v.name, v.id.clone()))
                        .take(25)
                        .collect()
                }
            }
            _ => vec![],
        };

        cmd.create_response(&lx.http, CreateInteractionResponse::Autocomplete(
            CreateAutocompleteResponse::new().set_choices(choices),
        )).await?;
        Ok(())
    }
}

// --- Helpers (outside the module, used by subcommands via super::) ---

async fn save_voice_config(lx: &LuminaContext, voice_mgr: &crate::voice::VoiceManager) {
    let mut cfg = lx.config.clone();
    if let Err(e) = voice_mgr.save_config(&mut cfg).await {
        tracing::warn!(error = %e, "failed to save voice config to lumina.toml");
    }
}

pub(super) async fn get_voice_manager(lx: &LuminaContext) -> anyhow::Result<Arc<crate::voice::VoiceManager>> {
    tracing::debug!("get_voice_manager: acquiring data read lock");
    let data = lx.ctx.data.read().await;
    tracing::debug!("get_voice_manager: got data lock");
    let mgr = data.get::<VoiceManagerKey>()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("VoiceManager not initialized"));
    drop(data);
    tracing::debug!("get_voice_manager: released data lock");
    mgr
}

/// Determine which voice channel `/voice` should act on without connecting to it.
///
/// Prefers the channel the invoking user is currently in; falls back to the
/// first voice channel in the guild (lowest position) so slash commands work
/// even when invoked from a text channel.
fn resolve_voice_channel(
    lx: &LuminaContext,
    cmd: &CommandInteraction,
) -> Result<(serenity::model::id::GuildId, serenity::model::id::ChannelId), String> {
    let guild_id = cmd.guild_id.ok_or("Not in a guild")?;
    let guild = lx.cache.guild(guild_id).ok_or("Guild not found")?;
    let voice_channel = guild.voice_states.get(&cmd.user.id)
        .and_then(|vs| vs.channel_id)
        .or_else(|| {
            let mut voice_channels: Vec<_> = guild.channels.values()
                .filter(|ch| ch.kind == serenity::model::channel::ChannelType::Voice)
                .collect();
            voice_channels.sort_by_key(|ch| ch.position);
            voice_channels.first().map(|ch| ch.id)
        })
        .ok_or("No voice channels in this guild")?;
    Ok((guild_id, voice_channel))
}

/// Resolve the target voice channel and connect songbird to it. Used by
/// `/voice transcribe` and `/voice say` where the voice_mgr doesn't own
/// session creation.
async fn join_user_channel(
    lx: &LuminaContext,
    cmd: &CommandInteraction,
) -> Result<(serenity::model::id::GuildId, serenity::model::id::ChannelId), String> {
    let (guild_id, voice_channel) = resolve_voice_channel(lx, cmd)?;
    let manager = songbird::get(&lx.ctx).await.ok_or("Songbird not initialized")?;
    manager.join(guild_id, voice_channel).await
        .map_err(|e| format!("Failed to join voice: {e}"))?;
    tracing::info!(guild_id = %guild_id, voice_channel = %voice_channel, "joined voice channel");
    Ok((guild_id, voice_channel))
}

async fn load_channel_seed(lx: &LuminaContext, channel_id: ChannelId) -> Vec<SeedMessage> {
    let bot_id = lx.cache.current_user().id.get();
    let messages = channel_id
        .messages(&lx.http, GetMessages::new().limit(50))
        .await
        .unwrap_or_default();

    messages.iter().rev()
        .filter(|m| !m.content.is_empty())
        .map(|m| {
            let role = if m.author.id.get() == bot_id { Role::Assistant } else { Role::User };
            let text = if role == Role::User {
                format!("<@{}> says: {}", m.author.id, m.content)
            } else {
                m.content.clone()
            };
            SeedMessage { role, content: vec![InputContent::Text { text }] }
        })
        .collect()
}


