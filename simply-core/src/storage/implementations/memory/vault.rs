//! In-memory VaultStore implementation.

use anyhow::{bail, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::storage::ids::{EntityId, VaultConflictId};
use crate::storage::traits::VaultStore;
use crate::storage::types::{VaultConflict, VaultFile, VaultSyncStatus};

#[derive(Debug, Default)]
pub struct MemoryVaultStore {
    files: Mutex<HashMap<String, VaultFile>>,
    conflicts: Mutex<HashMap<String, VaultConflict>>,
}

impl MemoryVaultStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl VaultStore for MemoryVaultStore {
    async fn upsert_vault_file(&self, file: &VaultFile) -> Result<()> {
        let mut files = self.files.lock().unwrap();
        if files.values().any(|existing| {
            existing.entity_id.as_str() != file.entity_id.as_str()
                && existing.path.as_str() == file.path.as_str()
        }) {
            bail!(
                "vault path already belongs to another entity: {}",
                file.path.as_str()
            );
        }
        files.insert(file.entity_id.as_str().to_string(), file.clone());
        Ok(())
    }

    async fn get_vault_file(&self, entity_id: &EntityId) -> Result<Option<VaultFile>> {
        Ok(self.files.lock().unwrap().get(entity_id.as_str()).cloned())
    }

    async fn get_vault_file_by_path(&self, path: &str) -> Result<Option<VaultFile>> {
        Ok(self
            .files
            .lock()
            .unwrap()
            .values()
            .find(|file| file.path.as_str() == path)
            .cloned())
    }

    async fn list_vault_files(&self, status: Option<&VaultSyncStatus>) -> Result<Vec<VaultFile>> {
        let mut files: Vec<_> = self
            .files
            .lock()
            .unwrap()
            .values()
            .filter(|file| status.is_none_or(|s| &file.sync_status == s))
            .cloned()
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    async fn delete_vault_file(&self, entity_id: &EntityId) -> Result<()> {
        self.files.lock().unwrap().remove(entity_id.as_str());
        Ok(())
    }

    async fn insert_vault_conflict(&self, conflict: &VaultConflict) -> Result<()> {
        self.conflicts
            .lock()
            .unwrap()
            .insert(conflict.id.as_str().to_string(), conflict.clone());
        Ok(())
    }

    async fn list_vault_conflicts(&self) -> Result<Vec<VaultConflict>> {
        let mut conflicts: Vec<_> = self.conflicts.lock().unwrap().values().cloned().collect();
        conflicts.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(conflicts)
    }

    async fn delete_vault_conflict(&self, id: &VaultConflictId) -> Result<bool> {
        Ok(self.conflicts.lock().unwrap().remove(id.as_str()).is_some())
    }

    async fn clear_vault_conflicts_for_path(&self, path: &str) -> Result<()> {
        self.conflicts
            .lock()
            .unwrap()
            .retain(|_, conflict| conflict.path.as_str() != path);
        Ok(())
    }
}
