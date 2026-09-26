//! Slash command registration and dispatch.
//!
//! Use `#[slash_command]` proc macro for stateless commands (auto-registers).
//! Use `register_command!` for stateful commands that need custom constructors.
//! No manual dispatch table — `inventory` collects them at link time.

pub mod chat;
mod config;
mod debug;
mod google;
mod invite;
mod model;
mod ping;
pub mod tool;
pub mod voice;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serenity::all::{CommandInteraction, ResolvedOption, ResolvedValue};
use serenity::builder::CreateCommand;
use serenity::prelude::*;
use simply_daemon_api::Daemon;
use simply_rpc::RequestContext;

/// Extract a String argument from a SubCommand's options by name.
pub(crate) fn sub_string_arg<'a>(sub_value: &'a ResolvedValue<'a>, name: &str) -> Option<&'a str> {
    let ResolvedValue::SubCommand(sub_opts) = sub_value else { return None };
    sub_opts.iter().find_map(|o| match o {
        ResolvedOption { name: n, value: ResolvedValue::String(s), .. } if *n == name => Some(*s),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Shared state across all handlers.
///
/// User identity is resolved from the daemon on every request — no local
/// cache, because the discord-id → simply-user mapping can change (e.g. after
/// `/google auth` links the discord id to an email-based user).
///
/// `tool_state` is a Lumina-owned SQLite cache mapping Discord message ids
/// to the `ToolCall` / `ToolResult` JSON we would otherwise attach to the
/// message. Keeps the channel UI clean while preserving structured replay.
pub struct SharedState {
    pub tool_state: Arc<crate::tool_state::ToolStateStore>,
}

impl SharedState {
    pub fn new(tool_state: Arc<crate::tool_state::ToolStateStore>) -> Self {
        Self { tool_state }
    }
}

impl TypeMapKey for SharedState {
    type Value = Arc<SharedState>;
}

// ---------------------------------------------------------------------------
// LuminaContext — passed to every command handler
// ---------------------------------------------------------------------------

/// Rich context for command handlers. Bundles serenity context, daemon, config, and shared state.
pub struct LuminaContext {
    pub ctx: Context,
    pub daemon: Arc<dyn Daemon>,
    // `::config` (leading colon) = the external `config` crate, not the sibling
    // `commands::config` module (the /config command), which would shadow it.
    pub config: ::config::LuminaConfig,
    pub state: Arc<SharedState>,
}

impl LuminaContext {
    /// Get a RequestContext for a Discord user.
    ///
    /// Always resolves via the daemon — no local cache. The mapping from a
    /// Discord identity to a simply user can change when the user completes
    /// OAuth (e.g. `/google auth` links `discord:123` to their email-based
    /// user), so caching would serve stale identities. Returns anonymous if
    /// no mapping exists — never creates a user.
    pub async fn ctx_for(&self, discord_user_id: u64) -> RequestContext {
        let external_id = format!("discord:{discord_user_id}");
        let anon = RequestContext::anonymous();
        match self.daemon.user().resolve_user(&anon, external_id).await {
            Ok(Some(scope)) => RequestContext::with_scope(scope),
            _ => RequestContext::anonymous(),
        }
    }

    /// Build from serenity Context by extracting TypeMap values.
    pub async fn from_serenity(ctx: &Context) -> Self {
        let data = ctx.data.read().await;
        let daemon = data.get::<crate::DaemonKey>().expect("DaemonKey missing").clone();
        let config = data.get::<crate::ConfigKey>().expect("ConfigKey missing").clone();
        let state = data.get::<SharedState>().expect("SharedState missing").clone();
        Self {
            ctx: ctx.clone(),
            daemon,
            config,
            state,
        }
    }
}

impl LuminaContext {
    /// Reply to a command interaction with a visible message.
    pub async fn reply(&self, cmd: &CommandInteraction, msg: &str) -> anyhow::Result<()> {
        use serenity::all::{CreateInteractionResponse, CreateInteractionResponseMessage};
        cmd.create_response(&self.http, CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new().content(msg),
        )).await?;
        Ok(())
    }

    /// Reply with an ephemeral message (only visible to the command user).
    pub async fn reply_ephemeral(&self, cmd: &CommandInteraction, msg: &str) -> anyhow::Result<()> {
        use serenity::all::{CreateInteractionResponse, CreateInteractionResponseMessage};
        cmd.create_response(&self.http, CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new().content(msg).ephemeral(true),
        )).await?;
        Ok(())
    }

    /// Acknowledge the interaction (defer) so Discord doesn't timeout.
    /// Follow up with `cmd.edit_response()` to send the actual response.
    pub async fn defer(&self, cmd: &CommandInteraction) -> anyhow::Result<()> {
        use serenity::all::CreateInteractionResponse;
        cmd.create_response(&self.http, CreateInteractionResponse::Defer(
            serenity::all::CreateInteractionResponseMessage::new(),
        )).await?;
        Ok(())
    }
}

impl std::ops::Deref for LuminaContext {
    type Target = Context;
    fn deref(&self) -> &Self::Target {
        &self.ctx
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// A slash command that can optionally hold state across invocations.
#[async_trait]
pub trait SlashCommand: Send + Sync {
    /// The command name (must match what `register()` returns).
    fn name(&self) -> &'static str;

    /// Build the Discord command definition.
    fn register(&self) -> CreateCommand;

    /// Handle an incoming invocation.
    async fn run(&self, lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()>;

    /// Handle autocomplete for a command option. Default: no-op.
    async fn autocomplete(&self, _lx: &LuminaContext, _ac: &CommandInteraction) -> anyhow::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Auto-registration via inventory
// ---------------------------------------------------------------------------

/// Submitted to `inventory` — a factory that produces a boxed command.
pub struct CommandInit {
    pub init: fn() -> Box<dyn SlashCommand>,
}

inventory::collect!(CommandInit);

/// Register a command type for auto-discovery.
///
/// ```ignore
/// // Stateless (Default)
/// register_command!(Ping);
///
/// // Stateful (custom constructor)
/// register_command!(Voice { connections: DashMap::new() });
/// ```
#[macro_export]
macro_rules! register_command {
    ($ty:ident) => {
        ::inventory::submit!($crate::commands::CommandInit {
            init: || Box::new($ty::default()),
        });
    };
    ($ty:ident { $($fields:tt)* }) => {
        ::inventory::submit!($crate::commands::CommandInit {
            init: || Box::new($ty { $($fields)* }),
        });
    };
}

// ---------------------------------------------------------------------------
// Registry — lives in serenity's TypeMap
// ---------------------------------------------------------------------------

/// Holds all instantiated commands, keyed by name.
pub struct CommandRegistry {
    commands: HashMap<&'static str, Box<dyn SlashCommand>>,
}

impl CommandRegistry {
    /// Build the registry from all `inventory`-registered commands.
    pub fn collect() -> Self {
        let commands = inventory::iter::<CommandInit>
            .into_iter()
            .map(|ci| {
                let cmd = (ci.init)();
                (cmd.name(), cmd)
            })
            .collect();
        Self { commands }
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Return Discord command definitions for guild registration.
    pub fn definitions(&self) -> Vec<CreateCommand> {
        self.commands.values().map(|c| c.register()).collect()
    }

    /// Dispatch an incoming command interaction.
    pub async fn dispatch(&self, lx: &LuminaContext, cmd: &CommandInteraction) {
        let name = cmd.data.name.as_str();
        match self.commands.get(name) {
            Some(handler) => {
                if let Err(e) = handler.run(lx, cmd).await {
                    tracing::error!(command = name, error = %e, "command failed");
                }
            }
            None => tracing::warn!(command = name, "unknown command"),
        }
    }

    /// Dispatch an autocomplete interaction.
    pub async fn dispatch_autocomplete(&self, lx: &LuminaContext, ac: &CommandInteraction) {
        let name = ac.data.name.as_str();
        if let Some(handler) = self.commands.get(name) {
            if let Err(e) = handler.autocomplete(lx, ac).await {
                tracing::error!(command = name, error = %e, "autocomplete failed");
            }
        }
    }
}

impl TypeMapKey for CommandRegistry {
    type Value = CommandRegistry;
}
