//! Lumina — Discord bot for the Simply platform.
//!
//! Connects to Discord via serenity and to simply-daemon for AI capabilities.

mod chat;
mod commands;
pub mod invite;
pub mod json_fmt;
pub mod mcp;
pub mod paginator;
pub mod tool_render;
pub mod tool_state;
pub mod voice;

use std::sync::Arc;

use commands::CommandRegistry;
use serenity::model::id::{ChannelId, GuildId};
use serenity::prelude::*;
use songbird::SerenityInit;
use songbird::Config as SongbirdConfig;
use songbird::driver::{DecodeMode, DecodeConfig, Channels, SampleRate};
use simply_daemon_api::{Daemon, Skill};

pub struct McpServerKey;
impl TypeMapKey for McpServerKey { type Value = mcp::DiscordSkill; }

pub struct DaemonKey;
impl TypeMapKey for DaemonKey { type Value = Arc<dyn Daemon>; }

pub struct ConfigKey;
impl TypeMapKey for ConfigKey { type Value = config::LuminaConfig; }

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    setup_logging();

    config::load_env_file();
    let lumina_cfg = config::LuminaConfig::load();
    let settings = config::Settings::load();

    // Connect to daemon — embedded (host or connect) or remote-only. Done
    // FIRST so the first-run setup wizard at /setup is reachable even before a
    // Discord token exists.
    let daemon = connect_daemon(&settings).await?;

    // Only connect to Discord when FULLY set up: we need both the owner email
    // (settings.toml) and a bot token (lumina.toml). Otherwise stay up in setup
    // mode so the daemon keeps serving /setup. This also avoids trying a stale
    // token after `reset-setup.sh` clears the owner email but leaves the token.
    let configured = settings.user_email.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
    let token = match (configured, lumina_cfg.bot_token()) {
        (true, Some(t)) => t.to_string(),
        _ => {
            tracing::warn!(
                "Not fully configured yet — running in setup mode. Finish setup in your \
                 browser (see SETUP-URL.txt or the log line above)."
            );
            std::future::pending::<()>().await;
            return Ok(());
        }
    };

    // Open the Lumina-owned tool-state DB (Discord message id →
    // structured ToolCall/ToolResult JSON). Separate file under
    // lumina_data_dir so nuking replay state doesn't touch noema.db.
    let _ = config::PathManager::ensure_dirs_exist();
    let tool_state_path = config::PathManager::lumina_tool_state_path()
        .ok_or_else(|| anyhow::anyhow!("could not determine lumina_tool_state_path"))?;
    let tool_state = Arc::new(tool_state::ToolStateStore::open(&tool_state_path)?);
    tracing::info!(path = %tool_state_path.display(), "lumina tool_state ready");

    // Register skills with daemon (works over WS for both embedded and remote)
    let http = Arc::new(serenity::http::Http::new(&token));
    let discord_skill = mcp::DiscordSkill::new(http);
    let voice_mgr = Arc::new(voice::VoiceManager::new(daemon.clone(), lumina_cfg.voice.clone()));
    discord_skill.set_voice_manager(Arc::clone(&voice_mgr));
    register_skills(&daemon, &discord_skill).await?;

    // Discord client
    let registry = CommandRegistry::collect();
    tracing::info!(count = registry.len(), "commands registered");

    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::MESSAGE_CONTENT;

    // Pre-create songbird so we can inject it into VoiceManager for tool-driven leaves.
    let songbird = songbird::Songbird::serenity_from_config(
        SongbirdConfig::default().decode_mode(DecodeMode::Decode(
            DecodeConfig::new(Channels::Mono, SampleRate::Hz16000)
        ))
    );
    voice_mgr.set_songbird(Arc::clone(&songbird));

    let mut client = Client::builder(token, intents)
        .event_handler(Handler {
            guild_ids: lumina_cfg.discord.guild_ids.iter().map(|&id| GuildId::new(id)).collect(),
            status_channel_id: lumina_cfg.discord.status_channel_id.map(ChannelId::new),
        })
        .register_songbird_with(songbird)
        .type_map_insert::<DaemonKey>(daemon)
        .type_map_insert::<voice::VoiceManagerKey>(voice_mgr)
        .type_map_insert::<ConfigKey>(lumina_cfg)
        .type_map_insert::<CommandRegistry>(registry)
        .type_map_insert::<commands::SharedState>(Arc::new(commands::SharedState::new(tool_state)))
        .type_map_insert::<McpServerKey>(discord_skill)
        .await?;

    tracing::info!("lumina starting");
    // Don't let a Discord failure (e.g. a reset/invalid bot token → 401) crash
    // the process: that would also take down the embedded daemon and its setup
    // wizard, creating a crash loop you can't fix. Log and stay up instead; the
    // operator fixes the token via setup and restarts to reconnect.
    if let Err(e) = client.start().await {
        tracing::error!(
            error = %e,
            "Discord connection failed — keeping the daemon (and setup wizard) up. \
             Fix the bot token via setup, then restart to reconnect."
        );
        std::future::pending::<()>().await;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon connection
// ---------------------------------------------------------------------------

/// Connect to daemon — embedded mode hosts/connects, remote mode only connects.
async fn connect_daemon(settings: &config::Settings) -> anyhow::Result<Arc<dyn Daemon>> {
    #[cfg(feature = "embedded")]
    {
        // _handle is kept alive to prevent the daemon from shutting down
        static HANDLE: tokio::sync::OnceCell<simply_daemon::net::DaemonHandle> = tokio::sync::OnceCell::const_new();
        let handle = simply_daemon::net::connect_or_host(settings.daemon_port, "lumina", None).await?;
        tracing::info!(host = handle.is_host(), "daemon ready");
        let daemon = handle.daemon();
        let _ = HANDLE.set(handle);
        Ok(daemon)
    }
    #[cfg(not(feature = "embedded"))]
    {
        let port = settings.daemon_port.unwrap_or(config::DEFAULT_DAEMON_PORT);
        let secret = settings.daemon_secret.clone().unwrap_or_default();
        let url = format!("127.0.0.1:{port}");
        tracing::info!(%url, "connecting to remote daemon");
        let remote = simply_daemon_api::RemoteDaemon::connect_as(&url, "lumina", &secret, None).await?;
        Ok(remote)
    }
}

// ---------------------------------------------------------------------------
// Skills — registered with daemon over Skill trait (in-process or WS reverse-RPC)
// ---------------------------------------------------------------------------

async fn register_skills(daemon: &Arc<dyn Daemon>, discord: &mcp::DiscordSkill) -> anyhow::Result<()> {
    let gdocs = Arc::new(mcp_gdocs::GDocsSkill::new(Arc::clone(daemon)));
    daemon.register_skill(gdocs).await?;
    tracing::info!("GDocs skill registered with daemon");

    daemon.register_skill(Arc::new(discord.clone())).await?;
    tracing::info!("Discord skill registered with daemon");

    Ok(())
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

fn setup_logging() {
    use tracing_subscriber::prelude::*;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            "lumina=debug,simply_daemon=info,simply_core=debug,mcp_gdocs=debug".into()
        });

    // Always log to stdout (captured by `docker compose logs`). Additionally,
    // when a data dir is available, mirror logs to <data>/logs/lumina.log
    // (daily-rolled) so they're visible on the host bind mount.
    let file_layer = config::PathManager::logs_dir().and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        let (writer, guard) =
            tracing_appender::non_blocking(tracing_appender::rolling::daily(&dir, "lumina.log"));
        // The worker guard must outlive the program or buffered logs are lost.
        static GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> =
            std::sync::OnceLock::new();
        let _ = GUARD.set(guard);
        Some(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(writer))
    });

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .with(file_layer)
        .init();
}

// ---------------------------------------------------------------------------
// Discord event handler
// ---------------------------------------------------------------------------

struct Handler {
    guild_ids: Vec<GuildId>,
    status_channel_id: Option<ChannelId>,
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: serenity::model::gateway::Ready) {
        tracing::info!(user = %ready.user.name, "connected to Discord — use .sync to register commands");

        // Log the invite URL on every boot so the operator can easily
        // add Lumina to another server without digging through the
        // developer portal. Mirrors what `/invite` serves to users.
        match ctx.http.get_current_application_info().await {
            Ok(info) => {
                let url = invite::invite_url(info.id.get());
                tracing::info!(invite_url = %url, "bot invite URL (share with server admins to add Lumina)");
            }
            Err(e) => tracing::warn!(error = %e, "could not fetch application info for invite URL"),
        }

        {
            let data = ctx.data.read().await;
            if let Some(mcp) = data.get::<McpServerKey>() {
                mcp.set_cache(ctx.cache.clone());
                tracing::info!("injected gateway cache into lumina MCP server");
            }
            if let Some(voice_mgr) = data.get::<voice::VoiceManagerKey>() {
                voice_mgr.set_shard(ctx.shard.clone());
            }
        }

        if let Some(channel_id) = self.status_channel_id {
            if let Ok(messages) = channel_id.messages(&ctx.http, serenity::builder::GetMessages::new().limit(1)).await {
                for msg in messages { let _ = msg.delete(&ctx.http).await; }
            }
            let _ = channel_id.say(&ctx.http, format!("\u{1f7e2} Bot connected as <@{}>", ready.user.id)).await;
        }
    }

    async fn cache_ready(&self, ctx: Context, _guilds: Vec<GuildId>) {
        let data = ctx.data.read().await;
        if let Some(mcp) = data.get::<McpServerKey>() {
            mcp.refresh_instructions();
            tracing::info!("refreshed MCP instructions with channel map");
        }
    }

    async fn channel_create(&self, ctx: Context, _channel: serenity::model::channel::GuildChannel) {
        let data = ctx.data.read().await;
        if let Some(mcp) = data.get::<McpServerKey>() { mcp.refresh_instructions(); }
    }

    async fn channel_delete(&self, ctx: Context, _channel: serenity::model::channel::GuildChannel, _messages: Option<Vec<serenity::model::channel::Message>>) {
        let data = ctx.data.read().await;
        if let Some(mcp) = data.get::<McpServerKey>() { mcp.refresh_instructions(); }
    }

    async fn channel_update(&self, ctx: Context, _old: Option<serenity::model::channel::GuildChannel>, _new: serenity::model::channel::GuildChannel) {
        let data = ctx.data.read().await;
        if let Some(mcp) = data.get::<McpServerKey>() { mcp.refresh_instructions(); }
    }

    /// Mirror the bot's real voice presence into the VoiceManager session map.
    ///
    /// - Bot disconnected (by admin, kick, or any non-/voice path) → end session.
    /// - Bot pulled into a voice channel (admin drag, `Invite to voice`) → start
    ///   a Listen session in that channel automatically.
    /// - Bot moved between channels → end the old session, start one in the new channel.
    ///
    /// A short debounce lets our own `/voice join` flow reach the sessions map
    /// before this handler observes the bot's state change and tries to react.
    async fn voice_state_update(
        &self,
        ctx: Context,
        old: Option<serenity::model::voice::VoiceState>,
        new: serenity::model::voice::VoiceState,
    ) {
        let bot_id = ctx.cache.current_user().id;
        if new.user_id != bot_id { return; }
        let Some(guild_id) = new.guild_id else { return; };

        let old_channel = old.and_then(|s| s.channel_id);
        let new_channel = new.channel_id;
        if old_channel == new_channel { return; }

        // Debounce: /voice join races this event (songbird dispatches the voice
        // state update before our command handler finishes inserting the
        // session). 500ms is enough slack for the session insert to land.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let data = ctx.data.read().await;
        let Some(voice_mgr) = data.get::<voice::VoiceManagerKey>().cloned() else { return };
        drop(data);

        match (old_channel, new_channel) {
            (_, None) => {
                if voice_mgr.has_session(&guild_id).await {
                    tracing::info!(guild_id = %guild_id, "bot disconnected from voice; ending session");
                    voice_mgr.stop_session(&guild_id).await;
                }
            }
            (Some(old_ch), Some(new_ch)) if old_ch != new_ch => {
                tracing::info!(guild_id = %guild_id, from = %old_ch, to = %new_ch, "bot moved voice channels; restarting session");
                voice_mgr.stop_session(&guild_id).await;
                let base_ctx = simply_rpc::RequestContext::anonymous();
                if let Err(e) = voice_mgr.join_voice(
                    base_ctx, guild_id, new_ch, new_ch, Vec::new(), Arc::clone(&ctx.http),
                ).await {
                    tracing::warn!(guild_id = %guild_id, error = %e, "auto-join after move failed");
                }
            }
            (None, Some(ch)) => {
                if !voice_mgr.has_session(&guild_id).await {
                    tracing::info!(guild_id = %guild_id, voice_channel = %ch, "bot pulled into voice externally; starting session");
                    let base_ctx = simply_rpc::RequestContext::anonymous();
                    if let Err(e) = voice_mgr.join_voice(
                        base_ctx, guild_id, ch, ch, Vec::new(), Arc::clone(&ctx.http),
                    ).await {
                        tracing::warn!(guild_id = %guild_id, error = %e, "auto-join failed");
                    }
                }
            }
            _ => {}
        }
    }

    async fn message(&self, ctx: Context, msg: serenity::model::channel::Message) {
        if msg.author.bot { return; }

        if msg.content.starts_with('.') {
            let data = ctx.data.read().await;
            let cfg = data.get::<ConfigKey>().expect("ConfigKey missing");
            let is_owner = cfg.discord.owner_id.map_or(false, |id| msg.author.id.get() == id);
            if is_owner {
                match msg.content.as_str() {
                    ".sync" => {
                        let registry = data.get::<CommandRegistry>().expect("CommandRegistry missing");
                        let definitions = registry.definitions();
                        drop(data);
                        let mut ok = 0usize;
                        let mut fail = 0usize;
                        for &guild_id in &self.guild_ids {
                            match guild_id.set_commands(&ctx.http, definitions.clone()).await {
                                Ok(_) => ok += 1,
                                Err(e) => { tracing::error!(guild_id = %guild_id, error = %e, "sync failed"); fail += 1; }
                            }
                        }
                        let _ = msg.reply(&ctx.http, format!("Synced commands to {ok} guild(s), {fail} failed.")).await;
                    }
                    _ => {}
                }
                return;
            }
        }

        let lx = commands::LuminaContext::from_serenity(&ctx).await;
        chat::handle_message(&lx, &msg).await;
    }

    async fn interaction_create(&self, ctx: Context, interaction: serenity::model::application::Interaction) {
        match interaction {
            serenity::model::application::Interaction::Command(cmd) => {
                let lx = commands::LuminaContext::from_serenity(&ctx).await;
                let data = ctx.data.read().await;
                let registry = data.get::<CommandRegistry>().expect("CommandRegistry missing");
                registry.dispatch(&lx, &cmd).await;
            }
            serenity::model::application::Interaction::Autocomplete(ac) => {
                let lx = commands::LuminaContext::from_serenity(&ctx).await;
                let data = ctx.data.read().await;
                let registry = data.get::<CommandRegistry>().expect("CommandRegistry missing");
                registry.dispatch_autocomplete(&lx, &ac).await;
            }
            serenity::model::application::Interaction::Modal(modal) => {
                if modal.data.custom_id.starts_with("tool_call:") {
                    let lx = commands::LuminaContext::from_serenity(&ctx).await;
                    if let Err(e) = commands::tool::handle_modal(&lx, &modal).await {
                        tracing::error!(error = %e, "tool modal failed");
                    }
                }
            }
            serenity::model::application::Interaction::Component(comp) => {
                // Only our own buttons are handled here; the paginator's
                // prev/next buttons are consumed by its local collector
                // and never reach this dispatcher.
                if comp.data.custom_id == "tool_json" {
                    let lx = commands::LuminaContext::from_serenity(&ctx).await;
                    if let Err(e) = chat::handle_tool_json_button(&lx, &comp).await {
                        tracing::error!(error = %e, "tool_json button failed");
                    }
                }
            }
            _ => {}
        }
    }
}
