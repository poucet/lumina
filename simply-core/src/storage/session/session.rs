//! DB-agnostic Session — a persistent ConversationContext backed by storage.

use anyhow::Result;
use async_trait::async_trait;
use llm::{ChatMessage, ChatPayload, ContentBlock};
use std::sync::Arc;

use crate::agent::{ConversationContext, MessagesGuard};
use crate::storage::coordinator::{AppendState, StorageCoordinator};
use crate::storage::ids::ConversationId;
use crate::storage::traits::StorageTypes;

use super::types::{ResolvedContent, ResolvedMessage};

pub struct Session<S: StorageTypes> {
    coordinator: Arc<StorageCoordinator<S>>,
    conversation_id: ConversationId,
    resolved_cache: Vec<ResolvedMessage>,
    llm_cache: Vec<ChatMessage>,
    llm_cache_valid: bool,
    pending: Vec<ChatMessage>,
    append_state: Option<AppendState>,
}

impl<S: StorageTypes> Session<S> {
    /// Open a session for an existing conversation, loading history from storage.
    pub async fn open(
        coordinator: Arc<StorageCoordinator<S>>,
        conversation_id: ConversationId,
    ) -> Result<Self> {
        let resolved_cache = coordinator.open_session(&conversation_id).await?;
        Ok(Self {
            coordinator,
            conversation_id,
            resolved_cache,
            llm_cache: Vec::new(),
            llm_cache_valid: false,
            pending: Vec::new(),
            append_state: None,
        })
    }

    /// Create a new session for a new conversation.
    pub fn new(
        coordinator: Arc<StorageCoordinator<S>>,
        conversation_id: ConversationId,
    ) -> Self {
        Self {
            coordinator,
            conversation_id,
            resolved_cache: Vec::new(),
            llm_cache: Vec::new(),
            llm_cache_valid: false,
            pending: Vec::new(),
            append_state: None,
        }
    }
}

#[async_trait]
impl<S: StorageTypes> ConversationContext for Session<S> {
    async fn messages(&mut self) -> Result<MessagesGuard<'_>> {
        let needs_rebuild = !self.llm_cache_valid ||
            self.llm_cache.len() != self.resolved_cache.len() + self.pending.len();

        if needs_rebuild {
            self.llm_cache.clear();
            self.llm_cache.reserve(self.resolved_cache.len() + self.pending.len());

            for msg in &self.resolved_cache {
                self.llm_cache.push(resolved_message_to_chat_message(msg));
            }

            for msg in &self.pending {
                self.llm_cache.push(msg.clone());
            }

            self.llm_cache_valid = self.pending.is_empty();
        }

        Ok(MessagesGuard::new(&self.llm_cache))
    }

    fn len(&self) -> usize {
        self.resolved_cache.len() + self.pending.len()
    }

    fn add(&mut self, message: ChatMessage) {
        self.pending.push(message);
    }

    async fn commit(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }

        let messages = std::mem::take(&mut self.pending);

        for msg in messages {
            let (resolved, new_state) = self.coordinator
                .append_message(&self.conversation_id, msg, self.append_state.take())
                .await?;
            self.resolved_cache.push(resolved);
            self.append_state = Some(new_state);
        }

        self.llm_cache_valid = false;
        Ok(())
    }
}

fn resolved_message_to_chat_message(msg: &ResolvedMessage) -> ChatMessage {
    let blocks: Vec<ContentBlock> = msg.content
        .iter()
        .map(|item| match item {
            ResolvedContent::Text { text } => ContentBlock::Text { text: text.clone() },
            ResolvedContent::Asset { asset_id, mime_type, resolved, .. } => {
                resolved.clone().unwrap_or_else(|| {
                    ContentBlock::Text { text: format!("[Asset: {} ({})]", asset_id, mime_type) }
                })
            }
            ResolvedContent::Entity { entity_id, resolved } => {
                resolved.clone().unwrap_or_else(|| {
                    ContentBlock::EntityRef { id: entity_id.to_string() }
                })
            }
            ResolvedContent::ToolCall(call) => ContentBlock::ToolCall(call.clone()),
            ResolvedContent::ToolResult(result) => ContentBlock::ToolResult(result.clone()),
        })
        .collect();
    ChatMessage::new(msg.role, ChatPayload::new(blocks))
}
