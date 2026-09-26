//! Storage coordinator - orchestrates multi-store operations
//!
//! The coordinator handles operations that require multiple stores working together:
//! - Converting LLM content to stored references (text, assets, documents)
//! - Session management (conversation + turn stores)
//! - Content resolution (text + asset + blob stores)
//!
//! For single-store operations, access stores directly via `Stores` trait.
//! Implements `ContentResolver` for resolving text and asset refs. Entity
//! refs (`@mention`s in chat) are resolved separately by `EntityResolver`
//! in `storage::entity_resolver`.

use anyhow::Result;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use llm::{ContentBlock, Role};
use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::storage::content::{ContentResolver, InputContent, StoredContent};
use crate::storage::ids::{AssetId, ContentBlockId, ConversationId, EntityId, SpanId, TurnId, UserId};
use crate::storage::session::{ResolvedContent, ResolvedMessage};
use crate::storage::traits::{
    AssetStore, BlobStore, EntityStore, StorageTypes, StoredEntity, Stores, TextStore, TurnStore,
};
use crate::storage::types::{
    Asset, BlobHash, ContentBlock as ContentBlockData, ContentOrigin, EntityType, OriginKind,
    RelationType, TurnWithContent,
};

/// Opaque state for batching messages into the same turn during `append_message`.
pub struct AppendState {
    turn_id: TurnId,
    span_id: SpanId,
    role: Role,
}

/// Coordinates storage across all store types.
///
/// Generic over `S: StorageTypes` which bundles all storage type associations.
/// Takes a `Stores<S>` implementation to access individual stores.
pub struct StorageCoordinator<S: StorageTypes> {
    blob_store: Arc<S::Blob>,
    asset_store: Arc<S::Asset>,
    content_block_store: Arc<S::Text>,
    entity_store: Arc<S::Entity>,
    turn_store: Arc<S::Turn>,
    _marker: PhantomData<S>,
}

impl<S: StorageTypes> StorageCoordinator<S> {
    /// Create a new storage coordinator from a Stores implementation
    pub fn from_stores(stores: &impl Stores<S>) -> Self {
        Self {
            blob_store: stores.blob(),
            asset_store: stores.asset(),
            content_block_store: stores.text(),
            entity_store: stores.entity(),
            turn_store: stores.turn(),
            _marker: PhantomData,
        }
    }

    /// Create a new storage coordinator from individual store instances
    pub fn new(
        blob_store: Arc<S::Blob>,
        asset_store: Arc<S::Asset>,
        content_block_store: Arc<S::Text>,
        entity_store: Arc<S::Entity>,
        turn_store: Arc<S::Turn>,
    ) -> Self {
        Self {
            blob_store,
            asset_store,
            content_block_store,
            entity_store,
            turn_store,
            _marker: PhantomData,
        }
    }

    /// Store a single ContentBlock and return its StoredContent reference
    ///
    /// - Text is stored in content_blocks and converted to TextRef
    /// - Inline images/audio are stored in blob/assets and converted to AssetRef
    /// - EntityRef, ToolCall, ToolResult pass through
    pub async fn store_content_block(
        &self,
        block: ContentBlock,
        origin: OriginKind,
    ) -> Result<StoredContent> {
        match block {
            ContentBlock::Text { text } => {
                let content_block =
                    ContentBlockData::plain(&text).with_origin(ContentOrigin::from_kind(origin));
                let content_block_id = self.content_block_store.store(content_block).await?;
                Ok(StoredContent::text_ref(content_block_id))
            }
            ContentBlock::Image { data, mime_type } => {
                let asset_id = self.store_asset(&data, &mime_type).await?;
                Ok(StoredContent::asset_ref(asset_id, mime_type))
            }
            ContentBlock::Audio { data, mime_type } => {
                let asset_id = self.store_asset(&data, &mime_type).await?;
                Ok(StoredContent::asset_ref(asset_id, mime_type))
            }
            ContentBlock::EntityRef { id, .. } => Ok(StoredContent::entity_ref(id)),
            ContentBlock::ToolCall(call) => Ok(StoredContent::ToolCall(call)),
            ContentBlock::ToolResult(result) => Ok(StoredContent::ToolResult(result)),
        }
    }

    /// Convert InputContent from UI to StoredContent refs
    ///
    /// - Text is stored in content_blocks → TextRef
    /// - Image/Audio base64 data is stored in blob/assets → AssetRef
    /// - EntityRef passes through
    /// - AssetRef passes through (already stored)
    pub async fn store_input_content(
        &self,
        content: Vec<InputContent>,
        origin: OriginKind,
    ) -> Result<Vec<StoredContent>> {
        let mut stored = Vec::with_capacity(content.len());

        for item in content {
            let stored_item = match item {
                InputContent::Text { text } => {
                    let content_block =
                        ContentBlockData::plain(&text).with_origin(ContentOrigin::from_kind(origin));
                    let content_block_id = self.content_block_store.store(content_block).await?;
                    StoredContent::text_ref(content_block_id)
                }
                InputContent::Image { data, mime_type } => {
                    let asset_id = self.store_asset(&data, &mime_type).await?;
                    StoredContent::asset_ref(asset_id, mime_type)
                }
                InputContent::Audio { data, mime_type } => {
                    let asset_id = self.store_asset(&data, &mime_type).await?;
                    StoredContent::asset_ref(asset_id, mime_type)
                }
                InputContent::EntityRef { id } => StoredContent::entity_ref(id),
                InputContent::AssetRef { asset_id, mime_type } => {
                    StoredContent::asset_ref(asset_id, mime_type)
                }
                InputContent::ToolCall(call) => StoredContent::ToolCall(call),
                InputContent::ToolResult(result) => StoredContent::ToolResult(result),
            };
            stored.push(stored_item);
        }

        Ok(stored)
    }

    /// Decode base64 data, store in blob storage, register in asset storage,
    /// and return the asset ID.
    pub async fn store_asset(&self, base64_data: &str, mime_type: &str) -> Result<AssetId> {
        let bytes = STANDARD.decode(base64_data)?;
        let blob_hash = self.blob_store.store(&bytes).await?;
        let asset = Asset::new(blob_hash, mime_type, bytes.len() as i64);
        self.asset_store.create_asset(asset).await
    }

    /// Get blob data by hash
    pub async fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>> {
        self.blob_store.get(hash).await
    }

    // ========== Turn/Span Methods ==========

    /// Create a new turn (without span or selection).
    pub async fn create_turn(&self, role: llm::Role) -> Result<TurnId> {
        let turn = self.turn_store.create_turn(role).await?;
        Ok(turn.id)
    }

    /// Create a span at a turn and select it in the conversation.
    pub async fn create_and_select_span(
        &self,
        conversation_id: &ConversationId,
        turn_id: &TurnId,
        model_id: Option<&str>,
    ) -> Result<SpanId> {
        let span = self.turn_store.create_span(turn_id, model_id).await?;
        self.turn_store.select_span(conversation_id, turn_id, &span.id).await?;
        Ok(span.id)
    }

    /// Create a turn with an initial span and select it.
    pub async fn create_turn_with_span(
        &self,
        conversation_id: &ConversationId,
        role: Role,
    ) -> Result<(TurnId, SpanId)> {
        let turn = self.turn_store.create_turn(role).await?;
        let span = self.turn_store.create_span(&turn.id, None).await?;
        self.turn_store.select_span(conversation_id, &turn.id, &span.id).await?;
        Ok((turn.id, span.id))
    }

    /// Append a message to a conversation, creating a new turn+span if the role
    /// differs from the previous message. Pass the returned `AppendState` back
    /// on subsequent calls to batch messages into the same turn.
    pub async fn append_message(
        &self,
        conversation_id: &ConversationId,
        msg: llm::ChatMessage,
        state: Option<AppendState>,
    ) -> Result<(ResolvedMessage, AppendState)> {
        let role = msg.role;
        let needs_new_turn = state.as_ref().is_none_or(|s| s.role != role);

        let (turn_id, span_id) = if needs_new_turn {
            self.create_turn_with_span(conversation_id, role).await?
        } else {
            let s = state.unwrap();
            (s.turn_id, s.span_id)
        };

        let origin = OriginKind::from(role);
        let resolved = self
            .add_message(&span_id, &turn_id, role, msg.payload.content, origin)
            .await?;

        Ok((resolved, AppendState { turn_id, span_id, role }))
    }

    // ========== Session Methods ==========

    /// Open a session for a conversation.
    ///
    /// This method handles the multi-store coordination of:
    /// 1. Getting the conversation entity
    /// 2. Loading the conversation path (turns with content)
    /// 3. Resolving stored content to resolved messages
    ///
    /// Returns resolved messages for Session construction.
    pub async fn open_session(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Vec<ResolvedMessage>> {
        // Verify conversation exists
        let _ = self.entity_store
            .get_entity(conversation_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Conversation not found: {}", conversation_id))?;

        // Load conversation path and resolve content
        let path = self.turn_store.get_conversation_path(conversation_id).await?;
        self.resolve_path(&path).await
    }

    /// Create a new conversation entity.
    ///
    /// Returns the ConversationId (EntityId) for further operations.
    /// The conversation starts empty - use add_message to add turns.
    pub async fn create_conversation(
        &self,
        user_id: &UserId,
        name: Option<&str>,
    ) -> Result<ConversationId> {
        // Create conversation entity
        let conversation_id = self.entity_store
            .create_entity(EntityType::conversation(), Some(user_id))
            .await?;

        // Set name if provided
        if let Some(n) = name {
            let mut entity = self.entity_store
                .get_entity(&conversation_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Just-created entity not found"))?;
            entity.name = Some(n.to_string());
            self.entity_store.update_entity(&conversation_id, &entity).await?;
        }

        Ok(conversation_id)
    }

    /// Fork a conversation at a specific turn.
    ///
    /// Creates a new conversation entity, copies selections up to and including
    /// the fork turn, and links it to the original via entity_relations.
    ///
    /// # Arguments
    /// * `conversation_id` - The conversation to fork from
    /// * `at_turn_id` - The turn at which to fork (fork includes this turn)
    /// * `name` - Optional name for the forked conversation
    ///
    /// # Returns
    /// The new conversation ID
    pub async fn fork_conversation(
        &self,
        conversation_id: &ConversationId,
        at_turn_id: &TurnId,
        name: Option<&str>,
    ) -> Result<ConversationId> {
        use crate::storage::types::RelationType;

        // Get the original conversation to copy user_id
        let original_entity = self.entity_store
            .get_entity(conversation_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Conversation not found: {}", conversation_id))?;

        // Create new conversation entity
        let new_conversation_id = self.entity_store
            .create_entity(EntityType::conversation(), original_entity.user_id.as_ref())
            .await?;

        // Set name on new conversation
        if let Some(n) = name {
            let mut new_entity = self.entity_store
                .get_entity(&new_conversation_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Just-created entity not found"))?;
            new_entity.name = Some(n.to_string());
            self.entity_store.update_entity(&new_conversation_id, &new_entity).await?;
        }

        // Copy selections from original to new (include the fork turn)
        self.turn_store
            .copy_selections(conversation_id, &new_conversation_id, at_turn_id, true)
            .await?;

        // Add forked_from relation
        self.entity_store
            .add_relation(
                &new_conversation_id,
                conversation_id,
                RelationType::conversation_forked_from(),
                None,
                Some(serde_json::json!({
                    "at_turn_id": at_turn_id.as_str()
                })),
            )
            .await?;

        Ok(new_conversation_id)
    }

    /// Get conversations forked from a given conversation.
    pub async fn get_forked_conversations(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Vec<(ConversationId, TurnId)>> {
        use crate::storage::types::RelationType;

        let relations = self.entity_store
            .get_relations_to(conversation_id, Some(&RelationType::conversation_forked_from()))
            .await?;

        let mut result = Vec::new();
        for (forked_id, relation) in relations {
            let at_turn_id = relation.metadata
                .as_ref()
                .and_then(|m| m.get("at_turn_id"))
                .and_then(|v| v.as_str())
                .map(TurnId::from_string);

            if let Some(turn_id) = at_turn_id {
                result.push((forked_id, turn_id));
            }
        }

        Ok(result)
    }

    /// Spawn a subconversation linked to a parent conversation.
    ///
    /// Creates a new conversation entity and links it to the parent via
    /// entity_relations with RelationType::conversation_spawned_from(). The metadata
    /// records which turn in the parent triggered the spawn.
    ///
    /// # Arguments
    /// * `parent_conversation_id` - The parent conversation entity ID
    /// * `user_id` - The user who owns the subconversation
    /// * `at_turn_id` - The turn in the parent where the spawn was triggered
    /// * `at_span_id` - Optional: the specific span at that turn
    /// * `name` - Optional name for the subconversation
    ///
    /// # Returns
    /// The new subconversation's ConversationId
    pub async fn spawn_subconversation(
        &self,
        parent_conversation_id: &ConversationId,
        user_id: &UserId,
        at_turn_id: &TurnId,
        at_span_id: Option<&SpanId>,
        name: Option<&str>,
    ) -> Result<ConversationId> {
        use crate::storage::types::RelationType;

        // Create the subconversation
        let sub_conversation_id = self.create_conversation(user_id, name).await?;

        // Build spawn metadata
        let mut metadata = serde_json::json!({
            "at_turn_id": at_turn_id.as_str()
        });
        if let Some(span_id) = at_span_id {
            metadata["at_span_id"] = serde_json::Value::String(span_id.as_str().to_string());
        }

        // Link subconversation → parent with spawned_from relation
        self.entity_store
            .add_relation(
                &sub_conversation_id,
                parent_conversation_id,
                RelationType::conversation_spawned_from(),
                None,
                Some(metadata),
            )
            .await?;

        Ok(sub_conversation_id)
    }

    /// Get the parent conversation for a subconversation.
    ///
    /// Returns None if the conversation has no parent (is not a subconversation).
    pub async fn get_parent_conversation(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<(ConversationId, TurnId, Option<SpanId>)>> {
        use crate::storage::types::RelationType;

        let relations = self.entity_store
            .get_relations_from(conversation_id, Some(&RelationType::conversation_spawned_from()))
            .await?;

        if let Some((parent_id, relation)) = relations.into_iter().next() {
            let at_turn_id = relation.metadata
                .as_ref()
                .and_then(|m| m.get("at_turn_id"))
                .and_then(|v| v.as_str())
                .map(TurnId::from_string)
                .ok_or_else(|| anyhow::anyhow!("spawned_from relation missing at_turn_id"))?;

            let at_span_id = relation.metadata
                .as_ref()
                .and_then(|m| m.get("at_span_id"))
                .and_then(|v| v.as_str())
                .map(SpanId::from_string);

            Ok(Some((parent_id, at_turn_id, at_span_id)))
        } else {
            Ok(None)
        }
    }

    /// List all subconversations spawned from a parent conversation.
    pub async fn list_subconversations(
        &self,
        parent_conversation_id: &ConversationId,
    ) -> Result<Vec<(ConversationId, TurnId, Option<SpanId>)>> {
        use crate::storage::types::RelationType;

        let relations = self.entity_store
            .get_relations_to(parent_conversation_id, Some(&RelationType::conversation_spawned_from()))
            .await?;

        let mut result = Vec::new();
        for (sub_id, relation) in relations {
            let at_turn_id = relation.metadata
                .as_ref()
                .and_then(|m| m.get("at_turn_id"))
                .and_then(|v| v.as_str())
                .map(TurnId::from_string);

            let at_span_id = relation.metadata
                .as_ref()
                .and_then(|m| m.get("at_span_id"))
                .and_then(|v| v.as_str())
                .map(SpanId::from_string);

            if let Some(turn_id) = at_turn_id {
                result.push((sub_id, turn_id, at_span_id));
            }
        }

        Ok(result)
    }

    /// Get the final result text from a subconversation.
    ///
    /// Returns the text content of the last assistant message in the subconversation.
    /// Returns None if there are no messages or no text content.
    pub async fn get_subconversation_result(
        &self,
        subconversation_id: &ConversationId,
    ) -> Result<Option<String>> {
        // Verify conversation exists
        let _ = self.entity_store
            .get_entity(subconversation_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Subconversation not found: {}", subconversation_id))?;

        // Get the conversation path
        let path = self.turn_store.get_conversation_path(subconversation_id).await?;

        // Find the last assistant message
        for turn in path.into_iter().rev() {
            if turn.turn.role() == Role::Assistant {
                // Get the last message content from this turn
                for msg in turn.messages.into_iter().rev() {
                    if msg.message.role == Role::Assistant {
                        // Resolve and extract text from content
                        let resolved = self.resolve_stored_content(&msg.content).await?;
                        let text: Vec<String> = resolved
                            .into_iter()
                            .filter_map(|c| c.as_text().map(|t| t.to_string()))
                            .collect();

                        if !text.is_empty() {
                            return Ok(Some(text.join("\n")));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Link a subconversation's result back to the parent conversation.
    ///
    /// Creates a ToolResult message in the parent conversation's current span
    /// containing the subconversation's final result. This is used when a
    /// spawned agent completes and its result should appear in the parent flow.
    ///
    /// # Arguments
    /// * `subconversation_id` - The subconversation whose result to link
    /// * `parent_span_id` - The span in the parent to add the ToolResult to
    /// * `parent_turn_id` - The turn in the parent (for ResolvedMessage)
    /// * `tool_call_id` - The ID of the original ToolCall that spawned this
    /// * `tool_name` - The name of the tool (e.g., "spawn_agent")
    ///
    /// # Returns
    /// The resolved message that was added (for caching in Session)
    pub async fn link_subconversation_result(
        &self,
        subconversation_id: &ConversationId,
        parent_span_id: &SpanId,
        parent_turn_id: &TurnId,
        tool_call_id: &str,
        _tool_name: &str,
    ) -> Result<ResolvedMessage> {
        // Get the subconversation's result
        let result_text = self
            .get_subconversation_result(subconversation_id)
            .await?
            .unwrap_or_else(|| "(no result)".to_string());

        // Create a ToolResult that includes both the result and a reference to the subconversation
        let result_content = format!(
            "{}\n\n[subconversation_id: {}]",
            result_text,
            subconversation_id.as_str()
        );
        let tool_result = llm::ToolResult {
            tool_call_id: tool_call_id.to_string(),
            content: vec![llm::ToolResultContent::text(result_content)],
        };

        // Add as a message in the parent span (tool results are sent as User role)
        let content = vec![ContentBlock::ToolResult(tool_result.clone())];
        self.add_message(
            parent_span_id,
            parent_turn_id,
            Role::User,
            content,
            OriginKind::System,
        )
        .await
    }

    /// Resolve a conversation path (turns with content) to resolved messages.
    async fn resolve_path(&self, path: &[TurnWithContent]) -> Result<Vec<ResolvedMessage>> {
        let mut messages = Vec::new();

        for turn in path {
            let turn_id = turn.turn.id.clone();
            for msg in &turn.messages {
                let resolved = self.resolve_stored_content(&msg.content).await?;
                messages.push(ResolvedMessage::new(msg.message.role, resolved, turn_id.clone()));
            }
        }

        Ok(messages)
    }

    /// Resolve stored content references to resolved content.
    pub async fn resolve_stored_content(
        &self,
        content: &[StoredContent],
    ) -> Result<Vec<ResolvedContent>> {
        let mut resolved = Vec::with_capacity(content.len());

        for item in content {
            let r = match item {
                StoredContent::TextRef { content_block_id } => {
                    let text = self
                        .content_block_store
                        .get_text(content_block_id)
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("Content block not found: {}", content_block_id)
                        })?;
                    ResolvedContent::text(text)
                }
                StoredContent::AssetRef {
                    asset_id,
                    mime_type,
                } => {
                    // Look up asset to get blob_hash and load data for LLM
                    let stored_asset = self.asset_store.get(asset_id).await?
                        .ok_or_else(|| anyhow::anyhow!("Asset not found: {}", asset_id))?;
                    let blob_hash = &stored_asset.blob_hash;

                    // Load blob data and create resolved ContentBlock for LLM
                    let resolved_block = match self.blob_store.get(blob_hash).await {
                        Ok(data) => {
                            let base64_data = STANDARD.encode(&data);
                            if mime_type.starts_with("image/") {
                                Some(ContentBlock::Image {
                                    data: base64_data,
                                    mime_type: mime_type.clone(),
                                })
                            } else if mime_type.starts_with("audio/") {
                                Some(ContentBlock::Audio {
                                    data: base64_data,
                                    mime_type: mime_type.clone(),
                                })
                            } else {
                                None
                            }
                        }
                        Err(_) => None,
                    };
                    ResolvedContent::asset(
                        asset_id.clone(),
                        blob_hash.clone(),
                        mime_type.clone(),
                        resolved_block,
                    )
                }
                StoredContent::EntityRef { entity_id } => {
                    ResolvedContent::entity(entity_id.clone())
                }
                StoredContent::ToolCall(call) => ResolvedContent::tool_call(call.clone()),
                StoredContent::ToolResult(result) => ResolvedContent::tool_result(result.clone()),
            };
            resolved.push(r);
        }

        Ok(resolved)
    }

    /// Add a message to a span, returning a resolved message for caching.
    ///
    /// Stores content blocks, adds the message to the span, and resolves
    /// content for display/LLM use.
    pub async fn add_message(
        &self,
        span_id: &SpanId,
        turn_id: &TurnId,
        role: Role,
        content: Vec<ContentBlock>,
        origin: OriginKind,
    ) -> Result<ResolvedMessage> {
        // Store content blocks
        let mut stored = Vec::with_capacity(content.len());
        for block in content {
            stored.push(self.store_content_block(block, origin).await?);
        }

        // Add message to turn store
        self.turn_store.add_message(span_id, role, &stored).await?;

        // Resolve for caching
        let resolved = self.resolve_stored_content(&stored).await?;

        Ok(ResolvedMessage::new(role, resolved, turn_id.clone()))
    }

    /// Get resolved context up to (but not including) a specific turn.
    ///
    /// Used for regeneration - returns messages that should be sent to LLM
    /// before generating a new response at the target turn.
    pub async fn get_context_before_turn(
        &self,
        conversation_id: &ConversationId,
        turn_id: &TurnId,
    ) -> Result<Vec<ResolvedMessage>> {
        let context_path = self.turn_store
            .get_context_at(conversation_id, turn_id)
            .await?;

        self.resolve_path(&context_path).await
    }

    // ========================================================================
    // Generic entity + content + relation primitives
    //
    // These primitives underpin the upcoming EntityApi (daemon layer) and the
    // entity-first admin/Noema UIs. The coordinator is the one place where
    // multi-store orchestration lives — creating a content block and
    // referencing it from an entity, walking `structure::contained_in` trees,
    // GCing orphan content blocks on delete, etc.
    // ========================================================================

    /// Create a new entity, optionally with an initial content block and origin.
    ///
    /// - `content` is `(text, origin)`: the text is stored as a new content
    ///   block with the given origin, and the entity's `content_block_id` is
    ///   set to it.
    /// - `origin` is the entity's `"<scheme>:<id>"` origin (separate from the
    ///   content block's provenance origin).
    pub async fn create_entity_with_content(
        &self,
        kind: EntityType,
        user: Option<&UserId>,
        name: Option<&str>,
        content: Option<(&str, ContentOrigin)>,
        origin: Option<&str>,
    ) -> Result<EntityId> {
        let entity_id = self.entity_store.create_entity(kind, user).await?;

        // Resolve initial content, if any.
        let content_block_id = match content {
            Some((text, content_origin)) => {
                let block = ContentBlockData::markdown(text).with_origin(content_origin);
                Some(self.content_block_store.store(block).await?)
            }
            None => None,
        };

        if name.is_some() || content_block_id.is_some() || origin.is_some() {
            let mut entity = self
                .entity_store
                .get_entity(&entity_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("entity not found after create: {entity_id}"))?;
            if let Some(n) = name {
                entity.name = Some(n.to_string());
            }
            if let Some(cid) = content_block_id {
                entity.content_block_id = Some(cid);
            }
            if let Some(o) = origin {
                entity.origin = Some(o.to_string());
            }
            self.entity_store.update_entity(&entity_id, &entity).await?;
        }

        Ok(entity_id)
    }

    /// Update an entity's live text. Stores a new content block, updates the
    /// entity's `content_block_id`, and leaves the old block as an orphan
    /// candidate (cleaned up lazily; content blocks are immutable snapshots).
    pub async fn update_entity_content(
        &self,
        id: &EntityId,
        text: &str,
        origin: ContentOrigin,
    ) -> Result<ContentBlockId> {
        let block = ContentBlockData::markdown(text).with_origin(origin);
        let new_block_id = self.content_block_store.store(block).await?;

        let mut entity = self
            .entity_store
            .get_entity(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("entity not found: {id}"))?;
        entity.content_block_id = Some(new_block_id.clone());
        self.entity_store.update_entity(id, &entity).await?;

        Ok(new_block_id)
    }

    /// Resolve an entity's live text via its `content_block_id`, if any.
    pub async fn resolve_entity_text(&self, id: &EntityId) -> Result<Option<String>> {
        let entity = match self.entity_store.get_entity(id).await? {
            Some(e) => e,
            None => return Ok(None),
        };
        match entity.content_block_id.as_ref() {
            Some(block_id) => self.content_block_store.get_text(block_id).await,
            None => Ok(None),
        }
    }

    /// Look up an entity by exact `origin` string (`"<scheme>:<id>"`).
    pub async fn get_entity_by_origin(
        &self,
        user: &UserId,
        origin: &str,
    ) -> Result<Option<StoredEntity>> {
        self.entity_store.get_entity_by_origin(user, origin).await
    }

    /// List entities whose `origin` is in the given scheme (e.g. `"google_drive"`).
    /// Thin wrapper around an internal filter — uses `LIKE '<scheme>:%'`.
    pub async fn list_entities_by_origin_scheme(
        &self,
        user: &UserId,
        scheme: &str,
    ) -> Result<Vec<StoredEntity>> {
        // No dedicated EntityStore helper yet; filter list by origin prefix.
        let all = self.entity_store.list_entities(user, None).await?;
        let prefix = format!("{scheme}:");
        Ok(all
            .into_iter()
            .filter(|e| e.origin.as_deref().is_some_and(|o| o.starts_with(&prefix)))
            .collect())
    }

    /// List entities whose `entity_type` starts with the given prefix
    /// (e.g. `"document::"` for all document kinds).
    pub async fn list_entities_by_type_prefix(
        &self,
        user: &UserId,
        prefix: &str,
    ) -> Result<Vec<StoredEntity>> {
        self.entity_store
            .list_entities_by_type_prefix(user, prefix)
            .await
    }

    fn relation_has_tree_invariants(relation: &RelationType) -> bool {
        relation == &RelationType::structure_contained_in()
    }

    async fn ensure_tree_relation_allowed(
        &self,
        child: &EntityId,
        parent: &EntityId,
        relation: &RelationType,
    ) -> Result<()> {
        if child == parent {
            anyhow::bail!("{} cannot relate an entity to itself", relation);
        }

        self.entity_store
            .get_entity(child)
            .await?
            .ok_or_else(|| anyhow::anyhow!("child entity not found: {child}"))?;
        self.entity_store
            .get_entity(parent)
            .await?
            .ok_or_else(|| anyhow::anyhow!("parent entity not found: {parent}"))?;

        let mut seen: HashSet<String> = HashSet::new();
        let mut stack = vec![parent.clone()];
        while let Some(node) = stack.pop() {
            for (next_parent, _) in self
                .entity_store
                .get_relations_from(&node, Some(relation))
                .await?
            {
                if &next_parent == child {
                    anyhow::bail!("{} cannot create a cycle", relation);
                }
                if seen.insert(next_parent.as_str().to_string()) {
                    stack.push(next_parent);
                }
            }
        }

        Ok(())
    }

    async fn remove_tree_parents(
        &self,
        child: &EntityId,
        relation: &RelationType,
    ) -> Result<Vec<EntityId>> {
        let existing_parents = self
            .entity_store
            .get_relations_from(child, Some(relation))
            .await?;
        let mut removed = Vec::with_capacity(existing_parents.len());
        for (parent_id, _) in existing_parents {
            self.entity_store
                .remove_relation(child, &parent_id, relation)
                .await?;
            removed.push(parent_id);
        }
        Ok(removed)
    }

    async fn renumber_children(
        &self,
        parent: &EntityId,
        relation: &RelationType,
    ) -> Result<()> {
        let siblings = self
            .entity_store
            .list_relations_to_ordered(parent, relation)
            .await?;
        for (position, (child_id, rel)) in siblings.into_iter().enumerate() {
            self.entity_store
                .add_relation(
                    &child_id,
                    parent,
                    relation.clone(),
                    Some(position as i64),
                    rel.metadata,
                )
                .await?;
        }
        Ok(())
    }

    async fn insert_tree_child(
        &self,
        parent: &EntityId,
        child: &EntityId,
        relation: &RelationType,
        position: Option<i64>,
        metadata: Option<serde_json::Value>,
    ) -> Result<()> {
        let siblings = self
            .entity_store
            .list_relations_to_ordered(parent, relation)
            .await?;
        let mut ordered: Vec<(EntityId, Option<serde_json::Value>)> = siblings
            .into_iter()
            .filter(|(sibling_id, _)| sibling_id != child)
            .map(|(sibling_id, rel)| (sibling_id, rel.metadata))
            .collect();

        let requested_position = position
            .unwrap_or(ordered.len() as i64)
            .clamp(0, ordered.len() as i64);
        let insert_at = requested_position as usize;
        ordered.insert(insert_at, (child.clone(), metadata));

        for (position, (child_id, metadata)) in ordered.into_iter().enumerate() {
            self.entity_store
                .add_relation(
                    &child_id,
                    parent,
                    relation.clone(),
                    Some(position as i64),
                    metadata,
                )
                .await?;
        }

        Ok(())
    }

    /// Add a relation through the coordinator so relation-specific invariants
    /// are enforced consistently. `structure::contained_in` is treated as a
    /// tree edge: each child has one parent, cycles are rejected, and sibling
    /// positions are normalized after writes.
    pub async fn add_relation(
        &self,
        from: &EntityId,
        to: &EntityId,
        relation: RelationType,
        position: Option<i64>,
        metadata: Option<serde_json::Value>,
    ) -> Result<()> {
        if !Self::relation_has_tree_invariants(&relation) {
            return self
                .entity_store
                .add_relation(from, to, relation, position, metadata)
                .await;
        }

        self.ensure_tree_relation_allowed(from, to, &relation).await?;
        let previous_parents = self.remove_tree_parents(from, &relation).await?;
        self.insert_tree_child(to, from, &relation, position, metadata)
            .await?;

        for parent in previous_parents {
            if &parent != to {
                self.renumber_children(&parent, &relation).await?;
            }
        }

        Ok(())
    }

    /// Remove a relation through the coordinator so relation-specific cleanup
    /// remains centralized.
    pub async fn remove_relation(
        &self,
        from: &EntityId,
        to: &EntityId,
        relation: &RelationType,
    ) -> Result<()> {
        self.entity_store.remove_relation(from, to, relation).await?;
        if Self::relation_has_tree_invariants(relation) {
            self.renumber_children(to, relation).await?;
        }
        Ok(())
    }

    /// Add a `child ──relation──> parent` edge, ordered by `position` when set.
    /// Uses the same relation invariant path as the public coordinator API.
    pub async fn add_child(
        &self,
        parent: &EntityId,
        child: &EntityId,
        relation: RelationType,
        position: Option<i64>,
        metadata: Option<serde_json::Value>,
    ) -> Result<()> {
        self.add_relation(child, parent, relation, position, metadata)
            .await
    }

    /// List the ordered children of an entity under a given relation.
    /// Returns `(child_entity, position)` tuples.
    pub async fn list_children(
        &self,
        parent: &EntityId,
        relation: &RelationType,
    ) -> Result<Vec<(StoredEntity, Option<i64>)>> {
        let relations = self
            .entity_store
            .list_relations_to_ordered(parent, relation)
            .await?;
        let mut out = Vec::with_capacity(relations.len());
        for (child_id, rel) in relations {
            if let Some(child) = self.entity_store.get_entity(&child_id).await? {
                out.push((child, rel.position));
            }
        }
        Ok(out)
    }

    /// Walk the tree of descendants reachable via the given relation (e.g.
    /// `structure::contained_in`). Returns a flat list of
    /// `(entity, parent_id, position)` tuples in DFS order. Detects cycles
    /// defensively: the same entity is never yielded twice.
    pub async fn list_children_recursive(
        &self,
        parent: &EntityId,
        relation: &RelationType,
    ) -> Result<Vec<(StoredEntity, EntityId, Option<i64>)>> {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: Vec<EntityId> = vec![parent.clone()];
        while let Some(node) = queue.pop() {
            let children = self
                .entity_store
                .list_relations_to_ordered(&node, relation)
                .await?;
            for (child_id, rel) in children {
                if !seen.insert(child_id.as_str().to_string()) {
                    continue;
                }
                if let Some(child) = self.entity_store.get_entity(&child_id).await? {
                    out.push((child, node.clone(), rel.position));
                    queue.push(child_id);
                }
            }
        }
        Ok(out)
    }

    /// Atomically re-parent `child` under `new_parent` and set `new_position`.
    /// Existing `contained_in` edges from `child` under the given relation are
    /// removed; the new edge is inserted; sibling positions under `new_parent`
    /// are renumbered to accommodate the insertion.
    ///
    /// Used by drag-and-drop filing in the admin / Noema UIs.
    pub async fn move_entity(
        &self,
        child: &EntityId,
        new_parent: &EntityId,
        new_position: i64,
        relation: &RelationType,
    ) -> Result<()> {
        if Self::relation_has_tree_invariants(relation) {
            return self
                .add_relation(
                    child,
                    new_parent,
                    relation.clone(),
                    Some(new_position),
                    None,
                )
                .await;
        }

        // Drop any existing parents under this relation (there should normally
        // be at most one).
        let existing_parents = self
            .entity_store
            .get_relations_from(child, Some(relation))
            .await?;
        for (parent_id, _) in existing_parents {
            self.entity_store
                .remove_relation(child, &parent_id, relation)
                .await?;
        }

        // Shift existing siblings >= new_position up by 1 to make room.
        let siblings = self
            .entity_store
            .list_relations_to_ordered(new_parent, relation)
            .await?;
        for (sibling_id, rel) in siblings {
            if let Some(pos) = rel.position {
                if pos >= new_position {
                    self.entity_store
                        .add_relation(
                            &sibling_id,
                            new_parent,
                            relation.clone(),
                            Some(pos + 1),
                            rel.metadata,
                        )
                        .await?;
                }
            }
        }

        self.entity_store
            .add_relation(child, new_parent, relation.clone(), Some(new_position), None)
            .await
    }

    /// Replace the full set of assets referenced by an entity. Used when a
    /// tab / flat doc's markdown embeds a new set of images.
    pub async fn set_entity_assets(
        &self,
        entity_id: &EntityId,
        asset_ids: &[AssetId],
    ) -> Result<()> {
        self.entity_store
            .set_entity_assets(entity_id, asset_ids)
            .await
    }

    /// Get the assets referenced by an entity.
    pub async fn get_entity_assets(&self, entity_id: &EntityId) -> Result<Vec<AssetId>> {
        self.entity_store.get_entity_assets(entity_id).await
    }

    /// Find all entities that still reference an asset. Blob GC uses this to
    /// decide whether an asset (and its underlying blob) can be dropped.
    pub async fn entities_referencing_asset(
        &self,
        asset_id: &AssetId,
    ) -> Result<Vec<EntityId>> {
        self.entity_store
            .entities_referencing_asset(asset_id)
            .await
    }

    /// Delete an entity and its descendants reachable through the given
    /// relations (typically `[structure::contained_in]`). Orphan content
    /// blocks from deleted entities are left for content-block GC; no DB
    /// cascade is relied upon.
    pub async fn delete_entity_cascade(
        &self,
        id: &EntityId,
        relations_to_follow: &[RelationType],
    ) -> Result<()> {
        // Walk descendants via each follow-relation; collect all entity ids
        // in DFS post-order so that children are deleted before parents.
        use std::collections::HashSet;
        let mut to_delete: Vec<EntityId> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut stack: Vec<EntityId> = vec![id.clone()];
        while let Some(node) = stack.pop() {
            if !seen.insert(node.as_str().to_string()) {
                continue;
            }
            for rel in relations_to_follow {
                let children = self
                    .entity_store
                    .list_relations_to_ordered(&node, rel)
                    .await?;
                for (child_id, _) in children {
                    stack.push(child_id);
                }
            }
            to_delete.push(node);
        }
        // Delete in reverse so children go first.
        for entity_id in to_delete.into_iter().rev() {
            self.entity_store.delete_entity(&entity_id).await?;
        }
        Ok(())
    }
}

/// Implement ContentResolver for the generic coordinator
#[async_trait]
impl<S: StorageTypes> ContentResolver for StorageCoordinator<S> {
    async fn get_text(&self, id: &ContentBlockId) -> Result<String> {
        self.content_block_store
            .get_text(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Content block not found: {}", id))
    }

    async fn get_asset(&self, id: &AssetId) -> Result<(Vec<u8>, String)> {
        let stored_asset = self
            .asset_store
            .get(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Asset not found: {}", id))?;

        // Use blob_hash to fetch from blob store
        let data = self.blob_store.get(&stored_asset.blob_hash).await?;

        Ok((data, stored_asset.mime_type.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::content::StoredContent;
    use crate::storage::implementations::memory::{
        MemoryAssetStore, MemoryBlobStore, MemoryEntityStore, MemoryStorage, MemoryTextStore,
        MemoryTurnStore,
    };
    use crate::storage::implementations::mock::{
        MockAssetStore, MockBlobStore, MockEntityStore, MockStorage, MockTextStore,
        MockTurnStore,
    };
    use crate::storage::traits::AssetStore;

    fn make_coordinator(content_block_store: Arc<MockTextStore>) -> StorageCoordinator<MockStorage> {
        StorageCoordinator::new(
            Arc::new(MockBlobStore::new()),
            Arc::new(MockAssetStore::new()),
            content_block_store,
            Arc::new(MockEntityStore),
            Arc::new(MockTurnStore),
        )
    }

    fn make_memory_coordinator() -> StorageCoordinator<MemoryStorage> {
        StorageCoordinator::new(
            Arc::new(MemoryBlobStore::new()),
            Arc::new(MemoryAssetStore::new()),
            Arc::new(MemoryTextStore::new()),
            Arc::new(MemoryEntityStore::new()),
            Arc::new(MemoryTurnStore::new()),
        )
    }

    async fn create_test_entity(
        coordinator: &StorageCoordinator<MemoryStorage>,
        kind: EntityType,
    ) -> EntityId {
        coordinator
            .create_entity_with_content(kind, None, None, None, None)
            .await
            .unwrap()
    }

    fn make_coordinator_with_stores(
        blob_store: Arc<MockBlobStore>,
        asset_store: Arc<MockAssetStore>,
        content_block_store: Arc<MockTextStore>,
    ) -> StorageCoordinator<MockStorage> {
        StorageCoordinator::new(
            blob_store,
            asset_store,
            content_block_store,
            Arc::new(MockEntityStore),
            Arc::new(MockTurnStore),
        )
    }

    #[tokio::test]
    async fn test_store_text_content() {
        let content_block_store = Arc::new(MockTextStore::new());
        let coordinator = make_coordinator(content_block_store.clone());

        let block = ContentBlock::Text {
            text: "Hello world".to_string(),
        };

        let result = coordinator
            .store_content_block(block, OriginKind::User)
            .await
            .unwrap();

        match &result {
            StoredContent::TextRef { content_block_id } => {
                // Verify the text was stored
                let stored_text = content_block_store.get_text(content_block_id).await.unwrap();
                assert_eq!(stored_text, Some("Hello world".to_string()));
            }
            other => panic!("Expected TextRef, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_store_image_content() {
        let blob_store = Arc::new(MockBlobStore::new());
        let asset_store = Arc::new(MockAssetStore::new());
        let content_block_store = Arc::new(MockTextStore::new());
        let coordinator = make_coordinator_with_stores(
            blob_store.clone(),
            asset_store.clone(),
            content_block_store,
        );

        let image_data = STANDARD.encode(b"fake image bytes");
        let block = ContentBlock::Image {
            data: image_data,
            mime_type: "image/png".to_string(),
        };

        let result = coordinator
            .store_content_block(block, OriginKind::User)
            .await
            .unwrap();

        match &result {
            StoredContent::AssetRef {
                asset_id,
                mime_type,
                ..
            } => {
                assert_eq!(mime_type, "image/png");
                // Verify asset metadata was stored
                assert!(asset_store.exists(asset_id).await.unwrap());
            }
            other => panic!("Expected AssetRef, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_resolve_text() {
        let content_block_store = Arc::new(MockTextStore::new());
        let coordinator = make_coordinator(content_block_store.clone());

        // Store some text
        let block = ContentBlock::Text {
            text: "Test text".to_string(),
        };
        let stored = coordinator
            .store_content_block(block, OriginKind::User)
            .await
            .unwrap();

        // Resolve it back
        let resolved = stored.resolve(&coordinator).await.unwrap();

        match resolved {
            ContentBlock::Text { text } => {
                assert_eq!(text, "Test text");
            }
            other => panic!("Expected Text, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_resolve_image() {
        let blob_store = Arc::new(MockBlobStore::new());
        let asset_store = Arc::new(MockAssetStore::new());
        let content_block_store = Arc::new(MockTextStore::new());
        let coordinator = make_coordinator_with_stores(
            blob_store.clone(),
            asset_store.clone(),
            content_block_store,
        );

        let original_data = b"fake image bytes";
        let image_data = STANDARD.encode(original_data);
        let block = ContentBlock::Image {
            data: image_data,
            mime_type: "image/png".to_string(),
        };

        let stored = coordinator
            .store_content_block(block, OriginKind::User)
            .await
            .unwrap();

        // Resolve it back
        let resolved = stored.resolve(&coordinator).await.unwrap();

        match resolved {
            ContentBlock::Image { data, mime_type } => {
                assert_eq!(mime_type, "image/png");
                let decoded = STANDARD.decode(&data).unwrap();
                assert_eq!(decoded, original_data);
            }
            other => panic!("Expected Image, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_tool_call_passthrough() {
        let content_block_store = Arc::new(MockTextStore::new());
        let coordinator = make_coordinator(content_block_store);

        let tool_call = llm::ToolCall {
            id: "call-1".to_string(),
            name: "test_tool".to_string(),
            arguments: serde_json::json!({"key": "value"}),
            extra: serde_json::Value::Null,
        };
        let block = ContentBlock::ToolCall(tool_call.clone());

        let result = coordinator
            .store_content_block(block, OriginKind::Assistant)
            .await
            .unwrap();

        match &result {
            StoredContent::ToolCall(stored_call) => {
                assert_eq!(stored_call.name, "test_tool");
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_contained_in_rejects_self_parent() {
        let coordinator = make_memory_coordinator();
        let entity = create_test_entity(&coordinator, EntityType::document_note()).await;
        let relation = RelationType::structure_contained_in();

        let err = coordinator
            .add_child(&entity, &entity, relation, Some(0), None)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("itself"));
    }

    #[tokio::test]
    async fn test_contained_in_rejects_cycles() {
        let coordinator = make_memory_coordinator();
        let root = create_test_entity(&coordinator, EntityType::document_tabbed()).await;
        let child = create_test_entity(&coordinator, EntityType::document_tab()).await;
        let grandchild = create_test_entity(&coordinator, EntityType::document_tab()).await;
        let relation = RelationType::structure_contained_in();

        coordinator
            .add_child(&root, &child, relation.clone(), Some(0), None)
            .await
            .unwrap();
        coordinator
            .add_child(&child, &grandchild, relation.clone(), Some(0), None)
            .await
            .unwrap();

        let err = coordinator
            .add_child(&grandchild, &root, relation, Some(0), None)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("cycle"));
    }

    #[tokio::test]
    async fn test_contained_in_move_keeps_single_parent_and_renumbers() {
        let coordinator = make_memory_coordinator();
        let first_parent = create_test_entity(&coordinator, EntityType::document_tabbed()).await;
        let second_parent = create_test_entity(&coordinator, EntityType::document_tabbed()).await;
        let pinned_child = create_test_entity(&coordinator, EntityType::document_tab()).await;
        let moved_child = create_test_entity(&coordinator, EntityType::document_tab()).await;
        let relation = RelationType::structure_contained_in();

        coordinator
            .add_child(&first_parent, &pinned_child, relation.clone(), Some(0), None)
            .await
            .unwrap();
        coordinator
            .add_child(&first_parent, &moved_child, relation.clone(), Some(1), None)
            .await
            .unwrap();
        coordinator
            .add_child(&second_parent, &moved_child, relation.clone(), Some(0), None)
            .await
            .unwrap();

        let first_children = coordinator
            .list_children(&first_parent, &relation)
            .await
            .unwrap();
        assert_eq!(first_children.len(), 1);
        assert_eq!(first_children[0].0.id, pinned_child);
        assert_eq!(first_children[0].1, Some(0));

        let second_children = coordinator
            .list_children(&second_parent, &relation)
            .await
            .unwrap();
        assert_eq!(second_children.len(), 1);
        assert_eq!(second_children[0].0.id, moved_child);
        assert_eq!(second_children[0].1, Some(0));
    }

    #[tokio::test]
    async fn test_contained_in_remove_renumbers_siblings() {
        let coordinator = make_memory_coordinator();
        let parent = create_test_entity(&coordinator, EntityType::document_tabbed()).await;
        let first = create_test_entity(&coordinator, EntityType::document_tab()).await;
        let second = create_test_entity(&coordinator, EntityType::document_tab()).await;
        let third = create_test_entity(&coordinator, EntityType::document_tab()).await;
        let relation = RelationType::structure_contained_in();

        coordinator
            .add_child(&parent, &first, relation.clone(), Some(0), None)
            .await
            .unwrap();
        coordinator
            .add_child(&parent, &second, relation.clone(), Some(1), None)
            .await
            .unwrap();
        coordinator
            .add_child(&parent, &third, relation.clone(), Some(2), None)
            .await
            .unwrap();

        coordinator
            .remove_relation(&second, &parent, &relation)
            .await
            .unwrap();

        let children = coordinator.list_children(&parent, &relation).await.unwrap();
        let ids_and_positions: Vec<_> = children
            .into_iter()
            .map(|(entity, position)| (entity.id, position))
            .collect();
        assert_eq!(ids_and_positions, vec![(first, Some(0)), (third, Some(1))]);
    }
}
