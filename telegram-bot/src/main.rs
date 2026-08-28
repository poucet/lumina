//! Telegram bot for the Simply platform.

mod api;
mod memory;
mod skill;

use std::sync::Arc;
use std::time::Duration;

use api::{BotCommand, ChatAction, Message, TelegramApi, User};
use memory::{ConversationKey, ConversationMemory};
use rmcp::model::RawContent;
use simply_daemon_api::{
    CallToolRequestParams, ContentBlock, CreateSessionOptions, Daemon, DaemonEvent, InputContent,
    ModelCapability, Persistence, RequestContext, Role, SeedMessage, ToolCall, ToolResult,
    ToolResultContent, UserMessage,
};

const GOOGLE_PROVIDER_ID: &str = "google";
const TELEGRAM_MESSAGE_LIMIT: usize = 3900;

#[derive(Clone)]
struct AppState {
    daemon: Arc<dyn Daemon>,
    config: config::TelegramConfig,
    memory: ConversationMemory,
}

impl AppState {
    async fn ctx_for_message(&self, message: &Message) -> RequestContext {
        let anon = RequestContext::anonymous();
        let mut ctx = match &message.from {
            Some(user) => {
                let external_id = format!("telegram:{}", user.id);
                match self.daemon.user().resolve_user(&anon, external_id).await {
                    Ok(Some(scope)) => RequestContext::with_scope(scope),
                    _ => RequestContext::anonymous(),
                }
            }
            None => RequestContext::anonymous(),
        };

        ctx = ctx
            .with_metadata("telegram.chat_id", message.chat.id.to_string())
            .with_metadata("telegram.chat_type", message.chat.kind.clone())
            .with_metadata("telegram.chat_name", message.chat.display_name())
            .with_metadata("telegram.message_id", message.message_id.to_string());

        if let Some(thread_id) = message.message_thread_id {
            ctx = ctx.with_metadata("telegram.message_thread_id", thread_id.to_string());
        }
        if let Some(user) = &message.from {
            ctx = ctx
                .with_metadata("telegram.user_id", user.id.to_string())
                .with_metadata("telegram.user_name", user.display_name());
            if let Some(username) = &user.username {
                ctx = ctx.with_metadata("telegram.username", username.clone());
            }
        }

        ctx
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    setup_logging();

    config::load_env_file();
    let telegram_cfg = config::TelegramConfig::load();
    let settings = config::Settings::load();

    let token = telegram_cfg
        .bot_token()
        .expect("TELEGRAM_BOT_TOKEN env var or telegram.bot_token in telegram.toml is required")
        .to_string();

    let daemon = connect_daemon(&settings).await?;
    let api = TelegramApi::new(token);
    let me = api.get_me().await?;
    tracing::info!(
        bot_id = me.id,
        username = ?me.username,
        "connected to Telegram"
    );

    if let Err(e) = api.delete_webhook().await {
        tracing::warn!(error = %e, "could not delete Telegram webhook before polling");
    }
    if let Err(e) = api.set_my_commands(default_commands()).await {
        tracing::warn!(error = %e, "could not register Telegram bot commands");
    }

    register_skills(&daemon, api.clone()).await?;

    let state = Arc::new(AppState {
        daemon,
        config: telegram_cfg,
        memory: ConversationMemory::default(),
    });

    tracing::info!("telegram bot polling started");
    run_polling(api, me, state).await
}

async fn run_polling(api: TelegramApi, me: User, state: Arc<AppState>) -> anyhow::Result<()> {
    let mut offset = None;
    loop {
        let updates = match api
            .get_updates(offset, state.config.poll_timeout_secs())
            .await
        {
            Ok(updates) => updates,
            Err(e) => {
                tracing::warn!(error = %e, "Telegram polling failed; retrying");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        for update in updates {
            offset = Some(update.update_id + 1);
            let Some(message) = update.message else {
                continue;
            };

            let api = api.clone();
            let me = me.clone();
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let chat_id = message.chat.id;
                let thread_id = message.message_thread_id;
                if let Err(e) = handle_message(api.clone(), me, Arc::clone(&state), message).await {
                    tracing::error!(chat_id, error = %e, "Telegram message handling failed");
                    let _ = api
                        .send_message(chat_id, thread_id, format!("Error: {e}"))
                        .await;
                }
            });
        }
    }
}

async fn handle_message(
    api: TelegramApi,
    me: User,
    state: Arc<AppState>,
    message: Message,
) -> anyhow::Result<()> {
    if message.from.as_ref().is_some_and(|user| user.is_bot) {
        return Ok(());
    }
    if !is_allowed(&state.config, &message) {
        tracing::debug!(
            chat_id = message.chat.id,
            user_id = ?message.from.as_ref().map(|u| u.id),
            "Telegram message skipped by allowlist"
        );
        return Ok(());
    }

    let Some(text) = message.text.as_deref() else {
        return Ok(());
    };

    if let Some(command) = parse_command(text) {
        if !command_is_for_bot(&command, &me, &message) {
            return Ok(());
        }
        if handle_command(
            api.clone(),
            me.clone(),
            Arc::clone(&state),
            &message,
            command,
        )
        .await?
        {
            return Ok(());
        }
    }

    if !should_respond(&state.config, &me, &message, text) {
        return Ok(());
    }

    let prompt_text = strip_bot_mention(text, &me).trim().to_string();
    if prompt_text.is_empty() {
        return Ok(());
    }

    let key = ConversationKey::from_message(&message);
    let _guard = state.memory.lock(key).await;
    process_chat(api, me, state, message, key, prompt_text).await
}

async fn handle_command(
    api: TelegramApi,
    me: User,
    state: Arc<AppState>,
    message: &Message,
    command: ParsedCommand,
) -> anyhow::Result<bool> {
    match command.name.as_str() {
        "start" | "help" => {
            send_pages(
                &api,
                message.chat.id,
                message.message_thread_id,
                &help_text(&me),
            )
            .await?;
            Ok(true)
        }
        "reset" => {
            state
                .memory
                .clear(ConversationKey::from_message(message))
                .await;
            api.send_message(
                message.chat.id,
                message.message_thread_id,
                "Conversation reset.",
            )
            .await?;
            Ok(true)
        }
        "whoami" => {
            let user_id = message
                .from
                .as_ref()
                .map(|u| u.id.to_string())
                .unwrap_or_else(|| "(unknown)".to_string());
            let text = format!(
                "Telegram user ID: {user_id}\nTelegram chat ID: {}\nThread ID: {}",
                message.chat.id,
                message
                    .message_thread_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "(none)".to_string())
            );
            api.send_message(message.chat.id, message.message_thread_id, text)
                .await?;
            Ok(true)
        }
        "auth" => {
            send_google_auth(api, state, message).await?;
            Ok(true)
        }
        "google" => {
            handle_google_command(api, state, message, &command.args).await?;
            Ok(true)
        }
        _ => {
            if message.chat.is_private() {
                api.send_message(
                    message.chat.id,
                    message.message_thread_id,
                    "Unknown command. Send /help for available commands.",
                )
                .await?;
            }
            Ok(true)
        }
    }
}

async fn handle_google_command(
    api: TelegramApi,
    state: Arc<AppState>,
    message: &Message,
    args: &str,
) -> anyhow::Result<()> {
    let mut parts = args.trim().splitn(2, char::is_whitespace);
    let subcommand = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();

    match subcommand {
        "auth" => send_google_auth(api, state, message).await,
        "status" => {
            match call_tool_text(
                &state,
                message,
                "gdocs_list",
                serde_json::json!({ "query": null, "limit": 1 }),
            )
            .await
            {
                Ok(_) => {
                    api.send_message(
                        message.chat.id,
                        message.message_thread_id,
                        "Google is connected.",
                    )
                    .await?;
                }
                Err(e) => {
                    let text = if e.to_string().contains("OAuth")
                        || e.to_string().contains("token")
                        || e.to_string().contains("authenticate")
                    {
                        "Google is not connected. Send /google auth first.".to_string()
                    } else {
                        format!("Google status check failed: {e}")
                    };
                    api.send_message(message.chat.id, message.message_thread_id, text)
                        .await?;
                }
            }
            Ok(())
        }
        "import" => {
            if rest.is_empty() {
                api.send_message(
                    message.chat.id,
                    message.message_thread_id,
                    "Usage: /google import <google-doc-url-or-id>",
                )
                .await?;
                return Ok(());
            }

            api.send_chat_action(
                message.chat.id,
                message.message_thread_id,
                ChatAction::Typing,
            )
            .await
            .ok();
            let doc_id = extract_doc_id(rest);
            let text = call_tool_text(
                &state,
                message,
                "gdocs_import",
                serde_json::json!({ "doc_id": doc_id }),
            )
            .await?;
            send_pages(&api, message.chat.id, message.message_thread_id, &text).await
        }
        _ => {
            api.send_message(
                message.chat.id,
                message.message_thread_id,
                "Usage: /google auth | /google status | /google import <google-doc-url-or-id>",
            )
            .await?;
            Ok(())
        }
    }
}

async fn send_google_auth(
    api: TelegramApi,
    state: Arc<AppState>,
    message: &Message,
) -> anyhow::Result<()> {
    if !message.chat.is_private() {
        api.send_message(
            message.chat.id,
            message.message_thread_id,
            "For account linking, send /google auth to me in a private chat.",
        )
        .await?;
        return Ok(());
    }

    let user = message
        .from
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("cannot authenticate a message without a Telegram user"))?;
    let base_url = state.daemon.core().public_url().await?;
    let external_id = format!("telegram:{}", user.id);
    let auth_url = format!(
        "{base_url}/auth/mcp/{GOOGLE_PROVIDER_ID}?external_id={}",
        urlencoding::encode(&external_id),
    );
    api.send_message(
        message.chat.id,
        message.message_thread_id,
        format!("Click to connect Google:\n{auth_url}"),
    )
    .await?;
    Ok(())
}

async fn process_chat(
    api: TelegramApi,
    me: User,
    state: Arc<AppState>,
    message: Message,
    key: ConversationKey,
    prompt_text: String,
) -> anyhow::Result<()> {
    api.send_chat_action(
        message.chat.id,
        message.message_thread_id,
        ChatAction::Typing,
    )
    .await
    .ok();

    let ctx = state
        .ctx_for_message(&message)
        .await
        .with_metadata("telegram.prompt_message_id", message.message_id.to_string());
    let history_limit = state.config.history_limit();
    let seed = state.memory.seed(key, history_limit).await;
    let system_prompt = build_system_prompt(&message, &me);
    let model_id = resolve_model(&state).await;

    let mut session = simply_daemon::DaemonSession::create(
        state.daemon.clone(),
        ctx,
        CreateSessionOptions {
            persistence: Some(Persistence::Ephemeral),
            system_prompt: Some(system_prompt),
            model_id,
            seed,
            tool_filter: None,
        },
    )
    .await?;

    let user_text = format_user_turn(&message, &prompt_text);
    let user_turn = SeedMessage {
        role: Role::User,
        content: vec![InputContent::Text {
            text: user_text.clone(),
        }],
    };

    session
        .send(UserMessage {
            content: vec![InputContent::Text { text: user_text }],
        })
        .await?;

    let mut new_turns = vec![user_turn];
    new_turns.extend(
        stream_response(
            &api,
            message.chat.id,
            message.message_thread_id,
            &mut session,
        )
        .await?,
    );

    state.memory.append(key, new_turns, history_limit).await;
    Ok(())
}

async fn stream_response(
    api: &TelegramApi,
    chat_id: i64,
    thread_id: Option<i64>,
    session: &mut simply_daemon::DaemonSession,
) -> anyhow::Result<Vec<SeedMessage>> {
    let mut text_buffer = String::new();
    let mut turns = Vec::new();
    let mut sent_final = false;

    loop {
        match session.recv().await {
            Ok(DaemonEvent::TextDelta(_)) => {}
            Ok(DaemonEvent::AssistantContent(content)) => match content {
                ContentBlock::Text { text } => text_buffer.push_str(&text),
                ContentBlock::Image { mime_type, .. } => {
                    api.send_message(
                        chat_id,
                        thread_id,
                        format!("Assistant produced image output ({mime_type}); Telegram media forwarding is not implemented yet."),
                    )
                    .await?;
                }
                ContentBlock::Audio { mime_type, .. } => {
                    api.send_message(
                        chat_id,
                        thread_id,
                        format!("Assistant produced audio output ({mime_type}); Telegram media forwarding is not implemented yet."),
                    )
                    .await?;
                }
                _ => {}
            },
            Ok(DaemonEvent::ToolCall {
                id,
                name,
                arguments,
            }) => {
                tracing::info!(tool = %name, "daemon requested tool");
                turns.push(SeedMessage {
                    role: Role::Assistant,
                    content: vec![InputContent::ToolCall(ToolCall {
                        id,
                        name,
                        arguments,
                        extra: serde_json::Value::Null,
                    })],
                });
            }
            Ok(DaemonEvent::ToolResult { id, result }) => {
                turns.push(SeedMessage {
                    role: Role::User,
                    content: vec![InputContent::ToolResult(ToolResult {
                        tool_call_id: id,
                        content: tool_result_content(result),
                    })],
                });
            }
            Ok(DaemonEvent::TurnComplete) => {
                if !text_buffer.trim().is_empty() {
                    send_pages(api, chat_id, thread_id, &text_buffer).await?;
                    turns.push(SeedMessage {
                        role: Role::Assistant,
                        content: vec![InputContent::Text {
                            text: text_buffer.clone(),
                        }],
                    });
                    sent_final = true;
                }
                break;
            }
            Ok(DaemonEvent::Error(e)) => return Err(anyhow::anyhow!("daemon error: {e}")),
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "event stream lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }

    if !sent_final && !text_buffer.trim().is_empty() {
        send_pages(api, chat_id, thread_id, &text_buffer).await?;
        turns.push(SeedMessage {
            role: Role::Assistant,
            content: vec![InputContent::Text { text: text_buffer }],
        });
    }

    Ok(turns)
}

fn tool_result_content(result: serde_json::Value) -> Vec<ToolResultContent> {
    serde_json::from_value::<Vec<ToolResultContent>>(result.clone())
        .unwrap_or_else(|_| vec![ToolResultContent::text(result.to_string())])
}

async fn call_tool_text(
    state: &AppState,
    message: &Message,
    tool_name: &str,
    args: serde_json::Value,
) -> anyhow::Result<String> {
    let ctx = state.ctx_for_message(message).await;
    let request = CallToolRequestParams::new(tool_name.to_string())
        .with_arguments(args.as_object().cloned().unwrap_or_default());
    let result = state.daemon.mcp().call_tool_direct(&ctx, request).await?;

    let text = call_result_text(&result);
    if result.is_error.unwrap_or(false) {
        anyhow::bail!(text);
    }
    Ok(text)
}

fn call_result_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match &content.raw {
            RawContent::Text(text) => Some(text.text.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn resolve_model(state: &AppState) -> Option<String> {
    let model_id = state
        .config
        .telegram
        .model_id
        .clone()
        .filter(|id| !id.is_empty())?;

    match state.daemon.model().list_models().await {
        Ok(models) => {
            let exists = models.iter().any(|model| {
                model.id.to_string() == model_id
                    && model
                        .definition
                        .capabilities
                        .contains(&ModelCapability::Text)
            });
            if exists {
                Some(model_id)
            } else {
                tracing::warn!(model = %model_id, "configured Telegram model not found or not text-capable");
                None
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not validate configured Telegram model");
            Some(model_id)
        }
    }
}

fn build_system_prompt(message: &Message, me: &User) -> String {
    let user_id = message
        .from
        .as_ref()
        .map(|user| user.id.to_string())
        .unwrap_or_else(|| "(unknown)".to_string());
    let username = me
        .username
        .as_ref()
        .map(|username| format!("@{username}"))
        .unwrap_or_else(|| "(unknown username)".to_string());
    let thread = message
        .message_thread_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "(none)".to_string());

    format!(
        "\
You are the Simply Telegram assistant.

## Current Telegram context
- Bot username: {username}
- Chat ID: {chat_id}
- Chat type: {chat_type}
- Message thread ID: {thread}
- User ID: {user_id}

When using Telegram tools, use these IDs directly. Do not invent chat IDs or user IDs.

## Conversation history
The prior messages provided in this session are the bot's in-process memory for this Telegram chat. Telegram does not expose arbitrary chat history to bots, so treat the provided history as the available conversation memory.

Messages from users are prefixed with their Telegram display name and user ID.

Be helpful, concise, and conversational.",
        chat_id = message.chat.id,
        chat_type = message.chat.kind.as_str(),
    )
}

fn format_user_turn(message: &Message, text: &str) -> String {
    match &message.from {
        Some(user) => format!(
            "{} (telegram user id {}) says: {}",
            user.display_name(),
            user.id,
            text
        ),
        None => format!("Unknown Telegram sender says: {text}"),
    }
}

fn is_allowed(config: &config::TelegramConfig, message: &Message) -> bool {
    let chat_allowed = config.telegram.allowed_chat_ids.is_empty()
        || config.telegram.allowed_chat_ids.contains(&message.chat.id);
    if !chat_allowed {
        return false;
    }

    config.telegram.allowed_user_ids.is_empty()
        || message
            .from
            .as_ref()
            .is_some_and(|user| config.telegram.allowed_user_ids.contains(&user.id))
}

fn should_respond(
    config: &config::TelegramConfig,
    me: &User,
    message: &Message,
    text: &str,
) -> bool {
    if message.chat.is_private() {
        return true;
    }
    if !message.chat.is_group() || !config.respond_in_groups() {
        return false;
    }
    is_bot_mentioned(text, me) || is_reply_to_bot(message, me)
}

fn is_bot_mentioned(text: &str, me: &User) -> bool {
    let Some(username) = &me.username else {
        return false;
    };
    text.to_lowercase()
        .contains(&format!("@{}", username.to_lowercase()))
}

fn is_reply_to_bot(message: &Message, me: &User) -> bool {
    message
        .reply_to_message
        .as_ref()
        .and_then(|reply| reply.from.as_ref())
        .is_some_and(|user| user.id == me.id)
}

fn strip_bot_mention(text: &str, me: &User) -> String {
    let Some(username) = &me.username else {
        return text.to_string();
    };
    let mention = format!("@{username}");
    text.replace(&mention, "")
}

#[derive(Debug)]
struct ParsedCommand {
    name: String,
    mention: Option<String>,
    args: String,
}

fn parse_command(text: &str) -> Option<ParsedCommand> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }

    let first_len = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let token = &trimmed[..first_len];
    let args = trimmed[first_len..].trim().to_string();
    let command = token.trim_start_matches('/');
    let mut parts = command.splitn(2, '@');
    let name = parts.next()?.to_lowercase();
    if name.is_empty() {
        return None;
    }
    let mention = parts.next().map(|value| value.to_lowercase());

    Some(ParsedCommand {
        name,
        mention,
        args,
    })
}

fn command_is_for_bot(command: &ParsedCommand, me: &User, message: &Message) -> bool {
    match &command.mention {
        Some(mention) => me
            .username
            .as_ref()
            .is_some_and(|username| username.eq_ignore_ascii_case(mention)),
        None => message.chat.is_private(),
    }
}

async fn send_pages(
    api: &TelegramApi,
    chat_id: i64,
    thread_id: Option<i64>,
    text: &str,
) -> anyhow::Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    for page in paginate_text(text, TELEGRAM_MESSAGE_LIMIT) {
        api.send_message(chat_id, thread_id, page).await?;
    }
    Ok(())
}

fn paginate_text(text: &str, max_chars: usize) -> Vec<String> {
    let mut pages = Vec::new();
    let mut current = String::new();

    for line in text.lines() {
        let line_chars = line.chars().count();
        if line_chars > max_chars {
            if !current.is_empty() {
                pages.push(std::mem::take(&mut current));
            }
            let mut chunk = String::new();
            for ch in line.chars() {
                if chunk.chars().count() >= max_chars {
                    pages.push(std::mem::take(&mut chunk));
                }
                chunk.push(ch);
            }
            if !chunk.is_empty() {
                pages.push(chunk);
            }
            continue;
        }

        let separator = if current.is_empty() { 0 } else { 1 };
        if current.chars().count() + separator + line_chars > max_chars {
            pages.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }

    if !current.is_empty() {
        pages.push(current);
    }
    pages
}

fn extract_doc_id(input: &str) -> String {
    if input.contains("docs.google.com") {
        if let Some(start) = input.find("/d/") {
            let id_start = start + 3;
            let id_end = input[id_start..]
                .find('/')
                .map(|i| id_start + i)
                .unwrap_or(input.len());
            return input[id_start..id_end].to_string();
        }
    }
    input.to_string()
}

fn help_text(me: &User) -> String {
    let mention = me
        .username
        .as_ref()
        .map(|username| format!("@{username}"))
        .unwrap_or_else(|| "the bot".to_string());
    format!(
        "\
Send me a message in a private chat, or mention {mention} in a group.

Commands:
/help - Show this help
/reset - Clear this chat's in-process conversation memory
/whoami - Show your Telegram IDs for allowlist setup
/google auth - Connect Google for Google Docs tools
/google status - Check Google connection
/google import <doc-url-or-id> - Import a Google Doc"
    )
}

fn default_commands() -> Vec<BotCommand> {
    vec![
        BotCommand::new("help", "Show help"),
        BotCommand::new("reset", "Clear conversation memory"),
        BotCommand::new("whoami", "Show Telegram IDs"),
        BotCommand::new("google", "Google Docs auth, status, and import"),
    ]
}

async fn connect_daemon(settings: &config::Settings) -> anyhow::Result<Arc<dyn Daemon>> {
    #[cfg(feature = "embedded")]
    {
        static HANDLE: tokio::sync::OnceCell<simply_daemon::net::DaemonHandle> =
            tokio::sync::OnceCell::const_new();
        let handle =
            simply_daemon::net::connect_or_host(settings.daemon_port, "telegram", None).await?;
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
        let remote =
            simply_daemon_api::RemoteDaemon::connect_as(&url, "telegram", &secret, None).await?;
        Ok(remote)
    }
}

async fn register_skills(daemon: &Arc<dyn Daemon>, api: TelegramApi) -> anyhow::Result<()> {
    let gdocs = Arc::new(mcp_gdocs::GDocsSkill::new(Arc::clone(daemon)));
    daemon.register_skill(gdocs).await?;
    tracing::info!("GDocs skill registered with daemon");

    daemon
        .register_skill(Arc::new(skill::TelegramSkill::new(api)))
        .await?;
    tracing::info!("Telegram skill registered with daemon");
    Ok(())
}

fn setup_logging() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "telegram=debug,simply_daemon=info,mcp_gdocs=debug".into());

    if let Ok(log_path) = std::env::var("TELEGRAM_LOG_FILE") {
        let file = std::fs::File::create(&log_path).expect("failed to create log file");
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(file)
            .with_ansi(false)
            .init();
        eprintln!("Logging to {log_path}");
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    };
}
