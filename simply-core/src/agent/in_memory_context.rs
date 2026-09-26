//! In-memory conversation context — no storage dependency.

use anyhow::Result;
use async_trait::async_trait;
use llm::ChatMessage;

use super::context::{ConversationContext, MessagesGuard};

/// A storage-free context that keeps all messages in memory.
pub struct InMemoryContext {
    messages: Vec<ChatMessage>,
    system_prompt: Option<String>,
}

impl Default for InMemoryContext {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryContext {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            system_prompt: None,
        }
    }

    pub fn with_system_prompt(prompt: impl Into<String>) -> Self {
        Self {
            messages: Vec::new(),
            system_prompt: Some(prompt.into()),
        }
    }
}

#[async_trait]
impl ConversationContext for InMemoryContext {
    async fn messages(&mut self) -> Result<MessagesGuard<'_>> {
        Ok(MessagesGuard::new(&self.messages))
    }

    fn len(&self) -> usize {
        self.messages.len()
    }

    fn add(&mut self, message: ChatMessage) {
        self.messages.push(message);
    }

    fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }
}
