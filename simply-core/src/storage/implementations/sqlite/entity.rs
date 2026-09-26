//! SQLite implementation of EntityStore

use anyhow::Result;
use async_trait::async_trait;
use rusqlite::{params, Connection, ToSql};
use std::collections::HashMap;

use super::SqliteStore;
use crate::storage::helper::unix_timestamp;
use crate::storage::ids::{AssetId, ContentBlockId, EntityId, UserId};
use crate::storage::traits::{EntityStore, RelationCountMap, StoredEntity};
use crate::storage::types::entity::{Entity, EntityRangeQuery, EntityRelation, EntityType, RelationType};
use crate::storage::types::stored_editable;

/// Columns for the `entities` table in select order used by `parse_entity_row`.
const ENTITY_SELECT_COLUMNS: &str =
    "id, entity_type, user_id, name, is_private, \
     content_block_id, origin, metadata, created_at, updated_at";

/// Initialize entity schema (entities, entity_relations, entity_assets tables)
pub(crate) fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        -- Entities: unified addressable layer
        CREATE TABLE IF NOT EXISTS entities (
            id TEXT PRIMARY KEY,
            entity_type TEXT NOT NULL,
            user_id TEXT REFERENCES users(id) ON DELETE CASCADE,
            name TEXT,
            is_private INTEGER NOT NULL DEFAULT 1,
            content_block_id TEXT REFERENCES content_blocks(id),
            origin TEXT,
            metadata TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_entities_user         ON entities(user_id);
        CREATE INDEX IF NOT EXISTS idx_entities_type         ON entities(entity_type, user_id);
        CREATE INDEX IF NOT EXISTS idx_entities_origin       ON entities(user_id, origin) WHERE origin IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_entities_has_content  ON entities(content_block_id) WHERE content_block_id IS NOT NULL;

        -- Entity relations: hierarchy, cross-references, grouping, tags
        -- PK is (from_id, to_id, relation); `position` orders siblings for
        -- ordered relations (e.g. structure::contained_in). `metadata` is
        -- optional JSON for relation-specific extras.
        CREATE TABLE IF NOT EXISTS entity_relations (
            from_id  TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
            to_id    TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
            relation TEXT NOT NULL,
            position INTEGER,
            metadata TEXT,
            PRIMARY KEY (from_id, to_id, relation)
        );

        CREATE INDEX IF NOT EXISTS idx_entity_relations_to       ON entity_relations(to_id, relation);
        CREATE INDEX IF NOT EXISTS idx_entity_relations_ordered  ON entity_relations(to_id, relation, position);

        -- Entity ↔ asset mapping. Used by blob GC to find entities keeping
        -- an asset alive. One row per (entity, asset) pair.
        CREATE TABLE IF NOT EXISTS entity_assets (
            entity_id TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
            asset_id  TEXT NOT NULL REFERENCES assets(id)   ON DELETE CASCADE,
            PRIMARY KEY (entity_id, asset_id)
        );

        CREATE INDEX IF NOT EXISTS idx_entity_assets_asset ON entity_assets(asset_id);
        "#,
    )?;
    Ok(())
}

/// Parse a row from the `entities` table (must be selected in `ENTITY_SELECT_COLUMNS` order).
fn parse_entity_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredEntity> {
    let id: String = row.get(0)?;
    let entity_type: String = row.get(1)?;
    let user_id: Option<String> = row.get(2)?;
    let name: Option<String> = row.get(3)?;
    let is_private: i32 = row.get(4)?;
    let content_block_id: Option<String> = row.get(5)?;
    let origin: Option<String> = row.get(6)?;
    let metadata: Option<String> = row.get(7)?;
    let created_at: i64 = row.get(8)?;
    let updated_at: i64 = row.get(9)?;

    let entity = Entity {
        entity_type: EntityType::new(entity_type),
        user_id: user_id.map(UserId::from_string),
        name,
        is_private: is_private != 0,
        content_block_id: content_block_id.map(ContentBlockId::from_string),
        origin,
        metadata: metadata.and_then(|m| serde_json::from_str(&m).ok()),
    };

    Ok(stored_editable(
        EntityId::from_string(id),
        entity,
        created_at,
        updated_at,
    ))
}

fn parse_relation_row(
    row: &rusqlite::Row<'_>,
    other_id_index: usize,
) -> rusqlite::Result<(EntityId, EntityRelation)> {
    let other_id: String = row.get(other_id_index)?;
    let relation: String = row.get(other_id_index + 1)?;
    let position: Option<i64> = row.get(other_id_index + 2)?;
    let metadata: Option<String> = row.get(other_id_index + 3)?;
    Ok((
        EntityId::from_string(other_id),
        EntityRelation {
            relation: RelationType::new(relation),
            position,
            metadata: metadata.and_then(|m| serde_json::from_str(&m).ok()),
        },
    ))
}

fn count_relations_by_entity(
    conn: &Connection,
    id_column: &str,
    ids: &[EntityId],
    relation_types: &[RelationType],
) -> Result<RelationCountMap> {
    if ids.is_empty() || relation_types.is_empty() {
        return Ok(RelationCountMap::new());
    }

    let id_placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let relation_placeholders = std::iter::repeat_n("?", relation_types.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {id_column}, relation, COUNT(*) \
         FROM entity_relations \
         WHERE {id_column} IN ({id_placeholders}) \
           AND relation IN ({relation_placeholders}) \
         GROUP BY {id_column}, relation"
    );

    let mut params_vec: Vec<Box<dyn ToSql>> =
        Vec::with_capacity(ids.len() + relation_types.len());
    for id in ids {
        params_vec.push(Box::new(id.as_str().to_string()));
    }
    for relation_type in relation_types {
        params_vec.push(Box::new(relation_type.as_str().to_string()));
    }
    let params_refs: Vec<&dyn ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params_refs.as_slice())?;
    let mut counts: RelationCountMap = HashMap::new();
    while let Some(row) = rows.next()? {
        let entity_id: String = row.get(0)?;
        let relation: String = row.get(1)?;
        let count: i64 = row.get(2)?;
        if count > 0 {
            counts
                .entry(EntityId::from_string(entity_id))
                .or_default()
                .insert(relation, count as u32);
        }
    }

    Ok(counts)
}

// ============================================================================
// EntityStore Implementation
// ============================================================================

#[async_trait]
impl EntityStore for SqliteStore {
    async fn create_entity(
        &self,
        entity_type: EntityType,
        user_id: Option<&UserId>,
    ) -> Result<EntityId> {
        let conn = self.write_conn().lock().unwrap();
        let now = unix_timestamp();
        let entity_id = EntityId::new();

        conn.execute(
            "INSERT INTO entities (id, entity_type, user_id, is_private, created_at, updated_at)
             VALUES (?1, ?2, ?3, 1, ?4, ?5)",
            params![
                entity_id.as_str(),
                entity_type.as_str(),
                user_id.map(|u| u.as_str()),
                now,
                now
            ],
        )?;

        Ok(entity_id)
    }

    async fn get_entity(&self, id: &EntityId) -> Result<Option<StoredEntity>> {
        let conn = self.read_conn().lock().unwrap();
        let sql = format!("SELECT {ENTITY_SELECT_COLUMNS} FROM entities WHERE id = ?1");
        match conn.query_row(&sql, params![id.as_str()], parse_entity_row) {
            Ok(entity) => Ok(Some(entity)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn get_entities(&self, ids: &[EntityId]) -> Result<Vec<StoredEntity>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.read_conn().lock().unwrap();
        let placeholders = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT {ENTITY_SELECT_COLUMNS} FROM entities WHERE id IN ({placeholders})"
        );

        let mut stmt = conn.prepare(&sql)?;
        let found: HashMap<String, StoredEntity> = stmt
            .query_map(
                rusqlite::params_from_iter(ids.iter().map(|id| id.as_str())),
                parse_entity_row,
            )?
            .filter_map(|r| r.ok())
            .map(|e| (e.id.as_str().to_string(), e))
            .collect();

        // Preserve caller's input order; drop missing ids.
        Ok(ids
            .iter()
            .filter_map(|id| found.get(id.as_str()).cloned())
            .collect())
    }

    async fn get_entity_by_origin(
        &self,
        user_id: &UserId,
        origin: &str,
    ) -> Result<Option<StoredEntity>> {
        let conn = self.read_conn().lock().unwrap();
        let sql = format!(
            "SELECT {ENTITY_SELECT_COLUMNS} FROM entities \
             WHERE user_id = ?1 AND origin = ?2"
        );
        match conn.query_row(&sql, params![user_id.as_str(), origin], parse_entity_row) {
            Ok(entity) => Ok(Some(entity)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn list_entities(
        &self,
        user_id: &UserId,
        entity_type: Option<&EntityType>,
    ) -> Result<Vec<StoredEntity>> {
        let conn = self.read_conn().lock().unwrap();
        let entities: Vec<StoredEntity> = match entity_type {
            Some(et) => {
                let sql = format!(
                    "SELECT {ENTITY_SELECT_COLUMNS} FROM entities \
                     WHERE user_id = ?1 AND entity_type = ?2 \
                     ORDER BY updated_at DESC"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params![user_id.as_str(), et.as_str()], parse_entity_row)?;
                rows.filter_map(|r| r.ok()).collect()
            }
            None => {
                let sql = format!(
                    "SELECT {ENTITY_SELECT_COLUMNS} FROM entities \
                     WHERE user_id = ?1 \
                     ORDER BY updated_at DESC"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params![user_id.as_str()], parse_entity_row)?;
                rows.filter_map(|r| r.ok()).collect()
            }
        };
        Ok(entities)
    }

    async fn list_entities_by_type_prefix(
        &self,
        user_id: &UserId,
        prefix: &str,
    ) -> Result<Vec<StoredEntity>> {
        let conn = self.read_conn().lock().unwrap();
        let sql = format!(
            "SELECT {ENTITY_SELECT_COLUMNS} FROM entities \
             WHERE user_id = ?1 AND entity_type LIKE ?2 \
             ORDER BY updated_at DESC"
        );
        let like_pattern = format!("{prefix}%");
        let mut stmt = conn.prepare(&sql)?;
        let entities = stmt
            .query_map(params![user_id.as_str(), &like_pattern], parse_entity_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(entities)
    }

    async fn list_entities_in_range(
        &self,
        user_id: &UserId,
        query: &EntityRangeQuery,
    ) -> Result<Vec<StoredEntity>> {
        let conn = self.read_conn().lock().unwrap();

        // Build query with optional type filter
        let (sql, type_filter): (String, Option<Vec<String>>) = match query.types_slice() {
            Some(types) if !types.is_empty() => {
                let placeholders: Vec<&str> = types.iter().map(|_| "?").collect();
                let type_list = placeholders.join(", ");
                let sql = format!(
                    "SELECT {ENTITY_SELECT_COLUMNS} FROM entities \
                     WHERE user_id = ?1 \
                       AND updated_at >= ?2 \
                       AND updated_at <= ?3 \
                       AND entity_type IN ({type_list}) \
                     ORDER BY updated_at DESC {limit}",
                    limit = query.limit.map(|l| format!("LIMIT {}", l)).unwrap_or_default()
                );
                let types_str: Vec<String> = types.iter().map(|t| t.as_str().to_string()).collect();
                (sql, Some(types_str))
            }
            _ => {
                let sql = format!(
                    "SELECT {ENTITY_SELECT_COLUMNS} FROM entities \
                     WHERE user_id = ?1 \
                       AND updated_at >= ?2 \
                       AND updated_at <= ?3 \
                     ORDER BY updated_at DESC {limit}",
                    limit = query.limit.map(|l| format!("LIMIT {}", l)).unwrap_or_default()
                );
                (sql, None)
            }
        };

        let mut stmt = conn.prepare(&sql)?;

        let entities: Vec<StoredEntity> = match &type_filter {
            Some(types) => {
                let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![
                    Box::new(user_id.as_str().to_string()),
                    Box::new(query.start),
                    Box::new(query.end),
                ];
                for t in types {
                    params_vec.push(Box::new(t.clone()));
                }
                let params_refs: Vec<&dyn rusqlite::ToSql> =
                    params_vec.iter().map(|p| p.as_ref()).collect();

                stmt.query_map(params_refs.as_slice(), parse_entity_row)?
                    .filter_map(|r| r.ok())
                    .collect()
            }
            None => stmt
                .query_map(params![user_id.as_str(), query.start, query.end], parse_entity_row)?
                .filter_map(|r| r.ok())
                .collect(),
        };

        Ok(entities)
    }

    async fn update_entity(&self, id: &EntityId, entity: &Entity) -> Result<()> {
        let conn = self.write_conn().lock().unwrap();
        let now = unix_timestamp();
        let metadata_json = entity.metadata.as_ref().map(|m| m.to_string());

        conn.execute(
            "UPDATE entities SET \
                name = ?1, is_private = ?2, \
                content_block_id = ?3, origin = ?4, metadata = ?5, updated_at = ?6 \
             WHERE id = ?7",
            params![
                entity.name,
                entity.is_private as i32,
                entity.content_block_id.as_ref().map(|c| c.as_str()),
                entity.origin,
                metadata_json,
                now,
                id.as_str()
            ],
        )?;

        Ok(())
    }

    async fn set_entity_type(&self, id: &EntityId, new_type: EntityType) -> Result<()> {
        let conn = self.write_conn().lock().unwrap();
        let now = unix_timestamp();
        conn.execute(
            "UPDATE entities SET entity_type = ?1, updated_at = ?2 WHERE id = ?3",
            params![new_type.as_str(), now, id.as_str()],
        )?;
        Ok(())
    }

    async fn delete_entity(&self, id: &EntityId) -> Result<()> {
        let conn = self.write_conn().lock().unwrap();

        // Delete relations first (both directions) — explicit, no DB cascade relied upon.
        conn.execute(
            "DELETE FROM entity_relations WHERE from_id = ?1 OR to_id = ?1",
            params![id.as_str()],
        )?;

        // Delete asset mappings for this entity.
        conn.execute(
            "DELETE FROM entity_assets WHERE entity_id = ?1",
            params![id.as_str()],
        )?;

        // Delete entity
        conn.execute("DELETE FROM entities WHERE id = ?1", params![id.as_str()])?;

        Ok(())
    }

    // ========================================================================
    // Relations
    // ========================================================================

    async fn add_relation(
        &self,
        from_id: &EntityId,
        to_id: &EntityId,
        relation: RelationType,
        position: Option<i64>,
        metadata: Option<serde_json::Value>,
    ) -> Result<()> {
        let conn = self.write_conn().lock().unwrap();
        let metadata_json = metadata.map(|m| m.to_string());

        conn.execute(
            "INSERT OR REPLACE INTO entity_relations (from_id, to_id, relation, position, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                from_id.as_str(),
                to_id.as_str(),
                relation.as_str(),
                position,
                metadata_json,
            ],
        )?;

        Ok(())
    }

    async fn get_relations_from(
        &self,
        id: &EntityId,
        relation_type: Option<&RelationType>,
    ) -> Result<Vec<(EntityId, EntityRelation)>> {
        let conn = self.read_conn().lock().unwrap();

        let results: Vec<(EntityId, EntityRelation)> = match relation_type {
            Some(rt) => {
                let mut stmt = conn.prepare(
                    "SELECT to_id, relation, position, metadata \
                     FROM entity_relations \
                     WHERE from_id = ?1 AND relation = ?2",
                )?;
                let rows = stmt
                    .query_map(params![id.as_str(), rt.as_str()], |row| parse_relation_row(row, 0))?;
                rows.filter_map(|r| r.ok()).collect()
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT to_id, relation, position, metadata \
                     FROM entity_relations \
                     WHERE from_id = ?1",
                )?;
                let rows = stmt.query_map(params![id.as_str()], |row| parse_relation_row(row, 0))?;
                rows.filter_map(|r| r.ok()).collect()
            }
        };

        Ok(results)
    }

    async fn get_relations_to(
        &self,
        id: &EntityId,
        relation_type: Option<&RelationType>,
    ) -> Result<Vec<(EntityId, EntityRelation)>> {
        let conn = self.read_conn().lock().unwrap();

        let results: Vec<(EntityId, EntityRelation)> = match relation_type {
            Some(rt) => {
                let mut stmt = conn.prepare(
                    "SELECT from_id, relation, position, metadata \
                     FROM entity_relations \
                     WHERE to_id = ?1 AND relation = ?2",
                )?;
                let rows = stmt
                    .query_map(params![id.as_str(), rt.as_str()], |row| parse_relation_row(row, 0))?;
                rows.filter_map(|r| r.ok()).collect()
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT from_id, relation, position, metadata \
                     FROM entity_relations \
                     WHERE to_id = ?1",
                )?;
                let rows = stmt.query_map(params![id.as_str()], |row| parse_relation_row(row, 0))?;
                rows.filter_map(|r| r.ok()).collect()
            }
        };

        Ok(results)
    }

    async fn count_relations_from(
        &self,
        ids: &[EntityId],
        relation_types: &[RelationType],
    ) -> Result<RelationCountMap> {
        let conn = self.read_conn().lock().unwrap();
        count_relations_by_entity(&conn, "from_id", ids, relation_types)
    }

    async fn count_relations_to(
        &self,
        ids: &[EntityId],
        relation_types: &[RelationType],
    ) -> Result<RelationCountMap> {
        let conn = self.read_conn().lock().unwrap();
        count_relations_by_entity(&conn, "to_id", ids, relation_types)
    }

    async fn list_relations_to_ordered(
        &self,
        id: &EntityId,
        relation_type: &RelationType,
    ) -> Result<Vec<(EntityId, EntityRelation)>> {
        let conn = self.read_conn().lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT from_id, relation, position, metadata \
             FROM entity_relations \
             WHERE to_id = ?1 AND relation = ?2 \
             ORDER BY position NULLS LAST, from_id",
        )?;
        let results = stmt
            .query_map(params![id.as_str(), relation_type.as_str()], |row| {
                parse_relation_row(row, 0)
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    async fn list_relations_from_ordered(
        &self,
        id: &EntityId,
        relation_type: &RelationType,
    ) -> Result<Vec<(EntityId, EntityRelation)>> {
        let conn = self.read_conn().lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT to_id, relation, position, metadata \
             FROM entity_relations \
             WHERE from_id = ?1 AND relation = ?2 \
             ORDER BY position NULLS LAST, to_id",
        )?;
        let results = stmt
            .query_map(params![id.as_str(), relation_type.as_str()], |row| {
                parse_relation_row(row, 0)
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    async fn remove_relation(
        &self,
        from_id: &EntityId,
        to_id: &EntityId,
        relation: &RelationType,
    ) -> Result<()> {
        let conn = self.write_conn().lock().unwrap();

        conn.execute(
            "DELETE FROM entity_relations WHERE from_id = ?1 AND to_id = ?2 AND relation = ?3",
            params![from_id.as_str(), to_id.as_str(), relation.as_str()],
        )?;

        Ok(())
    }

    // ========================================================================
    // Entity ↔ asset mappings
    // ========================================================================

    async fn set_entity_assets(&self, entity_id: &EntityId, asset_ids: &[AssetId]) -> Result<()> {
        let conn = self.write_conn().lock().unwrap();
        conn.execute(
            "DELETE FROM entity_assets WHERE entity_id = ?1",
            params![entity_id.as_str()],
        )?;
        for asset_id in asset_ids {
            conn.execute(
                "INSERT OR IGNORE INTO entity_assets (entity_id, asset_id) VALUES (?1, ?2)",
                params![entity_id.as_str(), asset_id.as_str()],
            )?;
        }
        Ok(())
    }

    async fn get_entity_assets(&self, entity_id: &EntityId) -> Result<Vec<AssetId>> {
        let conn = self.read_conn().lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT asset_id FROM entity_assets WHERE entity_id = ?1",
        )?;
        let assets = stmt
            .query_map(params![entity_id.as_str()], |row| {
                let s: String = row.get(0)?;
                Ok(AssetId::from_string(s))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(assets)
    }

    async fn entities_referencing_asset(&self, asset_id: &AssetId) -> Result<Vec<EntityId>> {
        let conn = self.read_conn().lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT entity_id FROM entity_assets WHERE asset_id = ?1",
        )?;
        let entities = stmt
            .query_map(params![asset_id.as_str()], |row| {
                let s: String = row.get(0)?;
                Ok(EntityId::from_string(s))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(entities)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::implementations::sqlite::SqliteStore;
    use crate::storage::traits::EntityStore;

    #[tokio::test]
    async fn test_create_and_get_entity() {
        let store = SqliteStore::in_memory().unwrap();
        let user_id = UserId::new();

        // Create user first (required for FK)
        {
            let conn = store.conn().lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, email, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                params![user_id.as_str(), "test@example.com", 1000, 1000],
            ).unwrap();
        }

        // Create entity
        let entity_id = store
            .create_entity(EntityType::conversation(), Some(&user_id))
            .await
            .unwrap();

        // Get entity
        let entity = store.get_entity(&entity_id).await.unwrap().unwrap();
        assert_eq!(entity.entity_type.as_str(), "conversation");
        assert_eq!(entity.user_id.as_ref().map(|u| u.as_str()), Some(user_id.as_str()));
        assert!(entity.is_private); // Default
        assert!(entity.content_block_id.is_none());
        assert!(entity.origin.is_none());
    }

    #[tokio::test]
    async fn test_update_entity_with_content_and_origin() {
        let store = SqliteStore::in_memory().unwrap();

        let entity_id = store
            .create_entity(EntityType::document_tabbed(), None)
            .await
            .unwrap();

        // Insert a content block manually to satisfy the FK
        let block_id = ContentBlockId::new();
        {
            let conn = store.conn().lock().unwrap();
            conn.execute(
                "INSERT INTO content_blocks (id, content_hash, content_type, text, is_private, origin_kind, created_at) \
                 VALUES (?1, 'deadbeef', 'markdown', 'hello', 0, 'system', 1000)",
                params![block_id.as_str()],
            ).unwrap();
        }

        // Update entity: set name, content_block_id, origin
        let mut entity = store.get_entity(&entity_id).await.unwrap().unwrap();
        entity.name = Some("Project Plan".to_string());
        entity.content_block_id = Some(block_id.clone());
        entity.origin = Some("google_drive:gdoc-abc123".to_string());
        store.update_entity(&entity_id, &entity).await.unwrap();

        // Verify update
        let updated = store.get_entity(&entity_id).await.unwrap().unwrap();
        assert_eq!(updated.name.as_deref(), Some("Project Plan"));
        assert_eq!(updated.content_block_id.as_ref(), Some(&block_id));
        assert_eq!(updated.origin.as_deref(), Some("google_drive:gdoc-abc123"));
    }

    #[tokio::test]
    async fn test_get_entity_by_origin() {
        let store = SqliteStore::in_memory().unwrap();
        let user_id = UserId::new();
        {
            let conn = store.conn().lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, email, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                params![user_id.as_str(), "test@example.com", 1000, 1000],
            ).unwrap();
        }

        let entity_id = store
            .create_entity(EntityType::document_tabbed(), Some(&user_id))
            .await
            .unwrap();
        let mut entity = store.get_entity(&entity_id).await.unwrap().unwrap();
        entity.origin = Some("google_drive:abc".to_string());
        store.update_entity(&entity_id, &entity).await.unwrap();

        let found = store
            .get_entity_by_origin(&user_id, "google_drive:abc")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, entity_id);

        let missing = store
            .get_entity_by_origin(&user_id, "google_drive:xyz")
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_list_entities_by_type_prefix() {
        let store = SqliteStore::in_memory().unwrap();
        let user_id = UserId::new();
        {
            let conn = store.conn().lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, email, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                params![user_id.as_str(), "test@example.com", 1000, 1000],
            ).unwrap();
        }

        store.create_entity(EntityType::document_tabbed(), Some(&user_id)).await.unwrap();
        store.create_entity(EntityType::document_note(), Some(&user_id)).await.unwrap();
        store.create_entity(EntityType::conversation(), Some(&user_id)).await.unwrap();

        let docs = store
            .list_entities_by_type_prefix(&user_id, "document::")
            .await
            .unwrap();
        assert_eq!(docs.len(), 2);

        let all_sys = store
            .list_entities_by_type_prefix(&user_id, "system::")
            .await
            .unwrap();
        assert!(all_sys.is_empty());
    }

    #[tokio::test]
    async fn test_delete_entity_clears_relations_and_assets() {
        let store = SqliteStore::in_memory().unwrap();

        let entity1 = store.create_entity(EntityType::document_tabbed(), None).await.unwrap();
        let entity2 = store.create_entity(EntityType::document_tab(), None).await.unwrap();

        store
            .add_relation(
                &entity2,
                &entity1,
                RelationType::structure_contained_in(),
                Some(0),
                None,
            )
            .await
            .unwrap();

        store.delete_entity(&entity1).await.unwrap();

        let entity = store.get_entity(&entity1).await.unwrap();
        assert!(entity.is_none());

        let relations = store.get_relations_from(&entity2, None).await.unwrap();
        assert!(relations.is_empty());
    }

    #[tokio::test]
    async fn test_ordered_relations() {
        let store = SqliteStore::in_memory().unwrap();

        let doc = store.create_entity(EntityType::document_tabbed(), None).await.unwrap();
        let tab_a = store.create_entity(EntityType::document_tab(), None).await.unwrap();
        let tab_b = store.create_entity(EntityType::document_tab(), None).await.unwrap();
        let tab_c = store.create_entity(EntityType::document_tab(), None).await.unwrap();

        store.add_relation(&tab_b, &doc, RelationType::structure_contained_in(), Some(1), None).await.unwrap();
        store.add_relation(&tab_a, &doc, RelationType::structure_contained_in(), Some(0), None).await.unwrap();
        store.add_relation(&tab_c, &doc, RelationType::structure_contained_in(), Some(2), None).await.unwrap();

        let children = store
            .list_relations_to_ordered(&doc, &RelationType::structure_contained_in())
            .await
            .unwrap();
        let ids: Vec<_> = children.iter().map(|(id, _)| id.clone()).collect();
        assert_eq!(ids, vec![tab_a, tab_b, tab_c]);
    }

    #[tokio::test]
    async fn test_relation_counts_batch_by_direction() {
        let store = SqliteStore::in_memory().unwrap();

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

    #[tokio::test]
    async fn test_forked_from_relation_with_metadata() {
        let store = SqliteStore::in_memory().unwrap();

        let original = store.create_entity(EntityType::conversation(), None).await.unwrap();
        let fork = store.create_entity(EntityType::conversation(), None).await.unwrap();

        let metadata = serde_json::json!({"at_turn_id": "turn-123"});
        store
            .add_relation(
                &fork,
                &original,
                RelationType::conversation_forked_from(),
                None,
                Some(metadata.clone()),
            )
            .await
            .unwrap();

        let to_relations = store
            .get_relations_to(&original, Some(&RelationType::conversation_forked_from()))
            .await
            .unwrap();
        assert_eq!(to_relations.len(), 1);
        assert_eq!(to_relations[0].0, fork);
        assert_eq!(to_relations[0].1.metadata, Some(metadata));
    }

    #[tokio::test]
    async fn test_entity_assets_roundtrip() {
        let store = SqliteStore::in_memory().unwrap();

        let entity = store.create_entity(EntityType::document_tab(), None).await.unwrap();
        let asset_a = AssetId::from_string("asset-a");
        let asset_b = AssetId::from_string("asset-b");

        // Insert the assets manually (FK)
        {
            let conn = store.conn().lock().unwrap();
            conn.execute(
                "INSERT INTO assets (id, blob_hash, mime_type, size_bytes, created_at) \
                 VALUES (?1, 'h1', 'image/png', 100, 1)",
                params![asset_a.as_str()],
            ).unwrap();
            conn.execute(
                "INSERT INTO assets (id, blob_hash, mime_type, size_bytes, created_at) \
                 VALUES (?1, 'h2', 'image/png', 200, 1)",
                params![asset_b.as_str()],
            ).unwrap();
        }

        store
            .set_entity_assets(&entity, &[asset_a.clone(), asset_b.clone()])
            .await
            .unwrap();

        let got = store.get_entity_assets(&entity).await.unwrap();
        assert_eq!(got.len(), 2);

        let users_of_a = store.entities_referencing_asset(&asset_a).await.unwrap();
        assert_eq!(users_of_a, vec![entity.clone()]);

        // Re-set to just asset_b — asset_a should be removed from the mapping.
        store.set_entity_assets(&entity, std::slice::from_ref(&asset_b)).await.unwrap();
        let got = store.get_entity_assets(&entity).await.unwrap();
        assert_eq!(got, vec![asset_b]);
        let users_of_a = store.entities_referencing_asset(&asset_a).await.unwrap();
        assert!(users_of_a.is_empty());
    }
}
