//! Entity CRUD service — generic entity/relation/content surface over the
//! `StorageCoordinator` primitives. Implements `EntityApi`.
//!
//! Writes route through the coordinator so orphan cleanup (relations,
//! entity_assets mappings, content-block replacement) happens consistently.
//! Reads hit `entity_store` directly where possible.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use simply_core::embedding::VectorStore;
use simply_core::storage::coordinator::StorageCoordinator;
use simply_core::storage::helper::{content_hash, unix_timestamp};
use simply_core::storage::ids::{AssetId as CoreAssetId, EntityId, UserId};
use simply_core::storage::traits::{
    EntityStore, StorageTypes, StoredEntity, Stores, TextStore, VaultStore,
};
use simply_core::storage::types::entity::origin_scheme;
use simply_core::storage::types::{
    ContentOrigin, EntityType, RelationType, VaultFile, VaultSyncStatus,
};
use simply_core::storage::vault::files::{read_markdown_body, write_markdown_preserving_frontmatter};
use simply_core::storage::vault::sidecar::{
    read_sidecar_manifest, write_sidecar_manifest, VaultSidecarManifest,
};
use simply_rpc::RequestContext;

use crate::api::*;
use crate::services::AccessPolicy;
use crate::types::AssetId;

/// Relations whose incoming/outgoing edge-counts are useful on an
/// `EntitySummary`. We query these for every summary; more exotic relations
/// are inspected on demand.
const SUMMARY_COUNT_RELATIONS: &[&str] = &[
    "structure::contained_in",
    "reference::to",
];

fn summary_count_relation_types() -> Vec<RelationType> {
    SUMMARY_COUNT_RELATIONS
        .iter()
        .map(|relation| RelationType::new(*relation))
        .collect()
}

/// Kinds callers of the public API are allowed to create or convert entities
/// into. Restricted to the `document::` namespace so callers can't
/// accidentally mint `conversation` entities (daemon-owned), `system::*`
/// organizational primitives, or other future non-document kinds.
/// Import skills still reach any kind through the same API (they create
/// `document::tabbed` + `document::tab`), which is fine.
fn is_user_creatable_kind(kind: &str) -> bool {
    kind.starts_with("document::")
}

pub struct EntityService<S: StorageTypes> {
    coordinator: Arc<StorageCoordinator<S>>,
    stores: Arc<dyn Stores<S>>,
    embedding_queue: Option<Arc<dyn crate::services::embedding_queue::EmbeddingQueue>>,
    vector_store: Option<Arc<dyn VectorStore>>,
    user_email_cache: Option<Arc<crate::services::user::UserEmailCache>>,
    vault_root: PathBuf,
}

impl<S: StorageTypes> EntityService<S> {
    pub fn new(
        coordinator: Arc<StorageCoordinator<S>>,
        stores: Arc<dyn Stores<S>>,
        vault_root: PathBuf,
    ) -> Self {
        Self {
            coordinator,
            stores,
            embedding_queue: None,
            vector_store: None,
            user_email_cache: None,
            vault_root,
        }
    }

    pub fn with_embedding(
        mut self,
        queue: Arc<dyn crate::services::embedding_queue::EmbeddingQueue>,
    ) -> Self {
        self.embedding_queue = Some(queue);
        self
    }

    pub fn with_vector_store(mut self, store: Arc<dyn VectorStore>) -> Self {
        self.vector_store = Some(store);
        self
    }

    pub fn with_user_email_cache(
        mut self,
        cache: Arc<crate::services::user::UserEmailCache>,
    ) -> Self {
        self.user_email_cache = Some(cache);
        self
    }

    async fn load_entity(
        &self,
        entity_id: &EntityId,
    ) -> anyhow::Result<StoredEntity> {
        self.stores
            .entity()
            .get_entity(entity_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("entity not found: {entity_id}"))
    }

    async fn entity_for_read(
        &self,
        user_id: Option<&UserId>,
        entity_id: &EntityId,
    ) -> anyhow::Result<StoredEntity> {
        let entity = self.load_entity(entity_id).await?;
        AccessPolicy::ensure_read(user_id, &entity)?;
        Ok(entity)
    }

    async fn entity_for_write(
        &self,
        user_id: &UserId,
        entity_id: &EntityId,
    ) -> anyhow::Result<StoredEntity> {
        let entity = self.load_entity(entity_id).await?;
        AccessPolicy::ensure_write(Some(user_id), &entity)?;
        Ok(entity)
    }

    async fn clear_vault_conflicts_for_entity(
        &self,
        entity_id: &EntityId,
    ) -> anyhow::Result<()> {
        for conflict in self.stores.vault().list_vault_conflicts().await? {
            if conflict.entity_id.as_ref() == Some(entity_id)
                || conflict.observed_entity_id.as_ref() == Some(entity_id)
            {
                self.stores.vault().delete_vault_conflict(&conflict.id).await?;
            }
        }
        Ok(())
    }

    async fn cleanup_entity_projection(
        &self,
        entity_id: &EntityId,
    ) -> anyhow::Result<()> {
        if let Some(vector_store) = &self.vector_store {
            vector_store.delete_by_entity(entity_id).await?;
        }
        self.stores.vault().delete_vault_file(entity_id).await?;
        self.clear_vault_conflicts_for_entity(entity_id).await
    }

    async fn delete_entity_tree(
        &self,
        entity_id: &EntityId,
    ) -> anyhow::Result<()> {
        let contained_in = RelationType::structure_contained_in();
        let descendants = self
            .coordinator
            .list_children_recursive(entity_id, &contained_in)
            .await?;

        let mut entity_ids: Vec<EntityId> = descendants
            .into_iter()
            .map(|(entity, _, _)| entity.id)
            .collect();
        entity_ids.push(entity_id.clone());

        for id in &entity_ids {
            self.cleanup_entity_projection(id).await?;
        }

        self.coordinator
            .delete_entity_cascade(entity_id, &[contained_in])
            .await
    }

    async fn summary_with_counts(
        &self,
        entity: StoredEntity,
        incoming_counts: BTreeMap<String, u32>,
        outgoing_counts: BTreeMap<String, u32>,
        has_vault_content: bool,
    ) -> anyhow::Result<EntitySummary> {
        let owner_email = match (&entity.user_id, &self.user_email_cache) {
            (Some(uid), Some(cache)) => cache.get(&self.stores, uid).await,
            _ => None,
        };

        Ok(EntitySummary {
            id: entity.id.to_string(),
            kind: entity.entity_type.as_str().to_string(),
            title: entity.name.clone(),
            origin: entity.origin.clone(),
            user_id: entity.user_id.as_ref().map(|u| u.to_string()),
            owner_email,
            is_private: entity.is_private,
            created_at: entity.content.created_at,
            updated_at: entity.content.updated_at,
            has_content: entity.content_block_id.is_some() || has_vault_content,
            incoming_counts,
            outgoing_counts,
        })
    }

    /// Convert a `StoredEntity` to a wire `EntitySummary`, populating
    /// `has_content`, `owner_email`, and per-relation incoming/outgoing edge
    /// counts for the relations we surface by default.
    async fn to_summary(&self, entity: StoredEntity) -> anyhow::Result<EntitySummary> {
        let ids = vec![entity.id.clone()];
        let relation_types = summary_count_relation_types();
        let mut incoming_by_entity = self
            .stores
            .entity()
            .count_relations_to(&ids, &relation_types)
            .await?;
        let incoming_counts = incoming_by_entity.remove(&entity.id).unwrap_or_default();
        let mut outgoing_by_entity = self
            .stores
            .entity()
            .count_relations_from(&ids, &relation_types)
            .await?;
        let outgoing_counts = outgoing_by_entity.remove(&entity.id).unwrap_or_default();
        let has_vault_content = self
            .stores
            .vault()
            .get_vault_file(&entity.id)
            .await?
            .is_some();

        self.summary_with_counts(entity, incoming_counts, outgoing_counts, has_vault_content)
            .await
    }

    async fn entities_to_summaries(
        &self,
        entities: Vec<StoredEntity>,
    ) -> anyhow::Result<Vec<EntitySummary>> {
        if entities.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<EntityId> = entities.iter().map(|entity| entity.id.clone()).collect();
        let relation_types = summary_count_relation_types();
        let incoming_counts = self
            .stores
            .entity()
            .count_relations_to(&ids, &relation_types)
            .await?;
        let outgoing_counts = self
            .stores
            .entity()
            .count_relations_from(&ids, &relation_types)
            .await?;
        let vault_entity_ids: HashSet<EntityId> = self
            .stores
            .vault()
            .list_vault_files(None)
            .await?
            .into_iter()
            .map(|file| file.entity_id)
            .collect();

        let mut out = Vec::with_capacity(entities.len());
        for entity in entities {
            let incoming = incoming_counts
                .get(&entity.id)
                .cloned()
                .unwrap_or_default();
            let outgoing = outgoing_counts
                .get(&entity.id)
                .cloned()
                .unwrap_or_default();
            let has_vault_content = vault_entity_ids.contains(&entity.id);
            out.push(
                self.summary_with_counts(entity, incoming, outgoing, has_vault_content)
                    .await?,
            );
        }
        Ok(out)
    }

    async fn resolve_entity_content_markdown(
        &self,
        entity: &StoredEntity,
    ) -> anyhow::Result<Option<String>> {
        if let Some(vault_file) = self.stores.vault().get_vault_file(&entity.id).await? {
            if let Some(body) = read_markdown_body(&self.vault_root, &vault_file.path)? {
                return Ok(Some(body));
            }
        }

        match entity.content_block_id.as_ref() {
            Some(block_id) => self.stores.text().get_text(block_id).await,
            None => Ok(None),
        }
    }

    async fn write_vault_content_if_managed(
        &self,
        entity_id: &EntityId,
        text: &str,
        expected_content_hash: Option<&str>,
    ) -> anyhow::Result<Option<VaultFile>> {
        let Some(existing) = self.stores.vault().get_vault_file(entity_id).await? else {
            return Ok(None);
        };

        if let Some(expected) = expected_content_hash {
            let current = read_markdown_body(&self.vault_root, &existing.path)?.unwrap_or_default();
            let current_hash = content_hash(&current);
            if current_hash != expected {
                anyhow::bail!(
                    "vault file changed since editor loaded: expected {expected}, found {current_hash}"
                );
            }
        }

        let written = write_markdown_preserving_frontmatter(&self.vault_root, &existing.path, text)?;
        let file = VaultFile {
            entity_id: entity_id.clone(),
            path: written.relative_path,
            file_key: existing.file_key,
            mtime: written.mtime,
            content_hash: written.content_hash,
            frontmatter_hash: written.frontmatter_hash,
            sync_status: VaultSyncStatus::synced(),
            last_seen_at: unix_timestamp(),
        };
        self.stores.vault().upsert_vault_file(&file).await?;
        self.write_sidecar_snapshot().await?;
        Ok(Some(file))
    }

    async fn write_sidecar_snapshot(&self) -> anyhow::Result<()> {
        let files = self.stores.vault().list_vault_files(None).await?;
        let mut manifest = VaultSidecarManifest::from_vault_files(&files);
        if let Some(existing) = read_sidecar_manifest(&self.vault_root)? {
            manifest.preserve_user_fields_from(&existing);
        }
        write_sidecar_manifest(&self.vault_root, &manifest)
    }

    /// If `relation` is set, drop any entity that has an outgoing edge of
    /// that relation — i.e. keep only the roots of the relation's forest.
    /// If `relation` is `None`, pass everything through unchanged.
    async fn filter_roots(
        &self,
        entities: Vec<StoredEntity>,
        relation: Option<&str>,
    ) -> anyhow::Result<Vec<StoredEntity>> {
        let Some(rel_name) = relation else { return Ok(entities); };
        let rel = RelationType::new(rel_name);
        let mut out = Vec::with_capacity(entities.len());
        for e in entities {
            let outgoing = self
                .stores
                .entity()
                .get_relations_from(&e.id, Some(&rel))
                .await?;
            if outgoing.is_empty() {
                out.push(e);
            }
        }
        Ok(out)
    }

    /// Enqueue a content-bearing entity for embedding. Skips silently if
    /// the entity has no live `content_block_id` (container-only
    /// entities like `document::tabbed`) or if no embedding queue is
    /// wired. Frontmatter is left empty here — incremental per-write
    /// jobs embed raw text; the reindex path assembles richer
    /// frontmatter (title / kind / ancestry).
    async fn enqueue_embedding(
        &self,
        entity_id: &EntityId,
        kind: &EntityType,
        text: &str,
    ) {
        let Some(queue) = self.embedding_queue.as_ref() else { return; };
        let Some(content_block_id) = self
            .stores
            .entity()
            .get_entity(entity_id)
            .await
            .ok()
            .flatten()
            .and_then(|e| e.content_block_id.clone())
        else { return };

        queue.enqueue(crate::services::embedding_queue::EmbedJob {
            content_block_id,
            entity_id: entity_id.clone(),
            entity_kind: kind.as_str().to_string(),
            frontmatter: None,
            text: text.to_string(),
        }).await;
    }
}

#[async_trait]
impl<S: StorageTypes> EntityApi for EntityService<S> {
    // ----- Listing ---------------------------------------------------------

    async fn list_entities(
        &self,
        ctx: &RequestContext,
        type_prefix: Option<String>,
        root_of_relation: Option<String>,
    ) -> anyhow::Result<Vec<EntitySummary>> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let entities = match type_prefix.as_deref() {
            Some(prefix) if !prefix.is_empty() => {
                self.stores.entity().list_entities_by_type_prefix(&user_id, prefix).await?
            }
            _ => self.stores.entity().list_entities(&user_id, None).await?,
        };
        let readable = entities
            .into_iter()
            .filter(|e| AccessPolicy::can_read(Some(&user_id), e))
            .collect();
        let roots = self.filter_roots(readable, root_of_relation.as_deref()).await?;
        self.entities_to_summaries(roots).await
    }

    async fn search_entities(
        &self,
        ctx: &RequestContext,
        query: &str,
        type_prefix: Option<String>,
        root_of_relation: Option<String>,
    ) -> anyhow::Result<Vec<EntitySummary>> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let all = match type_prefix.as_deref() {
            Some(prefix) if !prefix.is_empty() => {
                self.stores.entity().list_entities_by_type_prefix(&user_id, prefix).await?
            }
            _ => self.stores.entity().list_entities(&user_id, None).await?,
        };
        let needle = query.to_lowercase();
        let matching: Vec<_> = all
            .into_iter()
            .filter(|e| AccessPolicy::can_read(Some(&user_id), e))
            .filter(|e| e.name.as_deref().is_some_and(|n| n.to_lowercase().contains(&needle)))
            .take(50)
            .collect();
        let roots = self.filter_roots(matching, root_of_relation.as_deref()).await?;
        self.entities_to_summaries(roots).await
    }

    // ----- Entity CRUD -----------------------------------------------------

    async fn get_entity(
        &self,
        ctx: &RequestContext,
        entity_id: &str,
    ) -> anyhow::Result<EntitySummary> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let id = EntityId::from_string(entity_id);
        let entity = self.entity_for_read(Some(&user_id), &id).await?;
        self.to_summary(entity).await
    }

    async fn get_entity_by_origin(
        &self,
        ctx: &RequestContext,
        origin: &str,
    ) -> anyhow::Result<Option<EntitySummary>> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let entity = self
            .stores
            .entity()
            .get_entity_by_origin(&user_id, origin)
            .await?;
        match entity {
            Some(e) => Ok(Some(self.to_summary(e).await?)),
            None => Ok(None),
        }
    }

    async fn get_entities(
        &self,
        ctx: &RequestContext,
        request: GetEntitiesRequest,
    ) -> anyhow::Result<Vec<EntityWithContent>> {
        let user_id = AccessPolicy::require_user(ctx)?;
        if request.ids.is_empty() {
            return Ok(Vec::new());
        }
        let entity_ids: Vec<EntityId> = request
            .ids
            .iter()
            .map(|id| EntityId::from_string(id.as_str()))
            .collect();

        // Single batch query for the entity rows. Access filtering
        // happens here rather than per-row so a forbidden id is simply
        // dropped (not surfaced as an error).
        let entities = self.stores.entity().get_entities(&entity_ids).await?;
        let accessible: Vec<_> = entities
            .into_iter()
            .filter(|e| AccessPolicy::can_read(Some(&user_id), e))
            .collect();

        // If content was requested, batch-fetch content blocks once for
        // non-vault entities. Vault-backed bodies are read from files below.
        let text_by_block = if request.include_content {
            let block_ids: Vec<_> = accessible
                .iter()
                .filter_map(|e| e.content_block_id.clone())
                .collect();
            self.stores.text().get_texts(&block_ids).await?
        } else {
            std::collections::HashMap::new()
        };

        let mut out = Vec::with_capacity(accessible.len());
        for entity in accessible {
            let content = if request.include_content {
                let body = if let Some(vault_file) = self.stores.vault().get_vault_file(&entity.id).await? {
                    read_markdown_body(&self.vault_root, &vault_file.path)?
                } else {
                    entity
                        .content_block_id
                        .as_ref()
                        .map(|block_id| text_by_block.get(block_id).cloned().unwrap_or_default())
                };

                body.map(|body| {
                    let hash = content_hash(&body);
                    EntityContent {
                        entity_id: entity.id.to_string(),
                        content_markdown: Some(body),
                        content_hash: Some(hash),
                        // referenced_assets are not fetched in bulk yet;
                        // callers that need them can still use the
                        // per-entity get_entity_content endpoint.
                        referenced_assets: Vec::new(),
                    }
                })
            } else {
                None
            };

            let summary = self.to_summary(entity).await?;
            out.push(EntityWithContent { summary, content });
        }

        Ok(out)
    }

    async fn create_entity(
        &self,
        ctx: &RequestContext,
        request: CreateEntityRequest,
    ) -> anyhow::Result<EntitySummary> {
        let user_id = AccessPolicy::require_user(ctx)?;
        if !is_user_creatable_kind(&request.kind) {
            anyhow::bail!(
                "entity kind `{}` is not user-creatable (must start with `document::`)",
                request.kind
            );
        }
        let kind = EntityType::new(request.kind);

        // Build a ContentOrigin for the new content block (if any).
        let content_origin = match request.origin.as_deref().and_then(|o| {
            let (scheme, id) = o.split_once(':')?;
            Some((scheme.to_string(), id.to_string()))
        }) {
            Some((scheme, ext_id)) if scheme == origin_scheme::GOOGLE_DRIVE => {
                ContentOrigin::import(ext_id).with_user(user_id.clone())
            }
            _ => ContentOrigin::user(user_id.clone()),
        };

        let content = request.content.as_deref().map(|text| (text, content_origin));

        let entity_id = self
            .coordinator
            .create_entity_with_content(
                kind.clone(),
                Some(&user_id),
                request.title.as_deref(),
                content,
                request.origin.as_deref(),
            )
            .await?;

        // Track referenced assets for blob GC.
        if !request.referenced_assets.is_empty() {
            let core_assets: Vec<CoreAssetId> = request
                .referenced_assets
                .iter()
                .map(|a| CoreAssetId::from_string(a.as_str()))
                .collect();
            self.coordinator.set_entity_assets(&entity_id, &core_assets).await?;
        }

        // Kick off embedding for content-bearing entities.
        if let Some(text) = request.content.as_deref() {
            self.enqueue_embedding(&entity_id, &kind, text).await;
        }

        let entity = self.stores.entity().get_entity(&entity_id).await?
            .ok_or_else(|| anyhow::anyhow!("entity disappeared after create: {entity_id}"))?;
        self.to_summary(entity).await
    }

    async fn rename_entity(
        &self,
        ctx: &RequestContext,
        entity_id: &str,
        name: &str,
    ) -> anyhow::Result<()> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let id = EntityId::from_string(entity_id);
        let mut entity = self.entity_for_write(&user_id, &id).await?;
        entity.name = Some(name.to_string());
        self.stores.entity().update_entity(&id, &entity).await
    }

    async fn change_entity_kind(
        &self,
        ctx: &RequestContext,
        entity_id: &str,
        new_kind: &str,
    ) -> anyhow::Result<()> {
        let user_id = AccessPolicy::require_user(ctx)?;
        if !is_user_creatable_kind(new_kind) {
            anyhow::bail!(
                "entity kind `{}` cannot be set from the public API (must start with `document::`)",
                new_kind
            );
        }
        let id = EntityId::from_string(entity_id);
        self.entity_for_write(&user_id, &id).await?;
        self.stores
            .entity()
            .set_entity_type(&id, EntityType::new(new_kind))
            .await
    }

    async fn delete_entity(
        &self,
        ctx: &RequestContext,
        entity_id: &str,
    ) -> anyhow::Result<()> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let id = EntityId::from_string(entity_id);
        let _ = self.entity_for_write(&user_id, &id).await?;

        self.delete_entity_tree(&id).await
    }

    // ----- Content --------------------------------------------------------

    async fn get_entity_content(
        &self,
        ctx: &RequestContext,
        entity_id: &str,
    ) -> anyhow::Result<EntityContent> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let id = EntityId::from_string(entity_id);
        let entity = self.entity_for_read(Some(&user_id), &id).await?;

        let content_markdown = self.resolve_entity_content_markdown(&entity).await?;
        let assets = self.coordinator.get_entity_assets(&id).await?;
        let referenced_assets: Vec<AssetId> = assets
            .into_iter()
            .map(|a| AssetId::from(a.as_str().to_string()))
            .collect();

        Ok(EntityContent {
            entity_id: entity_id.to_string(),
            content_hash: content_markdown.as_deref().map(content_hash),
            content_markdown,
            referenced_assets,
        })
    }

    async fn update_entity_content(
        &self,
        ctx: &RequestContext,
        entity_id: &str,
        request: UpdateEntityContentRequest,
    ) -> anyhow::Result<()> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let id = EntityId::from_string(entity_id);
        let entity = self.entity_for_write(&user_id, &id).await?;

        // Build a content origin based on the entity's own origin scheme.
        let content_origin = entity
            .origin
            .as_deref()
            .and_then(|o| o.split_once(':'))
            .map(|(scheme, ext_id)| match scheme {
                s if s == origin_scheme::GOOGLE_DRIVE => {
                    ContentOrigin::import(ext_id).with_user(user_id.clone())
                }
                _ => ContentOrigin::user(user_id.clone()),
            })
            .unwrap_or_else(|| ContentOrigin::user(user_id.clone()));

        self.write_vault_content_if_managed(
            &id,
            &request.content,
            request.expected_content_hash.as_deref(),
        )
        .await?;

        self.coordinator
            .update_entity_content(&id, &request.content, content_origin)
            .await?;

        // Refresh entity_assets.
        let core_assets: Vec<CoreAssetId> = request
            .referenced_assets
            .iter()
            .map(|a| CoreAssetId::from_string(a.as_str()))
            .collect();
        self.coordinator.set_entity_assets(&id, &core_assets).await?;

        self.enqueue_embedding(&id, &entity.entity_type, &request.content).await;
        Ok(())
    }

    async fn flush_entity_embedding(
        &self,
        ctx: &RequestContext,
        entity_id: &str,
    ) -> anyhow::Result<()> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let id = EntityId::from_string(entity_id);
        let entity = self.entity_for_write(&user_id, &id).await?;
        let Some(queue) = self.embedding_queue.as_ref() else { return Ok(()); };
        let Some(block_id) = entity.content_block_id.as_ref() else { return Ok(()); };
        queue.flush(block_id.as_str()).await;
        Ok(())
    }

    // ----- Relations ------------------------------------------------------

    async fn list_incoming(
        &self,
        ctx: &RequestContext,
        entity_id: &str,
        relation: &str,
    ) -> anyhow::Result<Vec<RelatedEntity>> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let id = EntityId::from_string(entity_id);
        self.entity_for_read(Some(&user_id), &id).await?;

        let rel = RelationType::new(relation);
        let related = self
            .stores
            .entity()
            .list_relations_to_ordered(&id, &rel)
            .await?;

        let mut out = Vec::with_capacity(related.len());
        for (other_id, edge) in related {
            let Some(other) = self.stores.entity().get_entity(&other_id).await? else {
                continue;
            };
            if !AccessPolicy::can_read(Some(&user_id), &other) {
                continue;
            }
            out.push(RelatedEntity {
                summary: self.to_summary(other).await?,
                position: edge.position,
            });
        }
        Ok(out)
    }

    async fn list_outgoing(
        &self,
        ctx: &RequestContext,
        entity_id: &str,
        relation: &str,
    ) -> anyhow::Result<Vec<RelatedEntity>> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let id = EntityId::from_string(entity_id);
        self.entity_for_read(Some(&user_id), &id).await?;

        let rel = RelationType::new(relation);
        let related = self
            .stores
            .entity()
            .list_relations_from_ordered(&id, &rel)
            .await?;

        let mut out = Vec::with_capacity(related.len());
        for (other_id, edge) in related {
            let Some(other) = self.stores.entity().get_entity(&other_id).await? else {
                continue;
            };
            if !AccessPolicy::can_read(Some(&user_id), &other) {
                continue;
            }
            out.push(RelatedEntity {
                summary: self.to_summary(other).await?,
                position: edge.position,
            });
        }
        Ok(out)
    }

    async fn add_relation(
        &self,
        ctx: &RequestContext,
        request: AddRelationRequest,
    ) -> anyhow::Result<()> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let from = EntityId::from_string(&request.from_id);
        let to = EntityId::from_string(&request.to_id);
        self.entity_for_write(&user_id, &from).await?;
        self.entity_for_write(&user_id, &to).await?;

        let rel = RelationType::new(&request.relation);
        self.coordinator
            .add_relation(&from, &to, rel, request.position, None)
            .await
    }

    async fn remove_relation(
        &self,
        ctx: &RequestContext,
        from_id: &str,
        to_id: &str,
        relation: &str,
    ) -> anyhow::Result<()> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let from = EntityId::from_string(from_id);
        let to = EntityId::from_string(to_id);
        self.entity_for_write(&user_id, &from).await?;
        self.entity_for_write(&user_id, &to).await?;

        let rel = RelationType::new(relation);
        self.coordinator.remove_relation(&from, &to, &rel).await
    }

    async fn move_relation(
        &self,
        ctx: &RequestContext,
        request: MoveRelationRequest,
    ) -> anyhow::Result<()> {
        let user_id = AccessPolicy::require_user(ctx)?;
        let from = EntityId::from_string(&request.from_id);
        let new_to = EntityId::from_string(&request.new_to_id);
        self.entity_for_write(&user_id, &from).await?;
        self.entity_for_write(&user_id, &new_to).await?;

        let rel = RelationType::new(&request.relation);
        self.coordinator
            .move_entity(&from, &new_to, request.new_position, &rel)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use simply_core::embedding::{SearchQuery, SearchResult, VectorChunk, VectorStore};
    use simply_core::storage::implementations::memory::{
        MemoryAssetStore, MemoryBlobStore, MemoryEntityStore, MemoryStorage, MemoryTextStore,
        MemoryTurnStore, MemoryUserStore, MemoryVaultStore,
    };
    use simply_core::storage::ids::VaultConflictId;
    use simply_core::storage::traits::{EntityStore, Stores, VaultStore};
    use simply_core::storage::types::{VaultConflict, VaultConflictReason};
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingVectorStore {
        deleted_entities: Mutex<Vec<EntityId>>,
    }

    impl RecordingVectorStore {
        fn deleted_entities(&self) -> Vec<EntityId> {
            self.deleted_entities.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl VectorStore for RecordingVectorStore {
        async fn upsert(&self, _: &[VectorChunk]) -> anyhow::Result<()> {
            Ok(())
        }

        async fn search(&self, _: SearchQuery) -> anyhow::Result<Vec<SearchResult>> {
            Ok(Vec::new())
        }

        async fn delete_by_content_block(
            &self,
            _: &simply_core::storage::ids::ContentBlockId,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn delete_by_entity(&self, entity_id: &EntityId) -> anyhow::Result<()> {
            self.deleted_entities
                .lock()
                .unwrap()
                .push(entity_id.clone());
            Ok(())
        }

        async fn delete_all(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct TestStores {
        blob: Arc<MemoryBlobStore>,
        asset: Arc<MemoryAssetStore>,
        text: Arc<MemoryTextStore>,
        turn: Arc<MemoryTurnStore>,
        user: Arc<MemoryUserStore>,
        entity: Arc<MemoryEntityStore>,
        vault: Arc<MemoryVaultStore>,
    }

    impl TestStores {
        fn new() -> Self {
            Self {
                blob: Arc::new(MemoryBlobStore::new()),
                asset: Arc::new(MemoryAssetStore::new()),
                text: Arc::new(MemoryTextStore::new()),
                turn: Arc::new(MemoryTurnStore::new()),
                user: Arc::new(MemoryUserStore::new()),
                entity: Arc::new(MemoryEntityStore::new()),
                vault: Arc::new(MemoryVaultStore::new()),
            }
        }
    }

    impl Stores<MemoryStorage> for TestStores {
        fn turn(&self) -> Arc<MemoryTurnStore> {
            Arc::clone(&self.turn)
        }

        fn user(&self) -> Arc<MemoryUserStore> {
            Arc::clone(&self.user)
        }

        fn blob(&self) -> Arc<MemoryBlobStore> {
            Arc::clone(&self.blob)
        }

        fn asset(&self) -> Arc<MemoryAssetStore> {
            Arc::clone(&self.asset)
        }

        fn text(&self) -> Arc<MemoryTextStore> {
            Arc::clone(&self.text)
        }

        fn entity(&self) -> Arc<MemoryEntityStore> {
            Arc::clone(&self.entity)
        }

        fn vault(&self) -> Arc<MemoryVaultStore> {
            Arc::clone(&self.vault)
        }
    }

    fn make_service() -> (
        EntityService<MemoryStorage>,
        Arc<TestStores>,
        Arc<RecordingVectorStore>,
    ) {
        let stores = Arc::new(TestStores::new());
        let coordinator = Arc::new(StorageCoordinator::new(
            stores.blob(),
            stores.asset(),
            stores.text(),
            stores.entity(),
            stores.turn(),
        ));
        let stores_dyn: Arc<dyn Stores<MemoryStorage>> = stores.clone();
        let vector_store = Arc::new(RecordingVectorStore::default());
        let vector_dyn: Arc<dyn VectorStore> = vector_store.clone();
        let service = EntityService::new(coordinator, stores_dyn, std::env::temp_dir())
            .with_vector_store(vector_dyn);

        (service, stores, vector_store)
    }

    fn vault_file(entity_id: EntityId, path: &str) -> VaultFile {
        VaultFile {
            entity_id,
            path: path.to_string(),
            file_key: None,
            mtime: Some(1),
            content_hash: format!("hash:{path}"),
            frontmatter_hash: None,
            sync_status: VaultSyncStatus::synced(),
            last_seen_at: 1,
        }
    }

    #[tokio::test]
    async fn test_delete_entity_tree_cleans_vault_conflicts_and_vectors() {
        let (service, stores, vector_store) = make_service();
        let parent = service
            .coordinator
            .create_entity_with_content(
                EntityType::document_tabbed(),
                None,
                Some("Parent"),
                None,
                None,
            )
            .await
            .unwrap();
        let child = service
            .coordinator
            .create_entity_with_content(
                EntityType::document_tab(),
                None,
                Some("Child"),
                None,
                None,
            )
            .await
            .unwrap();
        let relation = RelationType::structure_contained_in();
        service
            .coordinator
            .add_child(&parent, &child, relation, Some(0), None)
            .await
            .unwrap();

        stores
            .vault()
            .upsert_vault_file(&vault_file(parent.clone(), "parent.md"))
            .await
            .unwrap();
        stores
            .vault()
            .upsert_vault_file(&vault_file(child.clone(), "parent/child.md"))
            .await
            .unwrap();
        stores
            .vault()
            .insert_vault_conflict(&VaultConflict {
                id: VaultConflictId::from_string("conflict-1"),
                entity_id: Some(parent.clone()),
                path: "parent-copy.md".to_string(),
                reason: VaultConflictReason::duplicate_id(),
                observed_entity_id: Some(child.clone()),
                details: None,
                created_at: 1,
            })
            .await
            .unwrap();

        service.delete_entity_tree(&parent).await.unwrap();

        assert!(stores.entity().get_entity(&parent).await.unwrap().is_none());
        assert!(stores.entity().get_entity(&child).await.unwrap().is_none());
        assert!(stores.vault().get_vault_file(&parent).await.unwrap().is_none());
        assert!(stores.vault().get_vault_file(&child).await.unwrap().is_none());
        assert!(stores.vault().list_vault_conflicts().await.unwrap().is_empty());

        let mut deleted = vector_store.deleted_entities();
        deleted.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let mut expected = vec![parent, child];
        expected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(deleted, expected);
    }
}
