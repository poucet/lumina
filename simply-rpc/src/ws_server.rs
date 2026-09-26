//! WebSocket server — generic RPC server over WebSocket.
//!
//! Knows nothing about specific API traits. Takes a dispatch function
//! that the caller wires up with the appropriate services.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::*;

/// Dispatch function signature.
///
/// Called for each incoming RPC request. The `write_tx` sender allows the
/// dispatch logic to send async notifications (e.g. session event streams).
///
/// Built by the caller (e.g. `discovery.rs`) from specific services.
pub type DispatchFn = Arc<
    dyn Fn(
            String,                    // method
            serde_json::Value,         // params
            mpsc::Sender<String>,      // write channel for notifications
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = WsResponse> + Send>>
        + Send
        + Sync,
>;

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

    async fn add(&self, addr: SocketAddr) -> u64 {
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

    async fn remove(&self, id: u64) {
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

/// Starts a WebSocket server on the given port using the provided dispatch function.
pub async fn start(dispatch: DispatchFn, port: u16) -> anyhow::Result<ServerHandle> {
    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(port, "WebSocket server listening");

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let tracker = ConnectionTracker::new();
    let tracker_clone = tracker.clone();

    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            tracing::info!(%addr, "WS client connected");
                            let dispatch = Arc::clone(&dispatch);
                            let tracker = tracker_clone.clone();
                            tokio::spawn(async move {
                                let conn_id = tracker.add(addr).await;
                                handle_connection(dispatch, stream, &tracker, conn_id).await;
                                tracker.remove(conn_id).await;
                            });
                        }
                        Err(e) => tracing::error!(error = %e, "WS accept error"),
                    }
                }
            }
        }
    });

    Ok(ServerHandle { _task: handle, _shutdown: shutdown_tx, port, tracker })
}

pub struct ServerHandle {
    _task: JoinHandle<()>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    port: u16,
    tracker: ConnectionTracker,
}

impl ServerHandle {
    pub fn port(&self) -> u16 { self.port }

    /// Get the connection tracker for admin endpoints.
    pub fn tracker(&self) -> &ConnectionTracker {
        &self.tracker
    }
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

async fn handle_connection(dispatch: DispatchFn, stream: tokio::net::TcpStream, tracker: &ConnectionTracker, conn_id: u64) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => { tracing::error!(error = %e, "WS handshake failed"); return; }
    };

    let (ws_sink, mut ws_source) = ws_stream.split();

    let (write_tx, mut write_rx) = mpsc::channel::<String>(256);
    let writer_handle = tokio::spawn(async move {
        let mut sink = ws_sink;
        while let Some(text) = write_rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() { break; }
        }
    });

    while let Some(msg) = ws_source.next().await {
        let msg = match msg {
            Ok(Message::Text(text)) => text.to_string(),
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(e) => { tracing::error!(error = %e, "WS read error"); break; }
        };

        let incoming: WsIncoming = match serde_json::from_str(&msg) {
            Ok(v) => v,
            Err(e) => { tracing::warn!(error = %e, "invalid WS message"); continue; }
        };

        if !incoming.is_request() { continue; }

        let id = incoming.id.unwrap();
        let method = incoming.method.unwrap();
        let params = incoming.params;

        // Built-in: client identification (not dispatched to services)
        if method == "client.identify" {
            let name = params.as_str()
                .or_else(|| params.get("name").and_then(|v| v.as_str()))
                .unwrap_or("unknown");
            tracker.set_name(conn_id, name.to_string()).await;
            tracing::info!(conn_id, name, "WS client identified");
            let response = WsResponse::ok(id, serde_json::json!({ "ok": true }));
            let text = serde_json::to_string(&response).unwrap_or_default();
            if write_tx.send(text).await.is_err() { break; }
            continue;
        }

        tracing::debug!(id, method = %method, "WS request");
        tracing::trace!(id, method = %method, params = %params, "WS request params");

        let mut response = dispatch(method.clone(), params, write_tx.clone()).await;
        response.id = id; // Stamp the request ID on the response

        let is_err = response.error.is_some();
        tracing::debug!(id, method = %method, error = is_err, "WS response");
        if is_err {
            tracing::debug!(id, error = ?response.error, "WS response error");
        }

        let text = serde_json::to_string(&response).unwrap_or_default();
        if write_tx.send(text).await.is_err() { break; }
    }

    writer_handle.abort();
    tracing::info!("WS client disconnected");
}
