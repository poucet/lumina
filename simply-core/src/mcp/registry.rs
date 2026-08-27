use crate::mcp::config::{McpConfig, ServerConfig};
use crate::traffic_log;
use anyhow::Result;
use llm::{ToolDefinition, ToolResultContent};
use rmcp::{
    model::{CallToolRequestParams, RawContent, Tool},
    service::{Peer, RunningService},
    transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    },
    RoleClient, ServiceExt,
};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// A cloneable handle for calling MCP tools without holding registry locks.
#[derive(Clone)]
pub struct McpToolCaller {
    peer: Peer<RoleClient>,
}

impl McpToolCaller {
    pub async fn call_tool(
        &self,
        name: String,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<rmcp::model::CallToolResult> {
        let result = self
            .peer
            .call_tool({
                let mut req = CallToolRequestParams::new(name);
                if let Some(args) = arguments { req = req.with_arguments(args); }
                req
            })
            .await?;
        Ok(result)
    }
}

/// A connected MCP server with its available tools.
pub struct ConnectedServer {
    pub config: ServerConfig,
    pub tools: Vec<Tool>,
    service: RunningService<rmcp::RoleClient, ()>,
}

impl ConnectedServer {
    /// Get a cloneable tool caller that can be used without holding registry locks.
    pub fn tool_caller(&self) -> McpToolCaller {
        McpToolCaller {
            peer: self.service.deref().clone(),
        }
    }

    pub async fn call_tool(
        &self,
        name: String,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<rmcp::model::CallToolResult> {
        let result = self
            .service
            .call_tool({
                let mut req = CallToolRequestParams::new(name);
                if let Some(args) = arguments { req = req.with_arguments(args); }
                req
            })
            .await?;
        Ok(result)
    }

    pub async fn disconnect(self) -> Result<()> {
        self.service.cancel().await?;
        Ok(())
    }
}

/// Status of a server's connection/retry state.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerStatus {
    Disconnected,
    Connected,
    Retrying { attempt: u32 },
    RetryStopped { last_error: String },
}

/// Registry managing MCP server connections.
///
/// Auth-agnostic: the caller resolves auth externally and passes
/// an optional bearer token when connecting.
pub struct McpRegistry {
    config: McpConfig,
    connections: HashMap<String, ConnectedServer>,
    retry_tokens: HashMap<String, CancellationToken>,
    server_status: HashMap<String, ServerStatus>,
}

impl McpRegistry {
    pub fn new(config: McpConfig) -> Self {
        Self {
            config,
            connections: HashMap::new(),
            retry_tokens: HashMap::new(),
            server_status: HashMap::new(),
        }
    }

    pub fn config(&self) -> &McpConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut McpConfig {
        &mut self.config
    }

    pub fn list_servers(&self) -> Vec<(&str, &ServerConfig)> {
        self.config
            .servers
            .iter()
            .map(|(id, cfg)| (id.as_str(), cfg))
            .collect()
    }

    pub fn is_connected(&self, id: &str) -> bool {
        self.connections.contains_key(id)
    }

    pub fn get_connection(&self, id: &str) -> Option<&ConnectedServer> {
        self.connections.get(id)
    }

    /// Connect to a server.
    ///
    /// `bearer_token` — optional Bearer token resolved externally by the daemon.
    /// `simply-core` treats it as opaque — no OAuth logic here.
    pub async fn connect(&mut self, id: &str, bearer_token: Option<&str>) -> Result<&ConnectedServer> {
        if self.connections.contains_key(id) {
            return Ok(self.connections.get(id).unwrap());
        }

        let server_config = self
            .config
            .get_server(id)
            .ok_or_else(|| anyhow::anyhow!("Server '{}' not found in configuration", id))?
            .clone();

        let connected = Self::connect_to_server(&server_config, bearer_token).await?;
        self.connections.insert(id.to_string(), connected);
        Ok(self.connections.get(id).unwrap())
    }

    /// Connect to a server config with an optional bearer token.
    pub async fn connect_to_server(config: &ServerConfig, bearer_token: Option<&str>) -> Result<ConnectedServer> {
        let transport = if let Some(token) = bearer_token {
            let mut transport_config =
                StreamableHttpClientTransportConfig::with_uri(Arc::from(config.url.as_str()));
            transport_config = transport_config.auth_header(token.to_string());
            StreamableHttpClientTransport::from_config(transport_config)
        } else {
            StreamableHttpClientTransport::from_uri(config.url.as_str())
        };

        let service = ().serve(transport).await?;
        let tools_result = service.list_tools(Default::default()).await?;

        Ok(ConnectedServer {
            config: config.clone(),
            tools: tools_result.tools,
            service,
        })
    }

    pub async fn disconnect(&mut self, id: &str) -> Result<()> {
        if let Some(connection) = self.connections.remove(id) {
            connection.disconnect().await?;
        }
        Ok(())
    }

    pub async fn disconnect_all(&mut self) -> Result<()> {
        let ids: Vec<String> = self.connections.keys().cloned().collect();
        for id in ids {
            self.disconnect(&id).await?;
        }
        Ok(())
    }

    pub fn add_server(&mut self, id: String, config: ServerConfig) {
        self.config.add_server(id, config);
    }

    pub async fn remove_server(&mut self, id: &str) -> Result<Option<ServerConfig>> {
        self.disconnect(id).await?;
        Ok(self.config.remove_server(id))
    }

    pub fn connected_servers(&self) -> impl Iterator<Item = (&str, &ConnectedServer)> {
        self.connections.iter().map(|(id, s)| (id.as_str(), s))
    }

    pub fn get_status(&self, id: &str) -> ServerStatus {
        self.server_status
            .get(id)
            .cloned()
            .unwrap_or(if self.connections.contains_key(id) {
                ServerStatus::Connected
            } else {
                ServerStatus::Disconnected
            })
    }

    pub fn all_statuses(&self) -> HashMap<String, ServerStatus> {
        let mut statuses = HashMap::new();
        for (id, _) in &self.config.servers {
            statuses.insert(id.clone(), self.get_status(id));
        }
        statuses
    }

    pub fn set_status(&mut self, id: &str, status: ServerStatus) {
        self.server_status.insert(id.to_string(), status);
    }

    pub fn is_retry_active(&self, id: &str) -> bool {
        self.retry_tokens.contains_key(id)
    }

    pub fn set_retry_token(&mut self, id: &str, token: CancellationToken) {
        self.retry_tokens.insert(id.to_string(), token);
    }

    pub fn cancel_retry(&mut self, id: &str) {
        if let Some(token) = self.retry_tokens.remove(id) {
            token.cancel();
        }
    }

    pub fn remove_retry_token(&mut self, id: &str) {
        self.retry_tokens.remove(id);
    }

    pub fn store_connection(&mut self, id: &str, server: ConnectedServer) {
        self.connections.insert(id.to_string(), server);
        self.server_status.insert(id.to_string(), ServerStatus::Connected);
        self.retry_tokens.remove(id);
    }

    pub fn auto_connect_servers(&self) -> Vec<(String, ServerConfig)> {
        self.config
            .servers
            .iter()
            .filter(|(_, cfg)| cfg.auto_connect)
            .map(|(id, cfg)| (id.clone(), cfg.clone()))
            .collect()
    }
}

const INITIAL_BACKOFF_MS: u64 = 1000;
const MAX_BACKOFF_MS: u64 = 60000;
const BACKOFF_MULTIPLIER: f64 = 2.0;
/// Give up on a server after this many failed connection attempts (~2.5 min
/// with the backoff above) instead of retrying forever. It stays disabled
/// until reconnected manually or the daemon restarts.
const MAX_CONNECT_ATTEMPTS: u32 = 8;

/// Spawn a background retry task for connecting to an MCP server.
///
/// `bearer_token` — resolved externally by the daemon before spawning.
/// If the token expires and a fresh one is needed, cancel this task and
/// spawn a new one with updated credentials.
pub fn spawn_retry_task(
    registry: Arc<Mutex<McpRegistry>>,
    server_id: String,
    config: ServerConfig,
    bearer_token: Option<String>,
    on_status_change: Option<Box<dyn Fn(&str, &ServerStatus) + Send + Sync>>,
) -> CancellationToken {
    let token = CancellationToken::new();
    let cancel_token = token.clone();

    tokio::spawn(async move {
        let mut attempt: u32 = 0;
        let mut backoff_ms = INITIAL_BACKOFF_MS;

        loop {
            attempt += 1;

            {
                let mut reg = registry.lock().await;
                reg.set_status(&server_id, ServerStatus::Retrying { attempt });
                if let Some(ref cb) = on_status_change {
                    cb(&server_id, &ServerStatus::Retrying { attempt });
                }
            }

            match McpRegistry::connect_to_server(&config, bearer_token.as_deref()).await {
                Ok(connected) => {
                    let mut reg = registry.lock().await;
                    reg.store_connection(&server_id, connected);
                    if let Some(ref cb) = on_status_change {
                        cb(&server_id, &ServerStatus::Connected);
                    }
                    tracing::info!("MCP server '{}' connected after {} attempts", server_id, attempt);
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        "MCP server '{}' connection attempt {} failed: {}",
                        server_id, attempt, e
                    );

                    let should_retry = {
                        let reg = registry.lock().await;
                        reg.config()
                            .get_server(&server_id)
                            .map(|c| c.auto_retry)
                            .unwrap_or(false)
                    };

                    if !should_retry || attempt >= MAX_CONNECT_ATTEMPTS {
                        if should_retry {
                            tracing::warn!(
                                "MCP server '{}' auto-disabled after {} failed connection attempts",
                                server_id, attempt
                            );
                        }
                        let mut reg = registry.lock().await;
                        let status = ServerStatus::RetryStopped {
                            last_error: if should_retry {
                                format!("auto-disabled after {attempt} failed attempts: {e}")
                            } else {
                                e.to_string()
                            },
                        };
                        reg.set_status(&server_id, status.clone());
                        reg.remove_retry_token(&server_id);
                        if let Some(ref cb) = on_status_change {
                            cb(&server_id, &status);
                        }
                        return;
                    }

                    tokio::select! {
                        _ = cancel_token.cancelled() => {
                            let mut reg = registry.lock().await;
                            let status = ServerStatus::RetryStopped {
                                last_error: "Retry cancelled".to_string(),
                            };
                            reg.set_status(&server_id, status.clone());
                            reg.remove_retry_token(&server_id);
                            if let Some(ref cb) = on_status_change {
                                cb(&server_id, &status);
                            }
                            return;
                        }
                        _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {
                            backoff_ms = ((backoff_ms as f64) * BACKOFF_MULTIPLIER) as u64;
                            backoff_ms = backoff_ms.min(MAX_BACKOFF_MS);
                        }
                    }
                }
            }
        }
    });

    token
}

/// Start auto-connect for all configured servers with auto_connect enabled.
pub async fn start_auto_connect(
    registry: Arc<Mutex<McpRegistry>>,
    bearer_tokens: HashMap<String, String>,
    on_status_change: Option<Arc<dyn Fn(&str, &ServerStatus) + Send + Sync>>,
) -> usize {
    let servers_to_connect: Vec<(String, ServerConfig)> = {
        let reg = registry.lock().await;
        reg.auto_connect_servers()
    };

    let count = servers_to_connect.len();

    for (server_id, config) in servers_to_connect {
        {
            let reg = registry.lock().await;
            if reg.is_connected(&server_id) || reg.is_retry_active(&server_id) {
                continue;
            }
        }

        let cb: Option<Box<dyn Fn(&str, &ServerStatus) + Send + Sync>> =
            on_status_change.as_ref().map(|f| {
                let f = Arc::clone(f);
                Box::new(move |id: &str, status: &ServerStatus| f(id, status))
                    as Box<dyn Fn(&str, &ServerStatus) + Send + Sync>
            });

        let bearer_token = bearer_tokens.get(&server_id).cloned();
        let token = spawn_retry_task(Arc::clone(&registry), server_id.clone(), config, bearer_token, cb);

        {
            let mut reg = registry.lock().await;
            reg.set_retry_token(&server_id, token);
        }
    }

    count
}

fn mcp_tool_to_definition(tool: &Tool) -> ToolDefinition {
    let schema_map: serde_json::Map<String, serde_json::Value> = serde_json::to_value(&*tool.input_schema)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let input_schema: schemars::Schema = schema_map.into();

    ToolDefinition {
        name: tool.name.to_string(),
        description: tool.description.as_ref().map(|d| d.to_string()),
        input_schema,
    }
}

fn coerce_args_to_schema(
    args: &serde_json::Value,
    schema: &serde_json::Value,
) -> serde_json::Value {
    match (args, schema.get("type").and_then(|t| t.as_str())) {
        (serde_json::Value::String(s), Some("integer")) => {
            if let Ok(n) = s.parse::<i64>() {
                serde_json::Value::Number(serde_json::Number::from(n))
            } else {
                args.clone()
            }
        }
        (serde_json::Value::String(s), Some("number")) => {
            if let Ok(n) = s.parse::<f64>() {
                serde_json::Value::Number(
                    serde_json::Number::from_f64(n).unwrap_or_else(|| serde_json::Number::from(0)),
                )
            } else {
                args.clone()
            }
        }
        (serde_json::Value::Number(n), Some("integer")) => {
            if let Some(f) = n.as_f64() {
                serde_json::Value::Number(serde_json::Number::from(f as i64))
            } else {
                args.clone()
            }
        }
        (serde_json::Value::String(s), Some("boolean")) => {
            match s.to_lowercase().as_str() {
                "true" | "1" | "yes" => serde_json::Value::Bool(true),
                "false" | "0" | "no" => serde_json::Value::Bool(false),
                _ => args.clone(),
            }
        }
        (serde_json::Value::String(s), Some("array")) => {
            serde_json::from_str(s).unwrap_or_else(|_| args.clone())
        }
        (serde_json::Value::String(s), Some("object")) => {
            serde_json::from_str(s).unwrap_or_else(|_| args.clone())
        }
        (serde_json::Value::Object(obj), _) => {
            let properties = schema.get("properties");
            let mut new_obj = serde_json::Map::new();
            for (key, value) in obj {
                let prop_schema = properties
                    .and_then(|p| p.get(key))
                    .unwrap_or(&serde_json::Value::Null);
                new_obj.insert(key.clone(), coerce_args_to_schema(value, prop_schema));
            }
            serde_json::Value::Object(new_obj)
        }
        (serde_json::Value::Array(arr), _) => {
            let items_schema = schema.get("items").unwrap_or(&serde_json::Value::Null);
            serde_json::Value::Array(
                arr.iter()
                    .map(|item| coerce_args_to_schema(item, items_schema))
                    .collect(),
            )
        }
        _ => args.clone(),
    }
}

fn mcp_content_to_tool_result(content: &RawContent) -> Option<ToolResultContent> {
    match content {
        RawContent::Text(text) => Some(ToolResultContent::text(&text.text)),
        RawContent::Image(img) => Some(ToolResultContent::image(&img.data, &img.mime_type)),
        RawContent::Audio(audio) => Some(ToolResultContent::audio(&audio.data, &audio.mime_type)),
        RawContent::Resource(resource) => {
            match &resource.resource {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                    Some(ToolResultContent::text(text))
                }
                rmcp::model::ResourceContents::BlobResourceContents {
                    blob, mime_type, ..
                } => {
                    let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
                    if mime.starts_with("image/") {
                        Some(ToolResultContent::image(blob, mime))
                    } else if mime.starts_with("audio/") {
                        Some(ToolResultContent::audio(blob, mime))
                    } else {
                        None
                    }
                }
            }
        }
        RawContent::ResourceLink(_) => None,
    }
}

/// Dynamic tool registry wrapping McpRegistry.
pub struct McpToolRegistry {
    mcp_registry: Arc<Mutex<McpRegistry>>,
}

impl McpToolRegistry {
    pub fn new(mcp_registry: Arc<Mutex<McpRegistry>>) -> Self {
        Self { mcp_registry }
    }

    pub async fn has_tool(&self, name: &str) -> bool {
        self.get_server_for_tool(name).await.is_some()
    }

    pub async fn get_server_for_tool(&self, name: &str) -> Option<String> {
        let (target_server, tool_name) = name.split_once('.').unwrap_or(("", name));
        let registry = self.mcp_registry.lock().await;
        for (server_id, server) in registry.connected_servers() {
            if !target_server.is_empty() && server_id != target_server {
                continue;
            }
            if server.tools.iter().any(|t| t.name.as_ref() == tool_name) {
                return Some(server_id.to_string());
            }
        }
        None
    }

    pub async fn is_tool_from_server(&self, tool_name: &str, server_id: &str) -> bool {
        self.get_server_for_tool(tool_name).await.as_deref() == Some(server_id)
    }
}

#[async_trait::async_trait]
impl crate::agent::ToolService for McpToolRegistry {
    async fn get_definitions(&self) -> Vec<ToolDefinition> {
        let registry = self.mcp_registry.lock().await;
        let mut definitions = Vec::new();
        for (server_id, server) in registry.connected_servers() {
            for tool in &server.tools {
                let mut def = mcp_tool_to_definition(tool);
                def.name = format!("{server_id}.{}", def.name);
                definitions.push(def);
            }
        }
        definitions
    }

    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<Vec<ToolResultContent>> {
        traffic_log::log_mcp_request(name, &args);

        let (target_server, tool_name) = name.split_once('.')
            .unwrap_or(("", name));

        let (tool_caller, arguments) = {
            let registry = self.mcp_registry.lock().await;

            let mut found = None;
            for (server_id, server) in registry.connected_servers() {
                if !target_server.is_empty() && server_id != target_server {
                    continue;
                }
                if let Some(tool) = server.tools.iter().find(|t| t.name.as_ref() == tool_name) {
                    let schema = serde_json::to_value(&*tool.input_schema).unwrap_or_default();
                    let coerced_args = coerce_args_to_schema(&args, &schema);
                    let arguments = coerced_args.as_object().cloned();
                    found = Some((server.tool_caller(), arguments));
                    break;
                }
            }

            match found {
                Some(f) => f,
                None => {
                    let err_msg = format!("Tool '{}' not found in any connected MCP server", name);
                    traffic_log::log_mcp_error(name, &err_msg);
                    return Err(anyhow::anyhow!(err_msg));
                }
            }
        };

        match tool_caller.call_tool(tool_name.to_string(), arguments).await {
            Ok(result) => {
                let content: Vec<ToolResultContent> = result
                    .content
                    .into_iter()
                    .filter_map(|c| mcp_content_to_tool_result(&c.raw))
                    .collect();
                traffic_log::log_mcp_response(name, &content);
                Ok(content)
            }
            Err(e) => {
                traffic_log::log_mcp_error(name, &e.to_string());
                Err(e)
            }
        }
    }
}
