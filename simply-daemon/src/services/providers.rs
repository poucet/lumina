//! Concrete ToolProvider implementations.
//!
//! Three flavors — all implement the same `ToolProvider` trait from `simply-daemon-api`:
//! - `McpToolProvider`: wraps connected MCP servers (global or per-user OAuth)
//! - `WsToolProvider`: wraps tools registered over WebSocket (reverse RPC)
//! - `EmbeddedToolProvider`: wraps in-process `Skill` implementations

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, CallToolResult, Content, Tool};
use simply_rpc::RequestContext;
use tokio::sync::{mpsc, oneshot, Mutex};

use simply_core::mcp::{McpRegistry, McpToolCaller};
use simply_daemon_api::skill::OAuthRequirement;
use simply_daemon_api::{ProviderKind, ToolProvider};

/// Convert an `llm::ToolDefinition` to an `rmcp::model::Tool`.
fn definition_to_tool(td: llm::ToolDefinition) -> Tool {
    let schema = serde_json::to_value(&td.input_schema).unwrap_or_default();
    let schema_map = schema.as_object().cloned().unwrap_or_default();
    Tool::new(td.name, td.description.unwrap_or_default(), schema_map)
}

// ---------------------------------------------------------------------------
// McpToolProvider — wraps a connected MCP server
// ---------------------------------------------------------------------------

/// Wraps an MCP server (connected via rmcp) as a ToolProvider.
///
/// Two modes:
/// - **Shared**: uses a global connection (for no-auth / static-token servers)
/// - **OnDemand**: reconnects per call with user's OAuth token
pub struct McpToolProvider {
    id: String,
    name: String,
    tools: Vec<Tool>,
    kind: McpCallerKind,
}

enum McpCallerKind {
    /// Global connection — cloned peer handle, registry owns the connection.
    Shared(McpToolCaller),
    /// Per-user OAuth — reconnects on demand using the user's token from RequestContext.
    /// Token is looked up by `provider_id` in ctx.tokens.
    OnDemand { url: String, provider_id: String },
}

impl McpToolProvider {
    /// Create from a shared/global MCP connection.
    pub fn shared(id: String, name: String, tools: Vec<Tool>, caller: McpToolCaller) -> Self {
        Self { id, name, tools, kind: McpCallerKind::Shared(caller) }
    }

    /// Create for a per-user OAuth server (reconnects on demand with user's token).
    pub fn on_demand(id: String, name: String, tools: Vec<Tool>, url: String, provider_id: String) -> Self {
        Self { id, name, tools, kind: McpCallerKind::OnDemand { url, provider_id } }
    }
}

#[async_trait]
impl ToolProvider for McpToolProvider {
    fn id(&self) -> &str { &self.id }
    fn display_name(&self) -> &str { &self.name }

    async fn tools(&self) -> Vec<Tool> {
        self.tools.clone()
    }

    async fn call_tool(&self, request: CallToolRequestParams, ctx: &RequestContext) -> Result<CallToolResult> {
        match &self.kind {
            McpCallerKind::Shared(caller) => {
                caller.call_tool(request.name.to_string(), request.arguments).await
            }
            McpCallerKind::OnDemand { url, provider_id } => {
                let config = simply_core::ServerConfig {
                    name: String::new(),
                    url: url.clone(),
                    auto_connect: false,
                    auto_retry: false,
                };
                let bearer_token = ctx.tokens.get(provider_id).map(|t| t.as_str());
                let connected = McpRegistry::connect_to_server(&config, bearer_token).await?;
                let result = connected.tool_caller().call_tool(
                    request.name.to_string(),
                    request.arguments,
                ).await;
                // connected drops here, closing cleanly
                result
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WsToolProvider — wraps tools registered over WebSocket
// ---------------------------------------------------------------------------

/// A pending reverse RPC call.
struct PendingCall {
    tx: oneshot::Sender<Result<serde_json::Value>>,
}

/// Per-WS-connection state shared by all skills registered on that connection.
///
/// All reverse calls (for any skill) use the same id namespace here, so
/// ids never collide across skills and response routing is unambiguous.
pub struct WsConnState {
    write_tx: mpsc::Sender<String>,
    pending: Mutex<HashMap<u64, PendingCall>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl WsConnState {
    pub fn new(write_tx: mpsc::Sender<String>) -> Self {
        Self {
            write_tx,
            pending: Mutex::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1_000_000),
        }
    }

    /// Route a reverse response to the matching pending call, if any.
    pub async fn handle_response(&self, id: u64, result: Result<serde_json::Value>) {
        if let Some(call) = self.pending.lock().await.remove(&id) {
            let _ = call.tx.send(result);
        }
    }
}

/// Wraps tools from a single WebSocket-connected client as a ToolProvider.
///
/// Tool calls are dispatched as reverse RPC over the same WS connection.
/// User context (tokens, user_id) is sent as `__ctx` in the call params.
pub struct WsToolProvider {
    id: String,
    display: String,
    tools: Vec<Tool>,
    oauth_reqs: Vec<OAuthRequirement>,
    conn: Arc<WsConnState>,
}

impl WsToolProvider {
    pub fn new(
        id: String,
        display_name: String,
        tools: Vec<Tool>,
        oauth_reqs: Vec<OAuthRequirement>,
        conn: Arc<WsConnState>,
    ) -> Self {
        Self {
            id,
            display: display_name,
            tools,
            oauth_reqs,
            conn,
        }
    }
}

#[async_trait]
impl ToolProvider for WsToolProvider {
    fn id(&self) -> &str { &self.id }
    fn display_name(&self) -> &str { &self.display }
    fn kind(&self) -> ProviderKind { ProviderKind::Remote }

    async fn tools(&self) -> Vec<Tool> {
        self.tools.clone()
    }

    async fn oauth_requirements(&self) -> Vec<OAuthRequirement> {
        self.oauth_reqs.clone()
    }

    async fn call_tool(&self, request: CallToolRequestParams, ctx: &RequestContext) -> Result<CallToolResult> {
        let id = self.conn.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Register pending call in the shared conn-level map
        let (tx, rx) = oneshot::channel();
        self.conn.pending.lock().await.insert(id, PendingCall { tx });

        // Build reverse RPC request with user context.
        // Include __skill_id so the client can route to the right skill when multiple are registered.
        // __ctx is the full RequestContext (scope, tokens, metadata) so skills
        // get the same context the daemon received.
        let params = serde_json::json!({
            "name": request.name.as_ref(),
            "arguments": request.arguments,
            "__skill_id": self.id,
            "__ctx": serde_json::to_value(ctx)?,
        });

        let rpc_request = serde_json::json!({
            "id": id,
            "method": "tools.call",
            "params": params,
        });
        self.conn.write_tx.send(serde_json::to_string(&rpc_request)?).await
            .map_err(|_| anyhow::anyhow!("WS connection closed"))?;

        // Wait with timeout
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            rx,
        ).await
            .map_err(|_| anyhow::anyhow!("tool call timed out"))?
            .map_err(|_| anyhow::anyhow!("WS connection dropped during tool call"))?;

        let value = result?;
        // The client sends back an `rmcp::CallToolResult` verbatim — rmcp's
        // serde impl round-trips cleanly, so just deserialize.
        serde_json::from_value::<CallToolResult>(value)
            .map_err(|e| anyhow::anyhow!("invalid tool result from client: {e}"))
    }
}

// ---------------------------------------------------------------------------
// EmbeddedToolProvider — wraps in-process Skill
// ---------------------------------------------------------------------------

/// Wraps a `Skill` as a ToolProvider, converting between llm types and rmcp types.
///
/// `Skill::call_tool` already takes the caller's `RequestContext`, so this
/// adapter just forwards it.
pub struct EmbeddedToolProvider {
    skill: Arc<dyn simply_daemon_api::Skill>,
}

impl EmbeddedToolProvider {
    pub fn new(skill: Arc<dyn simply_daemon_api::Skill>) -> Self {
        Self { skill }
    }
}

#[async_trait]
impl ToolProvider for EmbeddedToolProvider {
    fn id(&self) -> &str { self.skill.name() }
    fn display_name(&self) -> &str { self.skill.name() }
    fn kind(&self) -> ProviderKind { ProviderKind::InProcess }

    async fn tools(&self) -> Vec<Tool> {
        self.skill.tools().into_iter().map(definition_to_tool).collect()
    }

    async fn oauth_requirements(&self) -> Vec<OAuthRequirement> {
        self.skill.oauth_requirements()
    }

    async fn call_tool(&self, request: CallToolRequestParams, ctx: &RequestContext) -> Result<CallToolResult> {
        let args = request.arguments
            .map(serde_json::Value::Object)
            .unwrap_or_default();
        self.skill.call_tool(request.name.as_ref(), args, ctx).await
    }
}

// ---------------------------------------------------------------------------
// ClientToolProvider — wraps a ToolCallHandler (for register_client_tools)
// ---------------------------------------------------------------------------

/// Wraps a `ToolCallHandler` as a ToolProvider for embedded client tool registration.
/// This is the equivalent of the old `ClientToolSkill` but speaks rmcp types directly.
pub struct ClientToolProvider {
    id: String,
    tools: Vec<Tool>,
    handler: simply_daemon_api::ToolCallHandler,
}

impl ClientToolProvider {
    pub fn new(id: String, tools: Vec<Tool>, handler: simply_daemon_api::ToolCallHandler) -> Self {
        Self { id, tools, handler }
    }

    /// Create from llm::ToolDefinition list (converts to rmcp Tool).
    pub fn from_definitions(
        id: String,
        defs: Vec<llm::ToolDefinition>,
        handler: simply_daemon_api::ToolCallHandler,
    ) -> Self {
        let tools = defs.into_iter().map(definition_to_tool).collect();
        Self { id, tools, handler }
    }
}

#[async_trait]
impl ToolProvider for ClientToolProvider {
    fn id(&self) -> &str { &self.id }
    fn display_name(&self) -> &str { &self.id }

    async fn tools(&self) -> Vec<Tool> {
        self.tools.clone()
    }

    async fn call_tool(&self, request: CallToolRequestParams, ctx: &RequestContext) -> Result<CallToolResult> {
        let args = request.arguments
            .map(serde_json::Value::Object)
            .unwrap_or_default();

        let result = (self.handler)(request.name.to_string(), args, ctx.clone()).await?;

        // Prefer rmcp's native format; fall back to a single-text wrapper.
        if let Ok(ct) = serde_json::from_value::<CallToolResult>(result.clone()) {
            return Ok(ct);
        }
        let text = result.as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| serde_json::to_string(&result).unwrap_or_default());
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}
