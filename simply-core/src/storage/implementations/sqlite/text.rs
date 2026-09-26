//! SQLite implementation of TextStore

use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection};

use super::SqliteStore;
use crate::storage::helper::{content_hash, unix_timestamp};
use crate::storage::ids::ContentBlockId;
use crate::storage::traits::{StoredTextBlock, TextStore};
use crate::storage::types::{stored, ContentBlock, ContentOrigin, ContentType, Hashed, OriginKind};

/// Initialize the content_blocks schema
pub(crate) fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        -- Content blocks (content-addressed text storage)
        CREATE TABLE IF NOT EXISTS content_blocks (
            id TEXT PRIMARY KEY,
            content_hash TEXT NOT NULL,
            content_type TEXT NOT NULL DEFAULT 'plain',
            text TEXT NOT NULL,
            is_private INTEGER NOT NULL DEFAULT 0,
            origin_kind TEXT,
            origin_user_id TEXT,
            origin_model_id TEXT,
            origin_source_id TEXT,
            origin_parent_id TEXT REFERENCES content_blocks(id),
            created_at INTEGER NOT NULL
        );

        -- Index for deduplication lookups
        CREATE INDEX IF NOT EXISTS idx_content_blocks_hash
            ON content_blocks(content_hash);

        -- Index for origin queries
        CREATE INDEX IF NOT EXISTS idx_content_blocks_origin
            ON content_blocks(origin_kind, origin_user_id);

        -- Index for privacy filtering
        CREATE INDEX IF NOT EXISTS idx_content_blocks_private
            ON content_blocks(is_private) WHERE is_private = 1;

        -- Index for temporal queries
        CREATE INDEX IF NOT EXISTS idx_content_blocks_created
            ON content_blocks(created_at);
        "#,
    )
    .context("Failed to initialize content_blocks schema")?;
    Ok(())
}

#[async_trait]
impl TextStore for SqliteStore {
    async fn store(&self, content: ContentBlock) -> Result<ContentBlockId> {
        let hash = content_hash(&content.text);
        let conn = self.write_conn().lock().unwrap();

        let id = ContentBlockId::new();
        let now = unix_timestamp();

        conn.execute(
            "INSERT INTO content_blocks (id, content_hash, content_type, text, is_private, origin_kind, origin_user_id, origin_model_id, origin_source_id, origin_parent_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id.as_str(),
                &hash,
                content.content_type.as_str(),
                &content.text,
                content.is_private as i32,
                content.origin.kind().as_str(),
                content.origin.user_id().map(|id| id.as_str()),
                content.origin.model_id(),
                content.origin.source_id(),
                content.origin.parent_id().map(|id| id.as_str()),
                now,
            ],
        )
        .context("Failed to insert content block")?;

        Ok(id)
    }

    async fn get(&self, id: &ContentBlockId) -> Result<Option<StoredTextBlock>> {
        let conn = self.read_conn().lock().unwrap();

        let result = conn.query_row(
            "SELECT id, content_hash, content_type, text, is_private, origin_kind, origin_user_id, origin_model_id, origin_source_id, origin_parent_id, created_at
             FROM content_blocks WHERE id = ?1",
            params![id.as_str()],
            |row| {
                Ok(RowData {
                    id: row.get(0)?,
                    content_hash: row.get(1)?,
                    content_type: row.get(2)?,
                    text: row.get(3)?,
                    is_private: row.get(4)?,
                    origin_kind: row.get(5)?,
                    origin_user_id: row.get(6)?,
                    origin_model_id: row.get(7)?,
                    origin_source_id: row.get(8)?,
                    origin_parent_id: row.get(9)?,
                    created_at: row.get(10)?,
                })
            },
        );

        match result {
            Ok(data) => Ok(Some(data.into_stored_content_block()?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("Failed to get content block"),
        }
    }

    async fn get_text(&self, id: &ContentBlockId) -> Result<Option<String>> {
        let conn = self.read_conn().lock().unwrap();

        let result = conn.query_row(
            "SELECT text FROM content_blocks WHERE id = ?1",
            params![id.as_str()],
            |row| row.get(0),
        );

        match result {
            Ok(text) => Ok(Some(text)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("Failed to get content block text"),
        }
    }

    async fn get_texts(
        &self,
        ids: &[ContentBlockId],
    ) -> Result<std::collections::HashMap<ContentBlockId, String>> {
        use std::collections::HashMap;
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.read_conn().lock().unwrap();
        let placeholders = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, text FROM content_blocks WHERE id IN ({placeholders})"
        );

        let mut stmt = conn
            .prepare(&sql)
            .context("Failed to prepare get_texts query")?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(ids.iter().map(|id| id.as_str())),
                |row| {
                    let id: String = row.get(0)?;
                    let text: Option<String> = row.get(1)?;
                    Ok((id, text))
                },
            )
            .context("Failed to query content_blocks")?;

        let mut out = HashMap::with_capacity(ids.len());
        for row in rows {
            let (id, text) = row?;
            if let Some(t) = text {
                out.insert(ContentBlockId::from_string(id), t);
            }
        }
        Ok(out)
    }

    async fn exists(&self, id: &ContentBlockId) -> Result<bool> {
        let conn = self.read_conn().lock().unwrap();

        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM content_blocks WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .context("Failed to check content block existence")?;

        Ok(count > 0)
    }
}

/// Helper struct for reading row data
struct RowData {
    id: String,
    content_hash: String,
    content_type: String,
    text: String,
    is_private: i32,
    origin_kind: Option<String>,
    origin_user_id: Option<String>,
    origin_model_id: Option<String>,
    origin_source_id: Option<String>,
    origin_parent_id: Option<String>,
    created_at: i64,
}

impl RowData {
    fn into_stored_content_block(self) -> Result<StoredTextBlock> {
        let content_type = ContentType::from_str(&self.content_type)
            .ok_or_else(|| anyhow::anyhow!("Invalid content type: {}", self.content_type))?;

        let origin_kind = self
            .origin_kind
            .as_deref()
            .map(|s| {
                OriginKind::from_str(s)
                    .ok_or_else(|| anyhow::anyhow!("Invalid origin kind: {}", s))
            })
            .transpose()?
            .unwrap_or(OriginKind::System);

        let origin = ContentOrigin::from_db(
            origin_kind,
            self.origin_user_id,
            self.origin_model_id,
            self.origin_source_id,
            self.origin_parent_id,
        );

        Ok(stored(
            ContentBlockId::from_string(self.id),
            Hashed::new(
                self.content_hash,
                ContentBlock {
                    text: self.text,
                    content_type,
                    is_private: self.is_private != 0,
                    origin,
                },
            ),
            self.created_at,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_schema_creation() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        // Verify table exists
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='content_blocks'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Verify indexes exist
        let index_count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_content_blocks%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 4);
    }

    #[test]
    fn test_content_hash() {
        let hash1 = content_hash("Hello, world!");
        let hash2 = content_hash("Hello, world!");
        let hash3 = content_hash("Different content");

        assert_eq!(hash1, hash2, "Same content should produce same hash");
        assert_ne!(hash1, hash3, "Different content should produce different hash");
        assert_eq!(hash1.len(), 64, "SHA-256 hash should be 64 hex chars");
    }

    #[tokio::test]
    async fn test_store_and_get() {
        let store = SqliteStore::in_memory().unwrap();

        let content = ContentBlock::plain("Test content");
        let id = store.store(content).await.unwrap();

        let stored = store.get(&id).await.unwrap().unwrap();
        assert_eq!(stored.text(), "Test content");
        assert_eq!(stored.content_type(), &ContentType::Plain);
        assert!(!stored.is_private());
    }

    #[tokio::test]
    async fn test_no_deduplication() {
        // Each ContentBlock gets its own ID even with same text,
        // because metadata (origin, content_type, is_private) may differ
        let store = SqliteStore::in_memory().unwrap();

        let id1 = store.store(ContentBlock::plain("Duplicate me")).await.unwrap();
        let id2 = store.store(ContentBlock::plain("Duplicate me")).await.unwrap();

        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn test_get_text() {
        let store = SqliteStore::in_memory().unwrap();

        let content = ContentBlock::markdown("# Header");
        let id = store.store(content).await.unwrap();

        let text = store.get_text(&id).await.unwrap().unwrap();
        assert_eq!(text, "# Header");
    }

    #[tokio::test]
    async fn test_exists() {
        let store = SqliteStore::in_memory().unwrap();

        let content = ContentBlock::plain("Exists test");
        let id = store.store(content).await.unwrap();

        assert!(store.exists(&id).await.unwrap());
        assert!(!store.exists(&ContentBlockId::new()).await.unwrap());
    }

    #[tokio::test]
    async fn test_content_with_origin() {
        use crate::storage::ids::UserId;

        let store = SqliteStore::in_memory().unwrap();

        let content = ContentBlock::markdown("User content")
            .with_origin(ContentOrigin::user(UserId::from_string("user-123")));

        let id = store.store(content).await.unwrap();
        let stored = store.get(&id).await.unwrap().unwrap();

        assert_eq!(stored.origin().kind(), OriginKind::User);
        assert_eq!(
            stored.origin().user_id().map(|id| id.as_str()),
            Some("user-123")
        );
    }

    #[tokio::test]
    async fn test_private_content() {
        let store = SqliteStore::in_memory().unwrap();

        let content = ContentBlock::plain("Private data").private();
        let id = store.store(content).await.unwrap();

        let stored = store.get(&id).await.unwrap().unwrap();
        assert!(stored.is_private());
    }
}
