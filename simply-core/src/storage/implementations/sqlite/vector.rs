//! SQLite-vec implementation of VectorStore.
//!
//! Chunks key on `content_block_id`; `entity_id` + `entity_kind` + `title`
//! are denormalised so filters (e.g. `entity_kind LIKE 'document::%'`)
//! are a single predicate with no joins.

use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection};

use super::SqliteStore;
use crate::embedding::{SearchQuery, SearchResult, VectorChunk, VectorStore};
use crate::storage::ids::{ChunkId, ContentBlockId, EntityId};

/// Register sqlite-vec as an auto-extension. Call once before opening any connections.
pub fn register_sqlite_vec() {
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(
            std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::ffi::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> std::ffi::c_int,
            >(sqlite_vec::sqlite3_vec_init as *const ())
        ));
    }
}

pub(crate) fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        -- Chunk metadata. Only fields that feed the SQL filter
        -- (entity_kind) or index identity (entity_id / content_block_id)
        -- live here. Title, user/owner, access rules, chunk text — all
        -- resolved from the entity's current state at query time, so
        -- renames, visibility flips, or policy changes don't need a
        -- reindex.
        CREATE TABLE IF NOT EXISTS vector_chunks (
            id TEXT PRIMARY KEY,
            content_block_id TEXT NOT NULL,
            entity_id TEXT NOT NULL,
            entity_kind TEXT NOT NULL,
            model_id TEXT NOT NULL,
            embedded_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_chunks_kind   ON vector_chunks(entity_kind);
        CREATE INDEX IF NOT EXISTS idx_chunks_entity ON vector_chunks(entity_id);
        CREATE INDEX IF NOT EXISTS idx_chunks_block  ON vector_chunks(content_block_id);
        "#,
    )
    .context("Failed to initialize vector_chunks schema")?;

    Ok(())
}

/// Initialize the vec0 virtual table for a given dimension.
/// Called lazily when the first embedding is stored, since dimensions
/// depend on the configured provider.
pub(crate) fn ensure_vec_table(conn: &Connection, dimensions: usize) -> Result<()> {
    let sql = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS vector_chunks_vec USING vec0(id TEXT PRIMARY KEY, embedding float[{dimensions}])"
    );
    conn.execute_batch(&sql)
        .context("Failed to create vector_chunks_vec table")?;
    Ok(())
}

fn parse_chunk(row: &rusqlite::Row<'_>) -> rusqlite::Result<VectorChunk> {
    Ok(VectorChunk {
        id: ChunkId::from_string(row.get::<_, String>(0)?),
        content_block_id: ContentBlockId::from_string(row.get::<_, String>(1)?),
        entity_id: EntityId::from_string(row.get::<_, String>(2)?),
        entity_kind: row.get(3)?,
        embedding: vec![], // not loaded from metadata table
    })
}

#[async_trait]
impl VectorStore for SqliteStore {
    async fn upsert(&self, chunks: &[VectorChunk]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }

        let conn = self.write_conn().lock().unwrap();

        // Ensure the vec0 table exists with the right dimensions
        let dimensions = chunks[0].embedding.len();
        ensure_vec_table(&conn, dimensions)?;

        let now = crate::storage::helper::unix_timestamp();

        for chunk in chunks {
            conn.execute(
                "INSERT OR REPLACE INTO vector_chunks \
                 (id, content_block_id, entity_id, entity_kind, model_id, embedded_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    chunk.id.as_str(),
                    chunk.content_block_id.as_str(),
                    chunk.entity_id.as_str(),
                    &chunk.entity_kind,
                    "", // model_id set by caller
                    now,
                ],
            )?;

            let embedding_bytes: Vec<u8> = chunk
                .embedding
                .iter()
                .flat_map(|f| f.to_le_bytes())
                .collect();

            conn.execute(
                "INSERT OR REPLACE INTO vector_chunks_vec (id, embedding) VALUES (?1, ?2)",
                params![chunk.id.as_str(), embedding_bytes],
            )?;
        }

        Ok(())
    }

    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchResult>> {
        let conn = self.read_conn().lock().unwrap();

        let query_bytes: Vec<u8> = query
            .vector
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();

        // KNN search via vec0. The filter predicate is applied after the
        // metadata fetch; sqlite-vec's MATCH doesn't see our columns.
        let mut stmt = conn.prepare(
            "SELECT v.id, v.distance
             FROM vector_chunks_vec v
             WHERE v.embedding MATCH ?1
             ORDER BY v.distance
             LIMIT ?2",
        )?;

        let candidates: Vec<(String, f64)> = stmt
            .query_map(params![query_bytes, query.top_k as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Walk matches, apply user + entity-kind filters, and dedupe by
        // `content_block_id` keeping the best score. Two chunks from the
        // same block shouldn't turn into two hits — callers inject the
        // full block either way.
        let mut seen_blocks: std::collections::HashMap<String, SearchResult> =
            std::collections::HashMap::new();
        for (chunk_id, distance) in candidates {
            let chunk = match conn.query_row(
                "SELECT id, content_block_id, entity_id, entity_kind
                 FROM vector_chunks WHERE id = ?1",
                params![&chunk_id],
                parse_chunk,
            ) {
                Ok(c) => c,
                Err(_) => continue,
            };

            if let Some(ref filter) = query.filter {
                if let Some(ref ef) = filter.entity_filter {
                    if !ef.matches(&chunk.entity_kind) {
                        continue;
                    }
                }
            }

            let score = 1.0 / (1.0 + distance as f32);
            let block_key = chunk.content_block_id.as_str().to_string();
            match seen_blocks.get(&block_key) {
                Some(existing) if existing.score >= score => { /* keep existing */ }
                _ => { seen_blocks.insert(block_key, SearchResult { chunk, score }); }
            }
        }

        let mut results: Vec<SearchResult> = seen_blocks.into_values().collect();
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        Ok(results)
    }

    async fn delete_by_content_block(&self, content_block_id: &ContentBlockId) -> Result<()> {
        let conn = self.write_conn().lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id FROM vector_chunks WHERE content_block_id = ?1",
        )?;
        let ids: Vec<String> = stmt
            .query_map(params![content_block_id.as_str()], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        for id in &ids {
            let _ = conn.execute(
                "DELETE FROM vector_chunks_vec WHERE id = ?1",
                params![id],
            );
        }
        conn.execute(
            "DELETE FROM vector_chunks WHERE content_block_id = ?1",
            params![content_block_id.as_str()],
        )?;
        Ok(())
    }

    async fn delete_by_entity(&self, entity_id: &EntityId) -> Result<()> {
        let conn = self.write_conn().lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id FROM vector_chunks WHERE entity_id = ?1",
        )?;
        let ids: Vec<String> = stmt
            .query_map(params![entity_id.as_str()], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        for id in &ids {
            let _ = conn.execute(
                "DELETE FROM vector_chunks_vec WHERE id = ?1",
                params![id],
            );
        }
        conn.execute(
            "DELETE FROM vector_chunks WHERE entity_id = ?1",
            params![entity_id.as_str()],
        )?;
        Ok(())
    }

    async fn delete_all(&self) -> Result<()> {
        let conn = self.write_conn().lock().unwrap();
        conn.execute("DELETE FROM vector_chunks_vec", [])?;
        conn.execute("DELETE FROM vector_chunks", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::embedding::{EntityFilter, EntityTypeMatcher};

    #[test]
    fn entity_filter_matches_include_empty() {
        let f = EntityFilter::default();
        assert!(f.matches("document::note"));
        assert!(f.matches("conversation"));
    }

    #[test]
    fn entity_filter_matches_prefix_include() {
        let f = EntityFilter {
            include: vec![EntityTypeMatcher::Prefix("document::".to_string())],
            exclude: vec![],
        };
        assert!(f.matches("document::note"));
        assert!(f.matches("document::tab"));
        assert!(!f.matches("conversation"));
    }

    #[test]
    fn entity_filter_excludes_override_includes() {
        let f = EntityFilter {
            include: vec![EntityTypeMatcher::Prefix("document::".to_string())],
            exclude: vec![EntityTypeMatcher::Exact("document::system_prompt".to_string())],
        };
        assert!(f.matches("document::note"));
        assert!(!f.matches("document::system_prompt"));
    }
}
