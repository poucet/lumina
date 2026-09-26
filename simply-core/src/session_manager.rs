//! Unified session manager.
//!
//! The core loop: receive user message → run agent → commit → stream events.
//! Storage is handled by the ConversationContext implementation (Session commits
//! to DB, InMemoryContext is a no-op).
//! Input conversion (InputContent → ContentBlock) happens in the caller.

use std::sync::Arc;

use llm::{ChatMessage, ChatModel, ChatPayload, ContentBlock, Role, ToolResultContent};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::agent::tool_agent::AgentStreamEvent;
use crate::agent::{
    Agent, ConversationContext, ExecutionContext, InMemoryContext, ToolAgent, ToolService,
};
use crate::storage::coordinator::StorageCoordinator;
use crate::storage::ids::ConversationId;
use crate::storage::session::Session;
use crate::storage::traits::StorageTypes;
use crate::storage::EntityResolver;

// ============================================================================
// Persistence
// ============================================================================

/// How a session persists its data.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Persistence {
    Ephemeral,
    Persistent { conversation_id: ConversationId },
}

// ============================================================================
// Events
// ============================================================================

#[derive(Debug, Clone)]
pub enum SessionEvent {
    UserMessageAdded(ChatMessage),
    TextDelta(String),
    ContentBlock(ContentBlock),
    ToolCallStart {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolCallResult {
        id: String,
        content: Vec<ToolResultContent>,
    },
    AssistantMessage(ChatMessage),
    TurnComplete { new_messages: Vec<ChatMessage> },
    Error(String),
    ModelChanged(String),
}

// ============================================================================
// Commands
// ============================================================================

pub enum SessionCommand {
    SendMessage { content: Vec<ContentBlock> },
    SetModel(Arc<dyn ChatModel + Send + Sync>),
}

pub type SessionEventSender = mpsc::UnboundedSender<(String, SessionEvent)>;

// ============================================================================
// SessionManager
// ============================================================================

pub struct SessionManager {
    session_id: String,
    cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    _task: JoinHandle<()>,
}

impl SessionManager {
    /// Create a session manager with a pre-built context.
    pub fn new<C: ConversationContext + 'static>(
        session_id: impl Into<String>,
        context: C,
        model: Arc<dyn ChatModel + Send + Sync>,
        tools: Arc<dyn ToolService>,
        entity_resolver: Arc<dyn EntityResolver>,
        execution_context: ExecutionContext,
        event_tx: SessionEventSender,
    ) -> Self {
        let session_id = session_id.into();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let inner = Arc::new(Inner {
            session_id: session_id.clone(),
            context: Mutex::new(context),
            model: Mutex::new(model),
            tools,
            entity_resolver,
            execution_context,
            event_tx,
        });

        let task = tokio::spawn(Inner::run(inner, cmd_rx));

        Self { session_id, cmd_tx, _task: task }
    }

    /// Create a session from persistence mode, seed messages, and optional system prompt.
    ///
    /// For `Persistent`: opens the existing conversation from storage (loads history).
    /// For `Ephemeral`: creates an in-memory context.
    #[allow(clippy::too_many_arguments)] // one flat constructor per session; callers pass each piece explicitly
    pub async fn create<S: StorageTypes + 'static>(
        session_id: impl Into<String>,
        persistence: Persistence,
        seed: Vec<ChatMessage>,
        system_prompt: Option<String>,
        model: Arc<dyn ChatModel + Send + Sync>,
        tools: Arc<dyn ToolService>,
        coordinator: Arc<StorageCoordinator<S>>,
        entity_resolver: Arc<dyn EntityResolver>,
        execution_context: ExecutionContext,
        event_tx: SessionEventSender,
    ) -> anyhow::Result<Self> {
        match persistence {
            Persistence::Ephemeral => {
                let mut ctx = match system_prompt {
                    Some(prompt) => InMemoryContext::with_system_prompt(prompt),
                    None => InMemoryContext::new(),
                };
                for msg in seed {
                    ctx.add(msg);
                }
                Ok(Self::new(session_id, ctx, model, tools, entity_resolver, execution_context, event_tx))
            }
            Persistence::Persistent { conversation_id } => {
                let mut ctx = Session::open(coordinator, conversation_id).await?;
                for msg in seed {
                    ctx.add(msg);
                }
                Ok(Self::new(session_id, ctx, model, tools, entity_resolver, execution_context, event_tx))
            }
        }
    }

    pub fn send_message(&self, content: Vec<ContentBlock>) {
        let _ = self.cmd_tx.send(SessionCommand::SendMessage { content });
    }

    pub fn set_model(&self, model: Arc<dyn ChatModel + Send + Sync>) {
        let _ = self.cmd_tx.send(SessionCommand::SetModel(model));
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

// ============================================================================
// Inner
// ============================================================================

struct Inner<C: ConversationContext> {
    session_id: String,
    context: Mutex<C>,
    model: Mutex<Arc<dyn ChatModel + Send + Sync>>,
    tools: Arc<dyn ToolService>,
    entity_resolver: Arc<dyn EntityResolver>,
    execution_context: ExecutionContext,
    event_tx: SessionEventSender,
}

impl<C: ConversationContext + 'static> Inner<C> {
    fn emit(&self, event: SessionEvent) {
        let _ = self.event_tx.send((self.session_id.clone(), event));
    }

    async fn run(self: Arc<Self>, mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>) {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                SessionCommand::SendMessage { content } => {
                    self.handle_send_message(content).await;
                }
                SessionCommand::SetModel(model) => {
                    let model_id = model.id().to_string();
                    *self.model.lock().await = model;
                    self.emit(SessionEvent::ModelChanged(model_id));
                }
            }
        }
    }

    async fn handle_send_message(&self, content: Vec<ContentBlock>) {
        if content.is_empty() {
            self.emit(SessionEvent::Error("empty message".into()));
            return;
        }

        let user_msg = ChatMessage::user(ChatPayload::new(content));
        let msg_count_before = {
            let mut ctx = self.context.lock().await;
            let count = ctx.len();
            ctx.add(user_msg.clone());
            count
        };
        self.emit(SessionEvent::UserMessageAdded(user_msg));

        let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<AgentStreamEvent>();

        let session_id = self.session_id.clone();
        let event_tx = self.event_tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(evt) = stream_rx.recv().await {
                let session_event = match evt {
                    AgentStreamEvent::TextDelta(text) => SessionEvent::TextDelta(text),
                    AgentStreamEvent::ContentBlock(block) => SessionEvent::ContentBlock(block),
                    AgentStreamEvent::ToolCallStart { id, name, arguments } => {
                        SessionEvent::ToolCallStart { id, name, arguments }
                    }
                    AgentStreamEvent::ToolCallResult { id, content } => {
                        SessionEvent::ToolCallResult { id, content }
                    }
                };
                let _ = event_tx.send((session_id.clone(), session_event));
            }
        });

        let model = self.model.lock().await.clone();
        let agent = ToolAgent::new(
            Arc::clone(&self.tools),
            10,
            Arc::clone(&self.entity_resolver),
            self.execution_context.clone(),
        )
        .with_stream_sink(stream_tx);

        let result = {
            let mut ctx = self.context.lock().await;
            agent.execute_stream(&mut *ctx, model).await
        };

        drop(agent);
        let _ = forwarder.await;

        match result {
            Ok(_) => {
                let mut ctx = self.context.lock().await;

                let new_messages = match ctx.messages().await {
                    Ok(all) => all
                        .iter()
                        .skip(msg_count_before + 1)
                        .cloned()
                        .collect::<Vec<_>>(),
                    Err(_) => vec![],
                };

                if let Err(e) = ctx.commit().await {
                    self.emit(SessionEvent::Error(format!("Commit error: {e}")));
                    return;
                }

                for msg in &new_messages {
                    if msg.role != Role::User {
                        self.emit(SessionEvent::AssistantMessage(msg.clone()));
                    }
                }

                self.emit(SessionEvent::TurnComplete { new_messages });
            }
            Err(e) => {
                self.emit(SessionEvent::Error(e.to_string()));
            }
        }
    }
}
