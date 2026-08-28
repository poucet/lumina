//! AI chat message handling.
//!
//! Processes messages in AI Chats category channels and @mentions,
//! forwarding them to the daemon for LLM responses.

use std::collections::HashMap;

use serenity::builder::{CreateEmbed, GetMessages};
use serenity::model::channel::Message;
use serenity::model::id::ChannelId;
use simply_daemon_api::*;

use crate::commands::LuminaContext;

const DEFAULT_HISTORY_LIMIT: u16 = 1000;

// TODO: Load from UCM document (Content Stage 2)
fn build_system_prompt(msg: &Message) -> String {
    let channel_id = msg.channel_id.get();
    let guild_id = msg.guild_id.map(|g| g.get()).unwrap_or(0);

    format!("\
You are Lumina, an intelligent AI assistant on Discord.

## Current context
- Channel ID: {channel_id}
- Guild ID: {guild_id}

When using Discord tools, use these IDs directly — do not invent or guess IDs.

## Conversation history
The messages in this conversation are your actual conversation history with the user(s) in this channel. You can refer back to anything said earlier — it is your memory of this conversation.

## Formatting
- For channel references: <#channel_id>
- For user mentions: <@user_id>
- For timestamps: <t:timestamp_seconds:R>

Messages from users are prefixed with their Discord mention (e.g. '<@12345> says: hello').
Use these mentions when referring to what someone said.

Be helpful, concise, and conversational.")
}

/// Handle a click on the "📥 JSON" button attached to a tool-call or
/// tool-result embed. Looks up the stashed payload by the message the
/// button sits on and replies ephemerally with the JSON as an attached
/// file (visible only to the clicker, zero clutter in the channel).
pub async fn handle_tool_json_button(
    lx: &LuminaContext,
    comp: &serenity::model::application::ComponentInteraction,
) -> anyhow::Result<()> {
    use serenity::builder::{
        CreateAttachment, CreateInteractionResponse, CreateInteractionResponseMessage,
    };

    let msg_id = comp.message.id.get();
    let found = lx.state.tool_state.get_many(&[msg_id]).unwrap_or_default();

    let response = match found.get(&msg_id) {
        Some((kind, payload)) => {
            let filename = match kind.as_str() {
                crate::tool_state::KIND_TOOL_CALL => format!("tool_call_{msg_id}.json"),
                crate::tool_state::KIND_TOOL_RESULT => format!("tool_result_{msg_id}.json"),
                _ => format!("tool_{msg_id}.json"),
            };
            let attachment = CreateAttachment::bytes(payload.as_bytes().to_vec(), filename);
            CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .add_file(attachment)
        }
        None => CreateInteractionResponseMessage::new()
            .ephemeral(true)
            .content("No stored JSON for this message (it may predate the lumina tool_state store)."),
    };

    comp.create_response(&lx.http, CreateInteractionResponse::Message(response))
        .await?;
    Ok(())
}

/// Handle an incoming message — checks if it should get an AI response,
/// then processes it.
pub async fn handle_message(lx: &LuminaContext, msg: &Message) {
    if !should_respond(lx, msg).await {
        tracing::debug!(
            channel_id = %msg.channel_id,
            author = %msg.author.name,
            "skipping message (not AI chat channel or mention)"
        );
        return;
    }

    tracing::info!(
        channel_id = %msg.channel_id,
        author = %msg.author.name,
        "processing chat message"
    );

    let typing = msg.channel_id.start_typing(&lx.http);

    if let Err(e) = process_chat(lx, msg).await {
        tracing::error!(error = %e, "chat processing failed");
        let _ = msg
            .channel_id
            .say(&lx.http, format!("Error: {e}"))
            .await;
    }

    drop(typing);
}

/// Number of recent user messages to use as the RAG query.
const RAG_QUERY_MESSAGES: usize = 5;

/// Core chat flow: load history, create session, send message, stream response.
async fn process_chat(lx: &LuminaContext, msg: &Message) -> anyhow::Result<()> {
    let bot_id = lx.ctx.cache.current_user().id;

    // 1. Load channel history as seed messages
    let limit = lx.config.discord.history_limit.unwrap_or(DEFAULT_HISTORY_LIMIT);
    let history = load_channel_history(lx, msg.channel_id, bot_id.get(), limit).await?;

    // 2. Build system prompt with RAG context
    let mut system_prompt = build_system_prompt(msg);
    let (rag_context, rag_hits) = build_rag_context(lx, msg, &history).await;
    if !rag_context.is_empty() {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&rag_context);

        // Show debug embed if channel has [rag-debug] topic tag
        if crate::commands::chat::has_topic_tag(lx, msg.channel_id, "rag-debug") {
            let desc = rag_hits.iter()
                .map(|h| format!("`{}` ({}) — {:.0}%", h.title, h.entity_kind, h.score * 100.0))
                .collect::<Vec<_>>()
                .join("\n");
            let embed = CreateEmbed::new()
                .title(format!("RAG: {} entities injected", rag_hits.len()))
                .description(desc)
                .color(0x9B59B6);
            let _ = msg.channel_id.send_message(&lx.http, serenity::builder::CreateMessage::new().embed(embed)).await;
        }
    }

    // 3. Create session with seed and resolved model
    let model_id = resolve_model(lx, msg).await;
    tracing::debug!(seed_count = history.len(), "creating session");
    let mut session_ctx = lx.ctx_for(msg.author.id.get()).await;
    // Stamp the Discord origin so skill tools can act on "where the chat is
    // happening" without the LLM having to pass guild/channel IDs.
    session_ctx = session_ctx.with_metadata("discord.channel_id", msg.channel_id.get().to_string());
    if let Some(gid) = msg.guild_id {
        session_ctx = session_ctx.with_metadata("discord.guild_id", gid.get().to_string());
    }
    let mut session = simply_daemon::DaemonSession::create(
        lx.daemon.clone(),
        session_ctx,
        CreateSessionOptions {
            persistence: Some(Persistence::Ephemeral),
            system_prompt: Some(system_prompt),
            model_id,
            seed: history,
            tool_filter: None,
        },
    )
    .await?;

    tracing::info!(session_id = %session.id(), model = %session.model_id(), "session created");

    // 3. Send the current message
    let user_text = format!("<@{}> says: {}", msg.author.id, msg.content);
    tracing::debug!("sending message to daemon");
    session
        .send(UserMessage {
            content: vec![InputContent::Text { text: user_text }],
        })
        .await?;

    // 4. Stream response back to Discord (session closes on drop)
    stream_response(lx, msg, &mut session).await
}

/// A RAG hit joined with its resolved entity metadata + body. We do the
/// join here (search returns only (entity_id, entity_kind, score)) so
/// the caller can both render a debug embed and inject the full block
/// into the prompt in a single pass.
pub struct RagHit {
    pub entity_id: String,
    pub entity_kind: String,
    pub title: String,
    pub body: String,
    pub score: f32,
}

/// Default entity-kind filter for Lumina's chat RAG: pull from
/// user-authored documents (notes, knowledge, tabs, …) but never from
/// system prompts, access rules, or MCP-server config notes.
fn default_chat_entity_filter() -> EntityFilter {
    EntityFilter {
        include: vec![
            EntityTypeMatcher::Prefix("document::".to_string()),
        ],
        exclude: vec![
            EntityTypeMatcher::Exact("document::system_prompt".to_string()),
            EntityTypeMatcher::Exact("document::access_rule".to_string()),
            EntityTypeMatcher::Exact("document::mcp_server".to_string()),
        ],
    }
}

/// Build RAG context by searching for relevant entities based on recent
/// messages. Returns `(context_string, hits)` so the caller can render
/// a debug embed.
async fn build_rag_context(
    lx: &LuminaContext,
    msg: &Message,
    history: &[SeedMessage],
) -> (String, Vec<RagHit>) {
    let mut query_parts: Vec<&str> = Vec::new();
    let user_messages: Vec<&str> = history
        .iter()
        .rev()
        .filter(|m| matches!(m.role, Role::User))
        .take(RAG_QUERY_MESSAGES)
        .filter_map(|m| m.content.iter().find_map(|c| match c {
            InputContent::Text { text } => Some(text.as_str()),
            _ => None,
        }))
        .collect();
    query_parts.extend(user_messages.iter().rev());
    query_parts.push(&msg.content);

    let query = query_parts.join("\n");
    if query.trim().is_empty() {
        return (String::new(), vec![]);
    }

    let request = SearchRequest {
        query,
        top_k: Some(5),
        entity_filter: Some(default_chat_entity_filter()),
    };
    let ctx = lx.ctx_for(msg.author.id.get()).await;

    let hits = match lx.daemon.search().search(&ctx, request).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "RAG search failed, continuing without context");
            return (String::new(), vec![]);
        }
    };
    if hits.is_empty() {
        return (String::new(), vec![]);
    }

    // One batch fetch for all entities + their bodies instead of 2×N
    // round-trips. Hits whose entity was deleted since indexing fall
    // out naturally (missing from the response); score order preserved
    // via a small id→score lookup.
    let score_by_id: std::collections::HashMap<String, (String, f32)> = hits
        .iter()
        .map(|h| (h.entity_id.clone(), (h.entity_kind.clone(), h.score)))
        .collect();
    let entities = match lx.daemon.entity().get_entities(&ctx, GetEntitiesRequest {
        ids: hits.iter().map(|h| h.entity_id.clone()).collect(),
        include_content: true,
    }).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "RAG batch fetch failed, skipping context");
            return (String::new(), vec![]);
        }
    };

    let mut rag_hits: Vec<RagHit> = Vec::with_capacity(entities.len());
    for ewc in entities {
        let Some((kind, score)) = score_by_id.get(&ewc.summary.id).cloned() else { continue };
        let body = ewc.content.and_then(|c| c.content_markdown).unwrap_or_default();
        if body.trim().is_empty() { continue; }
        rag_hits.push(RagHit {
            entity_id: ewc.summary.id,
            entity_kind: kind,
            title: ewc.summary.title.unwrap_or_else(|| "(untitled)".to_string()),
            body,
            score,
        });
    }
    // Search dedupes by content block, so sorting here just restores
    // score-descending after the batch fetch re-orders us.
    rag_hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    if rag_hits.is_empty() {
        return (String::new(), vec![]);
    }

    // Inject title + body only — deliberately omit `entity_id` so the
    // model doesn't pair this context with a `get_entity_content` tool
    // call to re-fetch what it already has. If the user later refers to
    // one of these entities by name, the model can still discover it
    // via the search tool.
    let mut context = String::from(
        "## Relevant knowledge\n\
         The following entity contents have already been looked up and injected \
         for you — you do NOT need to fetch or re-read them. Treat them as \
         authoritative context:\n",
    );
    for h in &rag_hits {
        context.push_str(&format!(
            "\n---\ntitle: {}\nkind: {}\n---\n{}\n",
            h.title, h.entity_kind, h.body,
        ));
    }

    tracing::info!(hits = rag_hits.len(), "RAG context injected");
    (context, rag_hits)
}

/// Load recent channel messages and convert to seed messages for the daemon.
async fn load_channel_history(
    lx: &LuminaContext,
    channel_id: ChannelId,
    bot_user_id: u64,
    limit: u16,
) -> anyhow::Result<Vec<SeedMessage>> {
    let mut all_messages = Vec::new();
    let mut remaining = limit;
    let mut before = None;

    while remaining > 0 {
        let batch_size = remaining.min(100) as u8;
        let mut request = GetMessages::new().limit(batch_size);
        if let Some(id) = before {
            request = request.before(id);
        }

        let batch = channel_id.messages(&lx.http, request).await?;
        if batch.is_empty() {
            break;
        }

        before = batch.last().map(|m| m.id);
        remaining = remaining.saturating_sub(batch.len() as u16);
        all_messages.extend(batch);
    }

    // Discord returns newest-first, we need oldest-first.
    // Skip bot messages that appear before any user message (welcome /
    // prior-conversation tool turns) — they'd otherwise leak into the
    // new conversation.
    let ordered: Vec<&Message> = all_messages
        .iter()
        .rev()
        .skip_while(|m| m.author.id.get() == bot_user_id)
        .collect();

    // Batch-fetch tool-state for every bot message that carries an
    // embed — those are the tool-call / tool-result emissions. Non-embed
    // bot messages are plain text assistant replies; no state stashed.
    let tool_state_ids: Vec<u64> = ordered
        .iter()
        .filter(|m| m.author.id.get() == bot_user_id && !m.embeds.is_empty())
        .map(|m| m.id.get())
        .collect();
    let stash = lx
        .state
        .tool_state
        .get_many(&tool_state_ids)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "tool_state batch fetch failed");
            std::collections::HashMap::new()
        });

    let mut seed: Vec<SeedMessage> = Vec::with_capacity(ordered.len());
    for m in ordered {
        let stash_entry = stash.get(&m.id.get());
        if let Some(msg) = message_to_seed(m, bot_user_id, stash_entry).await {
            seed.push(msg);
        }
    }

    // An LLM given a leading ToolCall or ToolResult with no matching
    // assistant / user turn before it usually chokes. Drop any such
    // orphans from the head of the seed until we hit a normal turn.
    while let Some(first) = seed.first() {
        let is_orphan_tool = first.content.iter().any(|c| matches!(
            c,
            InputContent::ToolCall(_) | InputContent::ToolResult(_)
        ));
        if is_orphan_tool {
            seed.remove(0);
        } else {
            break;
        }
    }

    // Don't include the current message (it'll be sent separately)
    if let Some(last) = seed.last() {
        if matches!(last.role, Role::User) {
            seed.pop();
        }
    }

    Ok(seed)
}

/// Convert a Discord message to a `SeedMessage`. For bot messages
/// backed by stashed tool state (`(kind, payload)` from the lumina
/// tool_state DB), reconstructs the structured `ToolCall` /
/// `ToolResult` turn; otherwise falls back to text. Returns `None` for
/// messages that carry nothing we can seed from (e.g. an embed without
/// matching stash, or an empty user message).
async fn message_to_seed(
    m: &Message,
    bot_user_id: u64,
    tool_stash: Option<&(String, String)>,
) -> Option<SeedMessage> {
    let is_bot = m.author.id.get() == bot_user_id;

    if is_bot {
        if let Some((kind, payload)) = tool_stash {
            match kind.as_str() {
                crate::tool_state::KIND_TOOL_CALL => {
                    if let Ok(call) = serde_json::from_str::<ToolCall>(payload) {
                        return Some(SeedMessage {
                            role: Role::Assistant,
                            content: vec![InputContent::ToolCall(call)],
                        });
                    }
                }
                crate::tool_state::KIND_TOOL_RESULT => {
                    if let Ok(result) = serde_json::from_str::<ToolResult>(payload) {
                        return Some(SeedMessage {
                            role: Role::User,
                            content: vec![InputContent::ToolResult(result)],
                        });
                    }
                }
                _ => {}
            }
        }

        if m.content.is_empty() {
            return None;
        }
        Some(SeedMessage {
            role: Role::Assistant,
            content: vec![InputContent::Text { text: m.content.clone() }],
        })
    } else {
        if m.content.is_empty() {
            return None;
        }
        let text = format!("<@{}> says: {}", m.author.id, m.content);
        Some(SeedMessage {
            role: Role::User,
            content: vec![InputContent::Text { text }],
        })
    }
}

/// Stream daemon events back to Discord.
/// Collects text via AssistantContent and sends once on TurnComplete.
async fn stream_response(
    lx: &LuminaContext,
    msg: &Message,
    session: &mut simply_daemon::DaemonSession,
) -> anyhow::Result<()> {
    let mut text_buffer = String::new();
    // DaemonEvent::ToolResult only carries the tool-call id; the rendered
    // "Tool result" message is titled with the tool name, so track the
    // name seen on the matching ToolCall.
    let mut tool_names: HashMap<String, String> = HashMap::new();

    loop {
        match session.recv().await {
            Ok(DaemonEvent::TextDelta(_)) => {}
            Ok(DaemonEvent::AssistantContent(content_block)) => {
                match content_block {
                    ContentBlock::Text { text } => {
                        text_buffer.push_str(&text);
                    }
                    ContentBlock::Image { data, mime_type } => {
                        let ext = match mime_type.as_str() {
                            "image/png" => "png",
                            "image/gif" => "gif",
                            "image/webp" => "webp",
                            _ => "jpg",
                        };
                        if let Ok(bytes) = base64_decode(&data) {
                            let attachment = serenity::builder::CreateAttachment::bytes(bytes, format!("image.{ext}"));
                            msg.channel_id.send_message(&lx.http, serenity::builder::CreateMessage::new().add_file(attachment)).await?;
                        }
                    }
                    ContentBlock::Audio { data, mime_type } => {
                        let ext = match mime_type.as_str() {
                            "audio/mp3" | "audio/mpeg" => "mp3",
                            "audio/ogg" => "ogg",
                            "audio/wav" => "wav",
                            _ => "mp3",
                        };
                        if let Ok(bytes) = base64_decode(&data) {
                            let attachment = serenity::builder::CreateAttachment::bytes(bytes, format!("audio.{ext}"));
                            msg.channel_id.send_message(&lx.http, serenity::builder::CreateMessage::new().add_file(attachment)).await?;
                        }
                    }
                    _ => {}
                }
            }
            Ok(DaemonEvent::ToolCall { id, name, arguments }) => {
                if let Err(e) = crate::tool_render::post_tool_call(
                    &lx.http, msg.channel_id, &id, &name, arguments,
                    Some(lx.state.tool_state.as_ref()),
                ).await {
                    tracing::warn!(tool = %name, error = %e, "failed to post tool call");
                }
                tool_names.insert(id, name);
            }
            Ok(DaemonEvent::ToolResult { id, result }) => {
                let tool_name = tool_names
                    .remove(&id)
                    .unwrap_or_else(|| "\u{1f4e6} Tool result".to_string());
                if let Err(e) = crate::tool_render::post_tool_result(
                    &lx.http, &lx.ctx.shard, msg.channel_id, &id, &tool_name, result,
                    Some(lx.state.tool_state.as_ref()),
                ).await {
                    tracing::warn!(tool = %tool_name, error = %e, "failed to render tool result");
                }
            }
            Ok(DaemonEvent::TurnComplete) => {
                if !text_buffer.is_empty() {
                    for page in crate::tool_render::paginate_tool_output(&text_buffer, 2000) {
                        msg.channel_id.say(&lx.http, &page).await?;
                    }
                }
                break;
            }
            Ok(DaemonEvent::Error(e)) => {
                return Err(anyhow::anyhow!("daemon error: {e}"));
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "event stream lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }

    Ok(())
}

fn base64_decode(data: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(data)?)
}


// ---------------------------------------------------------------------------
// Model resolution
// ---------------------------------------------------------------------------

/// Resolve which model to use for a message.
/// Priority: channel topic tag → config default → None (daemon picks).
/// Validates the resolved model exists; falls back to config default if not.
async fn resolve_model(lx: &LuminaContext, msg: &Message) -> Option<String> {
    let from_topic = crate::commands::chat::get_topic_tag(lx, msg.channel_id, "model");

    let config_default = lx
        .config
        .discord
        .model_id
        .clone()
        .filter(|s| !s.is_empty());

    let candidate = from_topic.or(config_default.clone());

    let Some(model_id) = candidate else {
        return None;
    };

    // Validate the model exists
    if let Ok(models) = lx.daemon.model().list_models().await {
        let exists = models.iter().any(|m| m.id.to_string() == model_id);
        if exists {
            return Some(model_id);
        }
        tracing::warn!(model = %model_id, "model not found, falling back to default");
    }

    // Fallback to config default (which may also be None)
    config_default
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Check if the bot should respond to this message.
async fn should_respond(lx: &LuminaContext, msg: &Message) -> bool {
    if !is_ai_chat_channel(lx, msg) && !is_mentioned(lx, msg) {
        return false;
    }

    // Check channel topic for [paused] tag
    if crate::commands::chat::has_topic_tag(lx, msg.channel_id, "paused") {
        return false;
    }

    true
}

/// Message is in a channel under the configured AI Chats category.
fn is_ai_chat_channel(lx: &LuminaContext, msg: &Message) -> bool {
    let cat_id = match lx.config.discord.ai_chats_category_id {
        Some(id) => ChannelId::new(id),
        None => return false,
    };

    let guild_id = match msg.guild_id {
        Some(id) => id,
        None => return false,
    };

    lx.ctx
        .cache
        .guild(guild_id)
        .and_then(|guild| {
            guild
                .channels
                .get(&msg.channel_id)
                .and_then(|ch| ch.parent_id)
                .map(|parent| parent == cat_id)
        })
        .unwrap_or(false)
}

/// Bot is @mentioned in the message.
fn is_mentioned(lx: &LuminaContext, msg: &Message) -> bool {
    let bot_id = lx.ctx.cache.current_user().id;
    msg.mentions.iter().any(|u| u.id == bot_id)
}
