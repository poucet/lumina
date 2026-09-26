//! WebSocket-registered tool providers.
//!
//! Clients register tools over the WS connection. When a tool needs to be
//! called, the daemon sends a reverse RPC call to the client over the same WS.
//! Tools are automatically unregistered when the connection drops.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use llm::{ToolDefinition, ToolResultContent};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};

/// A pending reverse RPC call — waiting for the client to respond.
struct PendingCall {
    tx: oneshot::Sender<Result<serde_json::Value>>,
}

/// A tool provider connected over WebSocket.
struct WsToolProvider {
    /// Tool definitions this client provides.
    tools: Vec<ToolDefinition>,
    /// Channel to send outgoing messages to this client's WS connection.
    write_tx: mpsc::Sender<String>,
    /// Pending reverse RPC calls awaiting responses.
    pending: Arc<Mutex<HashMap<u64, PendingCall>>>,
    /// Next request ID for reverse calls.
    next_id: std::sync::atomic::AtomicU64,
}

impl WsToolProvider {
    /// Send a reverse RPC call to the client and wait for the response.
    ///
    /// `ctx` is forwarded as `__ctx` so the remote skill sees the same
    /// RequestContext the daemon received (user, tokens, metadata).
    async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &simply_rpc::RequestContext,
    ) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Register the pending call
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, PendingCall { tx });

        let params = serde_json::json!({
            "name": name,
            "arguments": arguments,
            "__ctx": serde_json::to_value(ctx).unwrap_or_default(),
        });
        let request = serde_json::json!({
            "id": id,
            "method": "tools.call",
            "params": params,
        });
        self.write_tx.send(serde_json::to_string(&request)?).await
            .map_err(|_| anyhow::anyhow!("WS connection closed"))?;

        // Wait for response (with timeout)
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            rx,
        ).await
            .map_err(|_| anyhow::anyhow!("tool call timed out"))?
            .map_err(|_| anyhow::anyhow!("WS connection dropped during tool call"))?;

        result
    }

    /// Handle a response from the client for a pending reverse call.
    fn handle_response(&self, id: u64, result: Result<serde_json::Value>) {
        if let Ok(mut pending) = self.pending.try_lock() {
            if let Some(call) = pending.remove(&id) {
                let _ = call.tx.send(result);
            }
        }
    }
}

/// Registry of WS-connected tool providers.
///
/// Thread-safe, shared between WS handlers and the tool dispatch pipeline.
pub struct WsToolRegistry {
    /// connection_id → provider
    providers: RwLock<HashMap<u64, WsToolProvider>>,
    next_conn_id: std::sync::atomic::AtomicU64,
}

impl Default for WsToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WsToolRegistry {
    pub fn new() -> Self {
        Self {
            providers: RwLock::new(HashMap::new()),
            next_conn_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Register tools from a WS client. Returns a connection ID for unregistration.
    pub async fn register(
        &self,
        tools: Vec<ToolDefinition>,
        write_tx: mpsc::Sender<String>,
    ) -> u64 {
        let conn_id = self.next_conn_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let provider = WsToolProvider {
            tools,
            write_tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: std::sync::atomic::AtomicU64::new(1_000_000), // avoid collision with normal WS ids
        };

        tracing::info!(conn_id, tool_count = provider.tools.len(), "WS tools registered");
        self.providers.write().await.insert(conn_id, provider);
        conn_id
    }

    /// Unregister tools when a WS connection drops.
    pub async fn unregister(&self, conn_id: u64) {
        if self.providers.write().await.remove(&conn_id).is_some() {
            tracing::info!(conn_id, "WS tools unregistered");
        }
    }

    /// Handle a response from a client (routes to the correct pending call).
    pub async fn handle_response(&self, conn_id: u64, id: u64, result: Result<serde_json::Value>) {
        let providers = self.providers.read().await;
        if let Some(provider) = providers.get(&conn_id) {
            provider.handle_response(id, result);
        }
    }

    /// Get all tool definitions from all connected providers.
    pub async fn get_definitions(&self) -> Vec<ToolDefinition> {
        let providers = self.providers.read().await;
        let mut defs = Vec::new();
        for provider in providers.values() {
            defs.extend(provider.tools.iter().cloned());
        }
        defs
    }

    /// Call a tool by name — finds the provider that has it and sends a reverse RPC.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &simply_rpc::RequestContext,
    ) -> Result<Vec<ToolResultContent>> {
        let providers = self.providers.read().await;

        for provider in providers.values() {
            if provider.tools.iter().any(|t| t.name == name) {
                let result = provider.call_tool(name, arguments, ctx).await?;

                // Parse the result as CallToolResult-like structure
                let content = if let Some(arr) = result.get("content").and_then(|v| v.as_array()) {
                    arr.iter().map(|c| {
                        if let Some(text) = c.get("text").and_then(|v| v.as_str()) {
                            ToolResultContent::text(text)
                        } else {
                            ToolResultContent::text(serde_json::to_string(c).unwrap_or_default())
                        }
                    }).collect()
                } else if let Some(text) = result.as_str() {
                    vec![ToolResultContent::text(text)]
                } else {
                    vec![ToolResultContent::text(serde_json::to_string(&result)?)]
                };

                return Ok(content);
            }
        }

        anyhow::bail!("tool not found in WS providers: {name}")
    }

    /// Check if any WS provider has a tool with this name.
    pub async fn has_tool(&self, name: &str) -> bool {
        let providers = self.providers.read().await;
        providers.values().any(|p| p.tools.iter().any(|t| t.name == name))
    }
}
