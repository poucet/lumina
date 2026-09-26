//! In-memory EntityStore implementation

use anyhow::Result;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::storage::ids::{AssetId, ContentBlockId, EntityId, UserId};
use crate::storage::traits::{EntityStore, RelationCountMap, StoredEntity};
use crate::storage::types::entity::{Entity, EntityRangeQuery, EntityRelation, EntityType, RelationType};
use crate::storage::types::stored_editable;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Storage entry for an entity
#[derive(Clone, Debug)]
struct EntityEntry {
    id: EntityId,
    entity_type: EntityType,
    user_id: Option<UserId>,
    name: Option<String>,
    is_private: bool,
    content_block_id: Option<ContentBlockId>,
    origin: Option<String>,
    metadata: Option<serde_json::Value>,
    created_at: i64,
    updated_at: i64,
}

impl EntityEntry {
    fn to_stored(&self) -> StoredEntity {
        let entity = Entity {
            entity_type: self.entity_type.clone(),
            user_id: self.user_id.clone(),
            name: self.name.clone(),
            is_private: self.is_private,
            content_block_id: self.content_block_id.clone(),
            origin: self.origin.clone(),
            metadata: self.metadata.clone(),
        };
        stored_editable(self.id.clone(), entity, self.created_at, self.updated_at)
    }
}

/// Relation key for indexing
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RelationKey {
    from_id: String,
    to_id: String,
    relation: String,
}

/// Storage entry for a relation
#[derive(Clone, Debug)]
struct RelationEntry {
    from_id: EntityId,
    to_id: EntityId,
    relation: RelationType,
    position: Option<i64>,
    metadata: Option<serde_json::Value>,
}

/// In-memory entity store for testing
#[derive(Debug, Default)]
pub struct MemoryEntityStore {
    entities: Mutex<HashMap<String, EntityEntry>>,
    relations: Mutex<HashMap<RelationKey, RelationEntry>>,
    entity_assets: Mutex<HashMap<String, HashSet<String>>>, // entity_id -> set of asset_id
}

impl MemoryEntityStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn count_relations_by_direction(
        &self,
        ids: &[EntityId],
        relation_types: &[RelationType],
        outgoing: bool,
    ) -> RelationCountMap {
        if ids.is_empty() || relation_types.is_empty() {
            return RelationCountMap::new();
        }

        let id_filter: HashSet<&str> = ids.iter().map(|id| id.as_str()).collect();
        let relation_filter: HashSet<&str> =
            relation_types.iter().map(|relation| relation.as_str()).collect();
        let relations = self.relations.lock().unwrap();
        let mut counts: RelationCountMap = HashMap::new();

        for entry in relations.values() {
            if !relation_filter.contains(entry.relation.as_str()) {
                continue;
            }
            let entity_id = if outgoing {
                &entry.from_id
            } else {
                &entry.to_id
            };
            if !id_filter.contains(entity_id.as_str()) {
                continue;
            }

            let relation_counts = counts
                .entry(entity_id.clone())
                .or_default();
            *relation_counts
                .entry(entry.relation.as_str().to_string())
                .or_insert(0) += 1;
        }

        counts
    }
}

#[async_trait]
impl EntityStore for MemoryEntityStore {
    async fn create_entity(
        &self,
        entity_type: EntityType,
        user_id: Option<&UserId>,
    ) -> Result<EntityId> {
        let id = EntityId::new();
        let now = now();
        let entry = EntityEntry {
            id: id.clone(),
            entity_type,
            user_id: user_id.cloned(),
            name: None,
            is_private: true,
            content_block_id: None,
            origin: None,
            metadata: None,
            created_at: now,
            updated_at: now,
        };
        self.entities
            .lock()
            .unwrap()
            .insert(id.as_str().to_string(), entry);
        Ok(id)
    }

    async fn get_entity(&self, id: &EntityId) -> Result<Option<StoredEntity>> {
        let entities = self.entities.lock().unwrap();
        Ok(entities.get(id.as_str()).map(|e| e.to_stored()))
    }

    async fn get_entities(&self, ids: &[EntityId]) -> Result<Vec<StoredEntity>> {
        let entities = self.entities.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| entities.get(id.as_str()).map(|e| e.to_stored()))
            .collect())
    }

    async fn get_entity_by_origin(
        &self,
        user_id: &UserId,
        origin: &str,
    ) -> Result<Option<StoredEntity>> {
        let entities = self.entities.lock().unwrap();
        let found = entities
            .values()
            .find(|e| e.user_id.as_ref() == Some(user_id) && e.origin.as_deref() == Some(origin))
            .map(|e| e.to_stored());
        Ok(found)
    }

    async fn list_entities(
        &self,
        user_id: &UserId,
        entity_type: Option<&EntityType>,
    ) -> Result<Vec<StoredEntity>> {
        let entities = self.entities.lock().unwrap();
        let mut result: Vec<_> = entities
            .values()
            .filter(|e| e.user_id.as_ref() == Some(user_id))
            .filter(|e| entity_type.is_none_or(|t| &e.entity_type == t))
            .map(|e| e.to_stored())
            .collect();
        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(result)
    }

    async fn list_entities_by_type_prefix(
        &self,
        user_id: &UserId,
        prefix: &str,
    ) -> Result<Vec<StoredEntity>> {
        let entities = self.entities.lock().unwrap();
        let mut result: Vec<_> = entities
            .values()
            .filter(|e| e.user_id.as_ref() == Some(user_id))
            .filter(|e| e.entity_type.as_str().starts_with(prefix))
            .map(|e| e.to_stored())
            .collect();
        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(result)
    }

    async fn list_entities_in_range(
        &self,
        user_id: &UserId,
        query: &EntityRangeQuery,
    ) -> Result<Vec<StoredEntity>> {
        let entities = self.entities.lock().unwrap();
        let mut result: Vec<_> = entities
            .values()
            .filter(|e| e.user_id.as_ref() == Some(user_id))
            .filter(|e| {
                query.entity_types.as_ref().is_none_or(|types| {
                    types.iter().any(|t| &e.entity_type == t)
                })
            })
            .filter(|e| e.updated_at >= query.start && e.updated_at <= query.end)
            .map(|e| e.to_stored())
            .collect();
        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        if let Some(limit) = query.limit {
            result.truncate(limit as usize);
        }
        Ok(result)
    }

    async fn update_entity(&self, id: &EntityId, entity: &Entity) -> Result<()> {
        let mut entities = self.entities.lock().unwrap();
        if let Some(entry) = entities.get_mut(id.as_str()) {
            entry.name = entity.name.clone();
            entry.is_private = entity.is_private;
            entry.content_block_id = entity.content_block_id.clone();
            entry.origin = entity.origin.clone();
            entry.metadata = entity.metadata.clone();
            entry.updated_at = now();
        }
        Ok(())
    }

    async fn set_entity_type(&self, id: &EntityId, new_type: EntityType) -> Result<()> {
        let mut entities = self.entities.lock().unwrap();
        if let Some(entry) = entities.get_mut(id.as_str()) {
            entry.entity_type = new_type;
            entry.updated_at = now();
        }
        Ok(())
    }

    async fn delete_entity(&self, id: &EntityId) -> Result<()> {
        // Remove relations
        {
            let mut relations = self.relations.lock().unwrap();
            relations.retain(|k, _| k.from_id != id.as_str() && k.to_id != id.as_str());
        }

        // Remove asset mappings
        {
            let mut mappings = self.entity_assets.lock().unwrap();
            mappings.remove(id.as_str());
        }

        // Remove entity
        self.entities.lock().unwrap().remove(id.as_str());
        Ok(())
    }

    async fn add_relation(
        &self,
        from_id: &EntityId,
        to_id: &EntityId,
        relation: RelationType,
        position: Option<i64>,
        metadata: Option<serde_json::Value>,
    ) -> Result<()> {
        let key = RelationKey {
            from_id: from_id.as_str().to_string(),
            to_id: to_id.as_str().to_string(),
            relation: relation.as_str().to_string(),
        };
        let entry = RelationEntry {
            from_id: from_id.clone(),
            to_id: to_id.clone(),
            relation,
            position,
            metadata,
        };
        self.relations.lock().unwrap().insert(key, entry);
        Ok(())
    }

    async fn get_relations_from(
        &self,
        id: &EntityId,
        relation_type: Option<&RelationType>,
    ) -> Result<Vec<(EntityId, EntityRelation)>> {
        let relations = self.relations.lock().unwrap();
        let result: Vec<_> = relations
            .values()
            .filter(|e| e.from_id == *id)
            .filter(|e| relation_type.is_none_or(|t| &e.relation == t))
            .map(|e| {
                (
                    e.to_id.clone(),
                    EntityRelation {
                        relation: e.relation.clone(),
                        position: e.position,
                        metadata: e.metadata.clone(),
                    },
                )
            })
            .collect();
        Ok(result)
    }

    async fn get_relations_to(
        &self,
        id: &EntityId,
        relation_type: Option<&RelationType>,
    ) -> Result<Vec<(EntityId, EntityRelation)>> {
        let relations = self.relations.lock().unwrap();
        let result: Vec<_> = relations
            .values()
            .filter(|e| e.to_id == *id)
            .filter(|e| relation_type.is_none_or(|t| &e.relation == t))
            .map(|e| {
                (
                    e.from_id.clone(),
                    EntityRelation {
                        relation: e.relation.clone(),
                        position: e.position,
                        metadata: e.metadata.clone(),
                    },
                )
            })
            .collect();
        Ok(result)
    }

    async fn count_relations_from(
        &self,
        ids: &[EntityId],
        relation_types: &[RelationType],
    ) -> Result<RelationCountMap> {
        Ok(self.count_relations_by_direction(ids, relation_types, true))
    }

    async fn count_relations_to(
        &self,
        ids: &[EntityId],
        relation_types: &[RelationType],
    ) -> Result<RelationCountMap> {
        Ok(self.count_relations_by_direction(ids, relation_types, false))
    }

    async fn list_relations_to_ordered(
        &self,
        id: &EntityId,
        relation_type: &RelationType,
    ) -> Result<Vec<(EntityId, EntityRelation)>> {
        let mut result = self.get_relations_to(id, Some(relation_type)).await?;
        result.sort_by(|a, b| match (a.1.position, b.1.position) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.0.as_str().cmp(b.0.as_str()),
        });
        Ok(result)
    }

    async fn list_relations_from_ordered(
        &self,
        id: &EntityId,
        relation_type: &RelationType,
    ) -> Result<Vec<(EntityId, EntityRelation)>> {
        let mut result = self.get_relations_from(id, Some(relation_type)).await?;
        result.sort_by(|a, b| match (a.1.position, b.1.position) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.0.as_str().cmp(b.0.as_str()),
        });
        Ok(result)
    }

    async fn remove_relation(
        &self,
        from_id: &EntityId,
        to_id: &EntityId,
        relation: &RelationType,
    ) -> Result<()> {
        let key = RelationKey {
            from_id: from_id.as_str().to_string(),
            to_id: to_id.as_str().to_string(),
            relation: relation.as_str().to_string(),
        };
        self.relations.lock().unwrap().remove(&key);
        Ok(())
    }

    async fn set_entity_assets(&self, entity_id: &EntityId, asset_ids: &[AssetId]) -> Result<()> {
        let mut mappings = self.entity_assets.lock().unwrap();
        let set: HashSet<String> = asset_ids
            .iter()
            .map(|a| a.as_str().to_string())
            .collect();
        mappings.insert(entity_id.as_str().to_string(), set);
        Ok(())
    }

    async fn get_entity_assets(&self, entity_id: &EntityId) -> Result<Vec<AssetId>> {
        let mappings = self.entity_assets.lock().unwrap();
        Ok(mappings
            .get(entity_id.as_str())
            .map(|set| set.iter().map(|s| AssetId::from_string(s.clone())).collect())
            .unwrap_or_default())
    }

    async fn entities_referencing_asset(&self, asset_id: &AssetId) -> Result<Vec<EntityId>> {
        let mappings = self.entity_assets.lock().unwrap();
        Ok(mappings
            .iter()
            .filter(|(_, assets)| assets.contains(asset_id.as_str()))
            .map(|(entity_id, _)| EntityId::from_string(entity_id.clone()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_and_get_entity() {
        let store = MemoryEntityStore::new();
        let user_id = UserId::new();

        let entity_id = store
            .create_entity(EntityType::conversation(), Some(&user_id))
            .await
            .unwrap();

        let entity = store.get_entity(&entity_id).await.unwrap().unwrap();
        assert_eq!(entity.entity_type.as_str(), "conversation");
        assert!(entity.is_private);
    }

    #[tokio::test]
    async fn test_list_entities_by_type_prefix() {
        let store = MemoryEntityStore::new();
        let user_id = UserId::new();

        store.create_entity(EntityType::document_tabbed(), Some(&user_id)).await.unwrap();
        store.create_entity(EntityType::document_note(), Some(&user_id)).await.unwrap();
        store.create_entity(EntityType::conversation(), Some(&user_id)).await.unwrap();

        let docs = store
            .list_entities_by_type_prefix(&user_id, "document::")
            .await
            .unwrap();
        assert_eq!(docs.len(), 2);
    }

    #[tokio::test]
    async fn test_ordered_relations() {
        let store = MemoryEntityStore::new();
        let parent = store.create_entity(EntityType::document_tabbed(), None).await.unwrap();
        let a = store.create_entity(EntityType::document_tab(), None).await.unwrap();
        let b = store.create_entity(EntityType::document_tab(), None).await.unwrap();

        store.add_relation(&b, &parent, RelationType::structure_contained_in(), Some(1), None).await.unwrap();
        store.add_relation(&a, &parent, RelationType::structure_contained_in(), Some(0), None).await.unwrap();

        let children = store
            .list_relations_to_ordered(&parent, &RelationType::structure_contained_in())
            .await
            .unwrap();
        let ids: Vec<_> = children.iter().map(|(id, _)| id.clone()).collect();
        assert_eq!(ids, vec![a, b]);
    }

    #[tokio::test]
    async fn test_relation_counts_batch_by_direction() {
        let store = MemoryEntityStore::new();
        let doc = store.create_entity(EntityType::document_tabbed(), None).await.unwrap();
        let tab_a = store.create_entity(EntityType::document_tab(), None).await.unwrap();
        let tab_b = store.create_entity(EntityType::document_tab(), None).await.unwrap();
        let note = store.create_entity(EntityType::document_note(), None).await.unwrap();
        let contained_in = RelationType::structure_contained_in();
        let reference_to = RelationType::reference_to();

        store.add_relation(&tab_a, &doc, contained_in.clone(), Some(0), None).await.unwrap();
        store.add_relation(&tab_b, &doc, contained_in.clone(), Some(1), None).await.unwrap();
        store.add_relation(&doc, &note, reference_to.clone(), None, None).await.unwrap();
        store.add_relation(&tab_a, &note, reference_to.clone(), None, None).await.unwrap();

        let ids = vec![doc.clone(), tab_a.clone(), tab_b.clone(), note.clone()];
        let relation_types = vec![contained_in.clone(), reference_to.clone()];
        let incoming = store.count_relations_to(&ids, &relation_types).await.unwrap();
        let outgoing = store.count_relations_from(&ids, &relation_types).await.unwrap();

        assert_eq!(incoming[&doc][contained_in.as_str()], 2);
        assert_eq!(incoming[&note][reference_to.as_str()], 2);
        assert!(!incoming.contains_key(&tab_a));
        assert_eq!(outgoing[&tab_a][contained_in.as_str()], 1);
        assert_eq!(outgoing[&tab_a][reference_to.as_str()], 1);
        assert_eq!(outgoing[&doc][reference_to.as_str()], 1);
        assert!(!outgoing.contains_key(&note));
    }
}
