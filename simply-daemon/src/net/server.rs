//! WebSocket types — dispatch function, connection tracking.
//!
//! The actual WebSocket handler lives in `rest.rs` (axum upgrade).
//! This module provides shared types used by both the server and clients.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use serde::Serialize;
use tokio::sync::Mutex;


/// Info about a connected WS client.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionInfo {
    pub id: u64,
    pub addr: String,
    pub connected_at: String,
    /// Client name (e.g. "noema", "lumina"). Set from the first RPC method prefix.
    pub name: Option<String>,
}

/// Tracks active WebSocket connections. Shared with the REST admin page.
#[derive(Debug, Clone)]
pub struct ConnectionTracker {
    connections: Arc<Mutex<HashMap<u64, ConnectionInfo>>>,
    next_id: Arc<AtomicU64>,
}

impl Default for ConnectionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionTracker {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub async fn add(&self, addr: SocketAddr) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let info = ConnectionInfo {
            id,
            addr: addr.to_string(),
            connected_at: Utc::now().to_rfc3339(),
            name: None,
        };
        self.connections.lock().await.insert(id, info);
        id
    }

    pub async fn remove(&self, id: u64) {
        self.connections.lock().await.remove(&id);
    }

    /// Set the client name for a connection (identified from first RPC call).
    pub async fn set_name(&self, id: u64, name: String) {
        if let Some(info) = self.connections.lock().await.get_mut(&id) {
            info.name = Some(name);
        }
    }

    /// List all active connections.
    pub async fn list(&self) -> Vec<ConnectionInfo> {
        self.connections.lock().await.values().cloned().collect()
    }
}
