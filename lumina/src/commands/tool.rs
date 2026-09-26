//! /tool — unified tool browsing, calling, and source management.
//!
//! This absorbs what used to be separate `/skills` and `/mcp` commands —
//! users think in "tools", not in providers/servers/skills, so the surface
//! area is one command with all the tool-related subcommands:
//!
//! - `/tool list`               — concise one-line-per-tool listing
//! - `/tool info <name>`        — full details for one tool (description, params, schema)
//! - `/tool call <name>`        — autocomplete + modal form → execute tool
//! - `/tool sources`            — list tool sources (daemon, skills, MCP servers)
//! - `/tool connect <id>`       — connect an MCP server
//! - `/tool disconnect <id>`    — disconnect an MCP server
//! - `/tool add <id> <name> <url>` — register a new MCP server
//! - `/tool remove <id>`        — remove an MCP server

use async_trait::async_trait;
use serenity::all::{
    ActionRowComponent, AutocompleteChoice, CommandInteraction, CommandOptionType,
    CreateAutocompleteResponse, CreateInteractionResponse, CreateInteractionResponseMessage,
    InputTextStyle, ResolvedOption, ResolvedValue,
};
use serenity::builder::{
    CreateActionRow, CreateCommand, CreateCommandOption, CreateEmbed, CreateInputText, CreateModal,
};
use std::time::Duration;

use super::LuminaContext;
use crate::register_command;

#[derive(Default)]
pub struct Tool;

register_command!(Tool);

const MODAL_PREFIX: &str = "tool_call:";

#[async_trait]
impl super::SlashCommand for Tool {
    fn name(&self) -> &'static str {
        "tool"
    }

    fn register(&self) -> CreateCommand {
        CreateCommand::new("tool")
            .description("Browse, call, and manage tools and their sources")
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "list", "List all available tools (names only)")
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "info", "Show full details for a tool")
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::String, "name", "Tool name")
                            .required(true)
                            .set_autocomplete(true),
                    ),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "call", "Call a tool via a form")
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::String, "name", "Tool name")
                            .required(true)
                            .set_autocomplete(true),
                    ),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "sources", "List where tools come from (daemon, skills, MCP servers)")
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "mcp_connect", "Connect an MCP server")
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::String, "id", "Server id")
                            .required(true)
                            .set_autocomplete(true),
                    ),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "mcp_disconnect", "Disconnect an MCP server")
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::String, "id", "Server id")
                            .required(true)
                            .set_autocomplete(true),
                    ),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "mcp_add", "Add a new MCP server")
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::String, "id", "Server id (also used as display name)")
                            .required(true),
                    )
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::String, "url", "Server URL")
                            .required(true),
                    )
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::String, "auth_type", "Auth: none | oauth | token")
                            .required(false),
                    ),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "mcp_remove", "Remove an MCP server")
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::String, "id", "Server id")
                            .required(true)
                            .set_autocomplete(true),
                    ),
            )
    }

    async fn run(&self, lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
        let opts = cmd.data.options();
        let sub = opts.first().ok_or_else(|| anyhow::anyhow!("missing subcommand"))?;

        match sub.name {
            "list" => cmd_list(lx, cmd).await,
            "info" => {
                let name = sub_string_arg(&sub.value, "name")
                    .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
                cmd_info(lx, cmd, name).await
            }
            "call" => {
                let tool_name = sub_string_arg(&sub.value, "name")
                    .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
                cmd_call(lx, cmd, tool_name).await
            }
            "sources" => cmd_sources(lx, cmd).await,
            "mcp_connect" => {
                let id = sub_string_arg(&sub.value, "id")
                    .ok_or_else(|| anyhow::anyhow!("missing server id"))?;
                cmd_connect(lx, cmd, id).await
            }
            "mcp_disconnect" => {
                let id = sub_string_arg(&sub.value, "id")
                    .ok_or_else(|| anyhow::anyhow!("missing server id"))?;
                cmd_disconnect(lx, cmd, id).await
            }
            "mcp_add" => {
                let id = sub_string_arg(&sub.value, "id")
                    .ok_or_else(|| anyhow::anyhow!("missing id"))?;
                let url = sub_string_arg(&sub.value, "url")
                    .ok_or_else(|| anyhow::anyhow!("missing url"))?;
                let auth_type = sub_string_arg(&sub.value, "auth_type");
                cmd_add(lx, cmd, id, url, auth_type).await
            }
            "mcp_remove" => {
                let id = sub_string_arg(&sub.value, "id")
                    .ok_or_else(|| anyhow::anyhow!("missing server id"))?;
                cmd_remove(lx, cmd, id).await
            }
            other => Err(anyhow::anyhow!("unknown subcommand `{other}`")),
        }
    }

    async fn autocomplete(&self, lx: &LuminaContext, ac: &CommandInteraction) -> anyhow::Result<()> {
        let opts = ac.data.options();
        let Some(sub) = opts.first() else { return Ok(()); };

        let partial = ac.data.autocomplete()
            .map(|opt| opt.value.to_lowercase())
            .unwrap_or_default();

        let choices: Vec<AutocompleteChoice> = match sub.name {
            // Tool-name autocomplete: name only (no description) to keep the
            // popup scannable; info/call explain the tool on invocation.
            "call" | "info" => {
                let tools = lx.daemon.tools().get_definitions().await;
                tools.into_iter()
                    .filter(|t| partial.is_empty() || t.name.to_lowercase().contains(&partial))
                    .take(25)
                    .map(|t| {
                        let label: String = t.name.chars().take(100).collect();
                        AutocompleteChoice::new(label, t.name)
                    })
                    .collect()
            }
            // MCP-server-id autocomplete: all configured servers; the label
            // shows status so the user doesn't have to guess.
            "mcp_connect" | "mcp_disconnect" | "mcp_remove" => {
                let servers = lx.daemon.mcp().list_mcp_servers().await.unwrap_or_default();
                servers.into_iter()
                    .filter(|s| partial.is_empty() || s.id.to_lowercase().contains(&partial))
                    .take(25)
                    .map(|s| {
                        let marker = if s.is_connected { "✅" } else { "❌" };
                        let label = format!("{marker} {} — {}", s.id, s.name);
                        let label: String = label.chars().take(100).collect();
                        AutocompleteChoice::new(label, s.id)
                    })
                    .collect()
            }
            _ => vec![],
        };

        ac.create_response(
            &lx.http,
            CreateInteractionResponse::Autocomplete(
                CreateAutocompleteResponse::new().set_choices(choices),
            ),
        ).await?;
        Ok(())
    }
}

/// Extract a String argument from a SubCommand's options by name.
fn sub_string_arg<'a>(sub_value: &'a ResolvedValue<'a>, name: &str) -> Option<&'a str> {
    let ResolvedValue::SubCommand(sub_opts) = sub_value else { return None };
    sub_opts.iter().find_map(|o| match o {
        ResolvedOption { name: n, value: ResolvedValue::String(s), .. } if *n == name => Some(*s),
        _ => None,
    })
}

async fn reply_ephemeral(lx: &LuminaContext, cmd: &CommandInteraction, content: &str) -> anyhow::Result<()> {
    cmd.create_response(
        &lx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new().content(content).ephemeral(true),
        ),
    ).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// /tool call
// ---------------------------------------------------------------------------

async fn cmd_call(lx: &LuminaContext, cmd: &CommandInteraction, tool_name: &str) -> anyhow::Result<()> {
    let tools = lx.daemon.tools().get_definitions().await;
    let tool = tools
        .iter()
        .find(|t| t.name == tool_name)
        .ok_or_else(|| anyhow::anyhow!("tool not found: {tool_name}"))?;

    let schema = serde_json::to_value(&tool.input_schema).unwrap_or_default();
    let params = extract_params(&schema);

    if params.is_empty() {
        // No params — execute immediately, scoped to the invoking Discord user.
        let result = call_tool_as_user(lx, cmd.user.id.get(), tool_name, serde_json::Map::new()).await;
        send_tool_result_from_service(lx, cmd, tool_name, result).await
    } else {
        // Build modal with input fields (Discord max: 5 components)
        let mut components = Vec::new();
        for param in params.iter().take(5) {
            let style = if matches!(param.name.as_str(), "content" | "description" | "prompt" | "question") {
                InputTextStyle::Paragraph
            } else {
                InputTextStyle::Short
            };
            let mut input = CreateInputText::new(style, &param.label, &param.name);
            if let Some(desc) = &param.description {
                input = input.placeholder(desc);
            }
            // Pre-populate contextual Discord IDs
            match param.name.as_str() {
                "guild_id" => if let Some(gid) = cmd.guild_id {
                    input = input.value(gid.get().to_string());
                },
                "channel_id" => {
                    input = input.value(cmd.channel_id.get().to_string());
                },
                _ => {}
            }
            input = input.required(param.required);
            components.push(CreateActionRow::InputText(input));
        }

        let modal = CreateModal::new(
            format!("{MODAL_PREFIX}{tool_name}"),
            format!("Tool: {tool_name}"),
        ).components(components);

        cmd.create_response(&lx.http, CreateInteractionResponse::Modal(modal)).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// /tool list — names only, grouped lightly
// ---------------------------------------------------------------------------

async fn cmd_list(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
    let tools = lx.daemon.tools().get_definitions().await;

    if tools.is_empty() {
        return reply_ephemeral(lx, cmd, "No tools available.").await;
    }

    // Preserve the order `get_definitions()` returns so the index here matches
    // `tools.N.*` in LLM API error messages. Number entries so you can map an
    // error like "tools.0.custom.input_schema.type" straight to the offender.
    let width = (tools.len() - 1).to_string().len();
    let body = tools.iter()
        .enumerate()
        .map(|(i, t)| format!("`{:>w$}  {}`", i, t.name, w = width))
        .collect::<Vec<_>>()
        .join("\n");

    let pages = crate::paginator::paginate_text(&body, 1800);
    if pages.len() <= 1 {
        let embed = CreateEmbed::new()
            .title(format!("Tools ({} total)", tools.len()))
            .description(pages.first().map(|s| s.as_str()).unwrap_or(""))
            .color(0x5865F2);
        cmd.create_response(&lx.http, CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new().embed(embed),
        )).await?;
    } else {
        let text_pages: Vec<String> = pages.iter().enumerate().map(|(i, content)| {
            format!("**Tools** ({} total) — Page {}/{}\n\n{}", tools.len(), i + 1, pages.len(), content)
        }).collect();
        crate::paginator::send_paginated(lx, cmd, &text_pages, Duration::from_secs(120)).await?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// /tool info — full details + raw JSON schema for debugging
// ---------------------------------------------------------------------------

async fn cmd_info(lx: &LuminaContext, cmd: &CommandInteraction, tool_name: &str) -> anyhow::Result<()> {
    let tools = lx.daemon.tools().get_definitions().await;
    let Some(tool) = tools.iter().find(|t| t.name == tool_name) else {
        return reply_ephemeral(lx, cmd, &format!("Tool not found: `{tool_name}`")).await;
    };

    let schema = serde_json::to_value(&tool.input_schema).unwrap_or_default();
    let schema_pretty = crate::json_fmt::pretty_compact(&schema);
    let schema_block = if schema_pretty.len() > 1600 {
        format!("{}\n... (truncated)", &schema_pretty[..1600])
    } else {
        schema_pretty
    };

    let params = extract_params(&schema);
    let params_block = if params.is_empty() {
        "(no parameters)".to_string()
    } else {
        params.iter().map(|p| {
            let req = if p.required { " **required**" } else { "" };
            let desc = p.description.as_deref().unwrap_or("");
            format!("• `{}`{req} — {desc}", p.name)
        }).collect::<Vec<_>>().join("\n")
    };

    let description = tool.description.as_deref().unwrap_or("(no description)");
    let body = format!(
        "**Description**\n{description}\n\n**Parameters**\n{params_block}\n\n**Raw input_schema**\n```json\n{schema_block}\n```"
    );

    let embed = CreateEmbed::new()
        .title(format!("Tool: {tool_name}"))
        .description(body)
        .color(0x5865F2);
    cmd.create_response(&lx.http, CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new().embed(embed).ephemeral(true),
    )).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// /tool sources — unified skills + MCP-server view
// ---------------------------------------------------------------------------

async fn cmd_sources(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
    let skills = lx.daemon.skills().list_skills().await.unwrap_or_default();
    let servers = lx.daemon.mcp().list_mcp_servers().await.unwrap_or_default();

    let mut lines = Vec::new();

    if !skills.is_empty() {
        lines.push("**Skills**".to_string());
        for s in &skills {
            let marker = if s.is_connected { "✅" } else { "❌" };
            let kind = match s.kind.as_str() {
                "embedded" => "in-process",
                "ws" => "WS client",
                other => other,
            };
            let oauth = if s.oauth_provider_ids.is_empty() {
                String::new()
            } else {
                format!(" · needs: {}", s.oauth_provider_ids.join(", "))
            };
            let tool_word = if s.tool_count == 1 { "tool" } else { "tools" };
            lines.push(format!(
                "{marker} `{}` — {} ({kind}, {} {tool_word}){oauth}",
                s.id, s.display_name, s.tool_count,
            ));
        }
    }

    if !servers.is_empty() {
        if !lines.is_empty() { lines.push(String::new()); }
        lines.push("**MCP servers**".to_string());
        for s in &servers {
            let status = if s.is_connected {
                format!("✅ {} tools", s.tool_count)
            } else if s.needs_oauth_login {
                "🔑 needs OAuth login".to_string()
            } else {
                format!("❌ {}", s.status)
            };
            lines.push(format!("`{}` — {} — {status}", s.id, s.name));
        }
    }

    if lines.is_empty() {
        return reply_ephemeral(lx, cmd, "No tool sources registered.").await;
    }
    reply_ephemeral(lx, cmd, &lines.join("\n")).await
}

// ---------------------------------------------------------------------------
// /tool connect | disconnect | add | remove — MCP server management
// ---------------------------------------------------------------------------

async fn cmd_connect(lx: &LuminaContext, cmd: &CommandInteraction, id: &str) -> anyhow::Result<()> {
    let count = lx.daemon.mcp().connect_mcp_server(id).await?;
    reply_ephemeral(lx, cmd, &format!("Connected to `{id}` — {count} tools available.")).await
}

async fn cmd_disconnect(lx: &LuminaContext, cmd: &CommandInteraction, id: &str) -> anyhow::Result<()> {
    lx.daemon.mcp().disconnect_mcp_server(id).await?;
    reply_ephemeral(lx, cmd, &format!("Disconnected from `{id}`.")).await
}

async fn cmd_add(
    lx: &LuminaContext,
    cmd: &CommandInteraction,
    id: &str,
    url: &str,
    auth_type: Option<&str>,
) -> anyhow::Result<()> {
    // `id` doubles as the display name — users were being asked for both and
    // just typing the same thing twice.
    let auth = auth_type.unwrap_or("none").to_string();
    lx.daemon.mcp().add_mcp_server(simply_daemon_api::AddMcpServerRequest {
        id: id.to_string(),
        name: id.to_string(),
        url: url.to_string(),
        auth_type: auth,
        auth_token: None,
        provider_id: None,
        scopes: None,
    }).await?;
    reply_ephemeral(lx, cmd, &format!("Added MCP server `{id}`.")).await
}

async fn cmd_remove(lx: &LuminaContext, cmd: &CommandInteraction, id: &str) -> anyhow::Result<()> {
    lx.daemon.mcp().remove_mcp_server(id).await?;
    reply_ephemeral(lx, cmd, &format!("Removed MCP server `{id}`.")).await
}

// ---------------------------------------------------------------------------
// Modal handler
// ---------------------------------------------------------------------------

/// Handle a modal submission for a tool call.
pub async fn handle_modal(lx: &LuminaContext, modal: &serenity::model::application::ModalInteraction) -> anyhow::Result<()> {
    let tool_name = modal.data.custom_id
        .strip_prefix(MODAL_PREFIX)
        .ok_or_else(|| anyhow::anyhow!("not a tool modal"))?;

    let mut args = serde_json::Map::new();
    for row in &modal.data.components {
        if let Some(ActionRowComponent::InputText(input)) = row.components.first() {
            if let Some(ref value) = input.value {
                if !value.is_empty() {
                    args.insert(input.custom_id.clone(), parse_smart_value(value));
                }
            }
        }
    }

    // Defer since tool calls may take time
    modal.create_response(&lx.http, CreateInteractionResponse::Defer(
        CreateInteractionResponseMessage::new(),
    )).await?;

    let result = call_tool_as_user(lx, modal.user.id.get(), tool_name, args).await;

    send_tool_result_from_service(lx, modal.channel_id, tool_name, result).await?;
    modal.delete_response(&lx.http).await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct ParamInfo {
    name: String,
    label: String,
    description: Option<String>,
    required: bool,
}

fn extract_params(schema: &serde_json::Value) -> Vec<ParamInfo> {
    let properties = schema.get("properties").and_then(|p| p.as_object());
    let required_list: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let Some(props) = properties else { return vec![] };

    let mut params: Vec<ParamInfo> = props
        .iter()
        .map(|(name, prop)| {
            let description = prop.get("description").and_then(|d| d.as_str()).map(|s| s.to_string());
            let label = description.as_deref().unwrap_or(name).chars().take(45).collect::<String>();
            ParamInfo {
                name: name.clone(),
                label,
                description,
                required: required_list.contains(&name.as_str()),
            }
        })
        .collect();

    // Required params first
    params.sort_by_key(|p| !p.required);
    params
}

fn parse_smart_value(s: &str) -> serde_json::Value {
    if let Ok(n) = s.parse::<i64>() {
        return serde_json::Value::Number(n.into());
    }
    if let Ok(n) = s.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(n) {
            return serde_json::Value::Number(n);
        }
    }
    match s {
        "true" => return serde_json::Value::Bool(true),
        "false" => return serde_json::Value::Bool(false),
        _ => {}
    }
    if s.starts_with('[') || s.starts_with('{') {
        if let Ok(v) = serde_json::from_str(s) {
            return v;
        }
    }
    serde_json::Value::String(s.to_string())
}

/// Send a ToolService result (Vec<ToolResultContent>) as a Discord response.
///
/// Defers the Command target's interaction (tool calls may take time),
/// then delegates formatting/pagination to `tool_render` so the `/tool`
/// surface and the voice session receiver render MCP results identically.
async fn send_tool_result_from_service(
    lx: &LuminaContext,
    target: impl Into<SendTarget<'_>>,
    tool_name: &str,
    result: anyhow::Result<Vec<simply_daemon_api::ToolResultContent>>,
) -> anyhow::Result<()> {
    let target = target.into();
    let channel_id = match &target {
        SendTarget::Channel(id) => *id,
        SendTarget::Command(cmd) => {
            cmd.create_response(&lx.http, CreateInteractionResponse::Defer(
                CreateInteractionResponseMessage::new(),
            )).await?;
            cmd.channel_id
        }
    };

    crate::tool_render::render_tool_result_to_channel(
        &lx.http, &lx.ctx.shard, channel_id, tool_name, result, Vec::new(),
    ).await?;

    if let SendTarget::Command(cmd) = target {
        cmd.delete_response(&lx.http).await.ok();
    }
    Ok(())
}

enum SendTarget<'a> {
    Channel(serenity::model::id::ChannelId),
    Command(&'a CommandInteraction),
}

impl<'a> From<&'a CommandInteraction> for SendTarget<'a> {
    fn from(cmd: &'a CommandInteraction) -> Self { Self::Command(cmd) }
}

impl From<serenity::model::id::ChannelId> for SendTarget<'_> {
    fn from(id: serenity::model::id::ChannelId) -> Self { Self::Channel(id) }
}

/// Call a tool on behalf of the invoking Discord user.
///
/// Resolves the Discord id to a daemon `RequestContext` (carrying user identity
/// and any stored OAuth tokens) and dispatches via `call_tool_direct` — the same
/// scoped path normal chat and `/google` use. The anonymous `tools().call_tool()`
/// shortcut is what made `/tool call entity__list_entities` fail with
/// "authentication required": it dropped the caller's identity. Returns
/// `Vec<ToolResultContent>` so callers render through the unified `tool_render`.
async fn call_tool_as_user(
    lx: &LuminaContext,
    discord_user_id: u64,
    tool_name: &str,
    args: serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<Vec<simply_daemon_api::ToolResultContent>> {
    let ctx = lx.ctx_for(discord_user_id).await;
    let request = simply_daemon_api::CallToolRequestParams::new(tool_name.to_string())
        .with_arguments(args);
    let result = lx.daemon.mcp().call_tool_direct(&ctx, request).await?;
    Ok(result.content.into_iter().map(|c| match c.raw {
        rmcp::model::RawContent::Text(t) => simply_daemon_api::ToolResultContent::text(t.text),
        rmcp::model::RawContent::Image(img) => simply_daemon_api::ToolResultContent::image(img.data, img.mime_type),
        _ => simply_daemon_api::ToolResultContent::text("[unsupported content type]"),
    }).collect())
}

