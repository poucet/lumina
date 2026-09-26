//! Semantic search service — content-block-keyed, entity-kind filtered.
//!
//! - `search` runs the query through the vector store with the caller's
//!   `EntityFilter`, then resolves each hit against live entity state
//!   to (a) enforce access control and (b) drop rows whose owning
//!   entity has been deleted. Titles, bodies, and visibility are always
//!   read from the entity at query time — nothing is cached on the
//!   index beyond what filter SQL needs (`entity_kind`).
//! - `reindex` walks every content-bearing entity across all users,
//!   builds frontmatter (title + kind + ancestry via
//!   `structure::contained_in`) and enqueues an `EmbedJob`. The queue
//!   handles debouncing + dedupe-by-hash.

use std::sync::Arc;

use async_trait::async_trait;
use simply_core::embedding;
use simply_core::storage::ids::UserId;
use simply_core::storage::traits::{EntityStore, StorageTypes, Stores, StoredEntity, TextStore, UserStore};
use simply_core::storage::types::RelationType;
use simply_rpc::RequestContext;

use crate::api::*;
use crate::services::AccessPolicy;
use crate::services::embedding_queue::{EmbedJob, EmbeddingQueue};

pub struct SearchService<S: StorageTypes> {
    embedding_provider: Arc<dyn llm::EmbeddingProvider>,
    vector_store: Arc<dyn simply_core::embedding::VectorStore>,
    embedding_queue: Arc<dyn EmbeddingQueue>,
    stores: Arc<dyn Stores<S>>,
}

impl<S: StorageTypes> SearchService<S> {
    pub fn new(
        embedding_provider: Arc<dyn llm::EmbeddingProvider>,
        vector_store: Arc<dyn simply_core::embedding::VectorStore>,
        embedding_queue: Arc<dyn EmbeddingQueue>,
        stores: Arc<dyn Stores<S>>,
    ) -> Self {
        Self { embedding_provider, vector_store, embedding_queue, stores }
    }

    /// Translate the public (wire) `EntityFilter` into the core one.
    /// Kept separate because the wire type lives in `simply-daemon-api`
    /// and we don't want that crate to depend on `simply-core`.
    fn translate_filter(api_filter: EntityFilter) -> embedding::EntityFilter {
        fn translate(m: EntityTypeMatcher) -> embedding::EntityTypeMatcher {
            match m {
                EntityTypeMatcher::Exact(k) => embedding::EntityTypeMatcher::Exact(k),
                EntityTypeMatcher::Prefix(p) => embedding::EntityTypeMatcher::Prefix(p),
            }
        }
        embedding::EntityFilter {
            include: api_filter.include.into_iter().map(translate).collect(),
            exclude: api_filter.exclude.into_iter().map(translate).collect(),
        }
    }

}

#[async_trait]
impl<S: StorageTypes> SearchApi for SearchService<S> {
    async fn search(&self, ctx: &RequestContext, request: SearchRequest) -> anyhow::Result<Vec<SearchHit>> {
        let top_k = request.top_k.unwrap_or(5);
        let caller = ctx.scope.user_id.as_ref().map(UserId::from_string);

        let embeddings = self.embedding_provider.embed(&[&request.query]).await?;
        let vector = embeddings.into_iter().next()
            .ok_or_else(|| anyhow::anyhow!("embedding provider returned no vectors"))?
            .vector;

        let entity_filter = request.entity_filter.map(Self::translate_filter);
        let filter = embedding::SearchFilter { entity_filter };

        // Over-fetch: the vector store already dedupes by content block,
        // but access + tombstone filtering happens next and may knock
        // out candidates. Fetching 4x top_k gives us headroom without a
        // retry loop; still cheap since the vec0 query itself is O(log).
        let raw_top_k = top_k.saturating_mul(4).max(top_k);
        let results = self.vector_store.search(embedding::SearchQuery {
            vector,
            top_k: raw_top_k,
            filter: Some(filter),
        }).await?;

        // Batch-fetch all owning entities once.
        let entity_ids: Vec<_> = results.iter().map(|r| r.chunk.entity_id.clone()).collect();
        let entities = self.stores.entity().get_entities(&entity_ids).await?;
        let by_id: std::collections::HashMap<String, StoredEntity> = entities
            .into_iter()
            .map(|e| (e.id.as_str().to_string(), e))
            .collect();

        let mut hits = Vec::with_capacity(top_k);
        for r in results {
            let Some(entity) = by_id.get(r.chunk.entity_id.as_str()) else { continue };
            if !AccessPolicy::can_read(caller.as_ref(), entity) { continue; }
            hits.push(SearchHit {
                content_block_id: r.chunk.content_block_id.to_string(),
                entity_id: r.chunk.entity_id.to_string(),
                entity_kind: r.chunk.entity_kind,
                score: r.score,
            });
            if hits.len() >= top_k { break; }
        }

        Ok(hits)
    }

    async fn reindex(&self, _ctx: &RequestContext) -> anyhow::Result<ReindexStatus> {
        self.vector_store.delete_all().await?;

        let users = self.stores.user().list_users().await?;
        let mut entities_queued = 0usize;
        let contained_in = RelationType::structure_contained_in();

        for user in users {
            let entities = self.stores.entity().list_entities(&user.id, None).await?;
            for entity in entities {
                let Some(block_id) = entity.content_block_id.clone() else { continue };
                let Some(text) = self.stores.text().get_text(&block_id).await? else { continue };
                if text.is_empty() { continue; }

                let frontmatter = self.build_frontmatter(&entity, &contained_in).await;

                self.embedding_queue.enqueue(EmbedJob {
                    content_block_id: block_id,
                    entity_id: entity.id.clone(),
                    entity_kind: entity.entity_type.as_str().to_string(),
                    frontmatter: Some(frontmatter),
                    text,
                }).await;
                entities_queued += 1;
            }
        }

        Ok(ReindexStatus {
            message: format!("Reindex started: {entities_queued} entities queued"),
            entities_queued,
        })
    }

    async fn queue_status(&self, _ctx: &RequestContext) -> anyhow::Result<crate::services::embedding_queue::EmbeddingQueueStatus> {
        Ok(self.embedding_queue.status().await)
    }
}

impl<S: StorageTypes> SearchService<S> {
    /// Collect a short `title / kind / ancestry` header to prepend to the
    /// content before chunking. Walks outgoing `contained_in` up to a
    /// small depth — enough for "tab → doc" without running away on deep
    /// directory trees.
    async fn build_frontmatter(
        &self,
        entity: &StoredEntity,
        contained_in: &RelationType,
    ) -> String {
        let mut ancestry: Vec<String> = Vec::new();
        let mut current = entity.id.clone();
        for _ in 0..4 {
            let parents = self.stores.entity()
                .list_relations_from_ordered(&current, contained_in)
                .await
                .unwrap_or_default();
            let Some((parent_id, _)) = parents.into_iter().next() else { break };
            let Some(parent) = self.stores.entity().get_entity(&parent_id).await.ok().flatten()
            else { break };
            let title = parent.name.clone().unwrap_or_else(|| "(untitled)".to_string());
            ancestry.push(title);
            current = parent_id;
        }
        ancestry.reverse();

        let title = entity.name.clone().unwrap_or_else(|| "(untitled)".to_string());
        let kind = entity.entity_type.as_str();
        let mut lines = vec![
            format!("title: {title}"),
            format!("kind: {kind}"),
        ];
        if !ancestry.is_empty() {
            lines.push(format!("ancestry: {}", ancestry.join(" / ")));
        }
        format!("---\n{}\n---", lines.join("\n"))
    }
}
