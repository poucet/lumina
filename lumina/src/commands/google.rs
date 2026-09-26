//! /google — Google Docs auth and import.
//!
//! Subcommands:
//! - `/google auth` — generate OAuth link for Google Docs
//! - `/google import <doc>` — import a Google Doc (autocomplete from Drive)
//! - `/google status` — check if Google is connected

use async_trait::async_trait;
use serenity::all::{
    AutocompleteChoice, CommandInteraction, CommandOptionType,
    CreateAutocompleteResponse, CreateInteractionResponse, CreateInteractionResponseMessage,
    ResolvedOption, ResolvedValue,
};
use serenity::builder::{CreateCommand, CreateCommandOption, CreateEmbed};

use super::LuminaContext;
use crate::register_command;

const GOOGLE_PROVIDER_ID: &str = "google";

#[derive(Default)]
pub struct Google;

register_command!(Google);

#[async_trait]
impl super::SlashCommand for Google {
    fn name(&self) -> &'static str { "google" }

    fn register(&self) -> CreateCommand {
        CreateCommand::new("google")
            .description("Google Docs integration")
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "auth", "Connect your Google account"),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "import", "Import a Google Doc")
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::String, "doc", "Google Doc to import (name or URL)")
                            .required(true)
                            .set_autocomplete(true),
                    ),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "status", "Check Google connection status"),
            )
    }

    async fn run(&self, lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
        let opts = cmd.data.options();
        let sub = opts.first().ok_or_else(|| anyhow::anyhow!("missing subcommand"))?;

        match sub.name {
            "auth" => cmd_auth(lx, cmd).await,
            "import" => {
                let doc_input = if let ResolvedValue::SubCommand(ref sub_opts) = sub.value {
                    sub_opts.iter().find_map(|o| match o {
                        ResolvedOption { name: "doc", value: ResolvedValue::String(s), .. } => Some(s.to_string()),
                        _ => None,
                    })
                } else { None }.ok_or_else(|| anyhow::anyhow!("missing doc argument"))?;
                cmd_import(lx, cmd, &doc_input).await
            }
            "status" => cmd_status(lx, cmd).await,
            other => Err(anyhow::anyhow!("unknown subcommand `{other}`")),
        }
    }

    async fn autocomplete(&self, lx: &LuminaContext, ac: &CommandInteraction) -> anyhow::Result<()> {
        // The focused autocomplete value. `data.autocomplete()` recurses into the
        // `import` subcommand and returns the `doc` option being typed — its value
        // arrives with kind Autocomplete, NOT a plain String, so the old code read
        // it as `ResolvedValue::String` and always got an empty string (→ nothing
        // ever filtered).
        let Some(focused) = ac.data.autocomplete() else { return Ok(()); };
        if focused.name != "doc" {
            return Ok(());
        }
        let partial = focused.value.to_string();

        tracing::debug!(partial = %partial, discord_id = ac.user.id.get(), "gdocs autocomplete: calling gdocs_list");

        // Filter server-side so it works across the WHOLE Drive (not just a
        // fetched page): gdocs_list's `query` does `name contains` on Drive.
        // Then keep a client-side name filter to drop any content-only matches
        // (the server query also matches body text) so the title picker is tight.
        let needle = partial.to_lowercase();
        let choices = match call_tool(lx, ac.user.id.get(), "gdocs_list",
            serde_json::json!({
                "query": if partial.is_empty() { None } else { Some(&partial) },
                "limit": 100,
            }),
        ).await {
            Ok(text) => {
                let docs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "failed to parse gdocs_list response");
                    Vec::new()
                });
                tracing::debug!(doc_count = docs.len(), needle = %needle, "gdocs autocomplete: filtering");
                docs.iter()
                    .filter_map(|d| Some((d.get("id")?.as_str()?, d.get("name")?.as_str()?)))
                    .filter(|(_, name)| needle.is_empty() || name.to_lowercase().contains(&needle))
                    .take(25)
                    .map(|(id, name)| {
                        // char-safe truncation to Discord's 100-char choice limit
                        let display = if name.chars().count() > 100 {
                            format!("{}…", name.chars().take(99).collect::<String>())
                        } else {
                            name.to_string()
                        };
                        AutocompleteChoice::new(display, id)
                    })
                    .collect()
            }
            Err(e) => {
                tracing::warn!(error = %e, "gdocs_list call failed");
                vec![AutocompleteChoice::new("(connect Google first: /google auth)", "")]
            }
        };

        tracing::debug!(choice_count = choices.len(), "gdocs autocomplete: responding");
        ac.create_response(&lx.http, CreateInteractionResponse::Autocomplete(
            CreateAutocompleteResponse::new().set_choices(choices),
        )).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

async fn cmd_auth(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
    let discord_id = cmd.user.id.get();
    let external_id = format!("discord:{discord_id}");

    // Don't create a simply user upfront — the OAuth callback will get-or-create
    // the user by email and link this discord external_id to that user. Email
    // is the canonical identity.
    let base_url = lx.daemon.core().public_url().await?;
    let auth_url = format!(
        "{base_url}/auth/mcp/{GOOGLE_PROVIDER_ID}?external_id={}",
        urlencoding::encode(&external_id),
    );

    cmd.create_response(&lx.http, CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(format!("Click to connect Google:\n{auth_url}"))
            .ephemeral(true),
    )).await?;
    Ok(())
}

async fn cmd_import(lx: &LuminaContext, cmd: &CommandInteraction, doc_input: &str) -> anyhow::Result<()> {
    let discord_id = cmd.user.id.get();
    let doc_id = extract_doc_id(doc_input);

    tracing::info!(discord_id, doc_id = %doc_id, "gdocs import: starting");
    lx.defer(cmd).await?;

    // Call gdocs_import skill tool — handles extraction, tab creation, image storage
    match call_tool(lx, discord_id, "gdocs_import", serde_json::json!({ "doc_id": doc_id })).await {
        Ok(text) => {
            tracing::info!(text_len = text.len(), "gdocs_import: ok");
            let embed = CreateEmbed::new()
                .title("Google Doc Imported")
                .description(text)
                .color(0x14b8a6);
            cmd.edit_response(&lx.http, serenity::builder::EditInteractionResponse::new().embed(embed)).await?;
        }
        Err(e) => {
            tracing::warn!(error = %e, "gdocs_import: failed");
            let msg = format!("{e}");
            if msg.contains("authenticate") || msg.contains("token") {
                cmd.edit_response(&lx.http,
                    serenity::builder::EditInteractionResponse::new()
                        .content("Not connected to Google. Run `/google auth` first."),
                ).await?;
            } else {
                cmd.edit_response(&lx.http,
                    serenity::builder::EditInteractionResponse::new()
                        .content(format!("Import failed: {e}")),
                ).await?;
            }
        }
    }
    Ok(())
}

async fn cmd_status(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
    let servers = lx.daemon.mcp().list_mcp_servers().await?;
    let google = servers.iter().find(|s| s.id == GOOGLE_PROVIDER_ID);

    let (status_text, color) = match google {
        Some(s) if s.is_connected => ("Connected", 0x14b8a6u32),
        Some(_) => ("Server configured but not connected", 0xf59e0bu32),
        None => ("Google Docs MCP server not configured", 0xef4444u32),
    };

    cmd.create_response(&lx.http, CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .embed(CreateEmbed::new().title("Google Docs Status").description(status_text).color(color))
            .ephemeral(true),
    )).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Call a daemon tool and return the text result.
async fn call_tool(lx: &LuminaContext, discord_user_id: u64, tool_name: &str, args: serde_json::Value) -> anyhow::Result<String> {
    let ctx = lx.ctx_for(discord_user_id).await;
    tracing::debug!(
        tool = tool_name,
        discord_user_id,
        resolved_user_id = ?ctx.scope.user_id,
        "call_tool: dispatching to daemon"
    );
    let request = simply_daemon_api::CallToolRequestParams::new(tool_name.to_string())
        .with_arguments(args.as_object().cloned().unwrap_or_default());
    let result = match lx.daemon.mcp().call_tool_direct(&ctx, request).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(tool = tool_name, error = %e, "call_tool: daemon returned error");
            return Err(e);
        }
    };

    if result.is_error.unwrap_or(false) {
        let text = result.content.iter().find_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.to_string()),
            _ => None,
        }).unwrap_or_default();
        tracing::warn!(tool = tool_name, error_text = %text, "call_tool: tool reported error");
        anyhow::bail!(text);
    }

    let text = result.content.iter().find_map(|c| match &c.raw {
        rmcp::model::RawContent::Text(t) => Some(t.text.to_string()),
        _ => None,
    }).unwrap_or_default();
    tracing::debug!(tool = tool_name, text_len = text.len(), "call_tool: ok");
    Ok(text)
}

fn extract_doc_id(input: &str) -> String {
    if input.contains("docs.google.com") {
        if let Some(start) = input.find("/d/") {
            let id_start = start + 3;
            let id_end = input[id_start..].find('/').map(|i| id_start + i).unwrap_or(input.len());
            return input[id_start..id_end].to_string();
        }
    }
    input.to_string()
}
