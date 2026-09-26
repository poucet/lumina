//! Unified HTTP server — REST + WebSocket on a single port.
//!
//! Uses axum for routing, REST dispatch, and WebSocket upgrades.
//! WebSocket connections upgrade at `/ws`. Everything else is REST dispatch.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequest, State, ConnectInfo};
use axum::extract::ws::{Message, WebSocket};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use simply_core::storage::ids::UserId;
use simply_core::storage::traits::UserStore;
use simply_rpc::{HttpMethod, ServiceRouter};

use crate::auth::RequestUser;
use crate::net::admin_api::{self, AdminState};
use crate::net::auth_routes::{self, SessionStore};
use crate::net::mcp_auth::{self, McpAuthState};
use crate::net::server::ConnectionTracker;
use crate::net::setup;
use crate::net::protocol::*;
use crate::token_store::TransientTokenStore;

/// Server configuration.
pub struct ServerConfig {
    pub rest_dispatcher: Arc<ServiceRouter>,
    pub port: u16,
    pub tracker: ConnectionTracker,
    pub daemon_secret: String,
    pub user_store: Arc<dyn UserStore>,
    pub token_store: Arc<TransientTokenStore>,
    pub oauth_tracker: Option<Arc<crate::oauth::callback::CallbackTracker>>,
    pub tools: Arc<crate::services::ToolRegistry>,
}

/// Shared state for axum handlers.
#[derive(Clone)]
struct AppState {
    rest_dispatcher: Arc<ServiceRouter>,
    tracker: ConnectionTracker,
    daemon_secret: Arc<str>,
    tools: Arc<crate::services::ToolRegistry>,
}

/// Starts the unified server (REST + WS + admin).
pub async fn start(config: ServerConfig) -> anyhow::Result<ServerHandle> {
    let sessions = SessionStore::new(std::time::Duration::from_secs(24 * 3600));

    let admin_state = AdminState {
        user_store: Arc::clone(&config.user_store),
    };

    let state = AppState {
        rest_dispatcher: config.rest_dispatcher,
        tracker: config.tracker,
        daemon_secret: Arc::from(config.daemon_secret.as_str()),
        tools: Arc::clone(&config.tools),
    };

    let admin_routes = Router::new()
        .route("/admin/api/setup-status", get(admin_api::get_setup_status))
        .route("/admin/api/settings", get(admin_api::get_settings).put(admin_api::update_settings))
        .route("/admin/api/api-key", axum::routing::post(admin_api::set_api_key))
        .route("/admin/api/api-key/{provider}", axum::routing::delete(admin_api::remove_api_key))
        .route("/admin/api/users", get(admin_api::list_users).post(admin_api::create_user))
        .route("/admin/api/users/{user_id}", axum::routing::delete(admin_api::delete_user))
        .with_state(admin_state);

    let settings = config::Settings::load();
    let public_url = settings.public_url.clone()
        .unwrap_or_else(|| format!("http://localhost:{}", config.port));

    // First-run setup: with no owner email yet, mint a one-time token and
    // advertise the setup URL (logged + written to SETUP-URL.txt so the deploy
    // script can echo it). Reaching `/setup` requires this token; it dies once
    // setup completes and the process restarts configured.
    let configured = settings.user_email.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
    let setup_state = if configured {
        setup::SetupState { token: None }
    } else {
        let token: Arc<str> = Arc::from(
            format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple()).as_str(),
        );
        let url = format!("{}/setup?token={}", public_url.trim_end_matches('/'), token);
        tracing::info!(%url, "first-run setup — open this URL in a browser to configure Lumina");
        if let Some(dir) = config::PathManager::data_dir() {
            let _ = std::fs::write(dir.join("SETUP-URL.txt"), format!("{url}\n"));
        }
        setup::SetupState { token: Some(token) }
    };

    let mcp_auth_state = McpAuthState {
        token_store: Arc::clone(&config.token_store),
        user_store: Arc::clone(&config.user_store),
        public_url,
        oauth_tracker: config.oauth_tracker,
        daemon_mcp_config: config.tools.mcp_service().daemon_config(),
        skill_scopes: config.tools.mcp_service().skill_scopes(),
    };

    let auth_routes = Router::new()
        .route("/auth/status", get(auth_routes::auth_status))
        .with_state(sessions);

    let mcp_auth_routes = Router::new()
        .route("/auth/mcp/callback", get(mcp_auth::auth_callback))
        .route("/auth/mcp/{server_id}", get(mcp_auth::auth_initiate))
        .with_state(mcp_auth_state);

    // First-run setup wizard (token-gated; 404s once configured).
    let setup_routes = Router::new()
        .route("/setup", get(setup::page))
        .route("/setup/api/models", axum::routing::post(setup::models))
        .route("/setup/api/complete", axum::routing::post(setup::complete))
        .with_state(setup_state);

    // RPC routes under /api/* — uses fallback handler for dynamic dispatch
    let api_routes = Router::new()
        .fallback(rest_or_stream_handler)
        .with_state(state.clone());

    // Static file serving for the admin UI (Astro build output)
    let admin_dist = find_admin_dist();
    let serve_dir = admin_dist.map(|dist| {
        tracing::info!(path = %dist.display(), "serving admin UI");
        ServeDir::new(dist).append_index_html_on_directories(true)
    });

    let mut app = Router::new()
        .route("/admin/api/connections", get(admin_connections))
        .route("/admin/api/routes", get(admin_api_routes))
        .route("/ws", get(ws_upgrade_handler))
        .merge(auth_routes)
        .merge(mcp_auth_routes)
        .merge(setup_routes)
        .merge(admin_routes)
        .nest("/api", api_routes);

    if let Some(serve) = serve_dir {
        app = app.fallback_service(serve);
    }

    // CORS: allow cross-origin requests from localhost dev servers and the
    // Tauri webview. Tauri pages load from `tauri://localhost` (macOS/Linux)
    // or `https://tauri.localhost` (Windows) and talk to the embedded daemon
    // over HTTP on loopback.
    let cors = CorsLayer::new()
        .allow_origin([
            "http://localhost:4321".parse().unwrap(),  // Astro dev (admin)
            "http://127.0.0.1:4321".parse().unwrap(),
            "http://localhost:5173".parse().unwrap(),  // Vite dev (noema/ui)
            "http://127.0.0.1:5173".parse().unwrap(),
            "http://localhost:9800".parse().unwrap(),  // same-origin
            "http://127.0.0.1:9800".parse().unwrap(),
            "tauri://localhost".parse().unwrap(),      // Tauri (macOS/Linux)
            "https://tauri.localhost".parse().unwrap(),// Tauri (Windows)
        ])
        .allow_methods(Any)
        .allow_headers(Any);

    let app = app
        .layer(cors)
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state);

    let addr = format!("127.0.0.1:{}", config.port);
    let listener = TcpListener::bind(&addr).await?;
    let actual_port = listener.local_addr()?.port();
    tracing::info!(port = actual_port, "server listening");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .with_graceful_shutdown(async { shutdown_rx.await.ok(); })
            .await
            .ok();
    });

    Ok(ServerHandle { _task: handle, _shutdown: shutdown_tx, port: actual_port })
}

pub struct ServerHandle {
    _task: JoinHandle<()>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    port: u16,
}

impl ServerHandle {
    pub fn port(&self) -> u16 { self.port }
}

// ---------------------------------------------------------------------------
// Auth middleware
// ---------------------------------------------------------------------------

async fn auth_middleware(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    mut req: axum::extract::Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let is_localhost = addr.ip().is_loopback();

    // Localhost = admin for everything (UI, API, RPC)
    if is_localhost {
        // Still check for Bearer if present (service clients on localhost)
        let has_bearer = req.headers().get(header::AUTHORIZATION).is_some();
        if !has_bearer {
            req.extensions_mut().insert(RequestUser::Admin);
            return next.run(req).await;
        }
    }

    // Non-localhost: only /api/* and /ws require Bearer auth
    let requires_bearer = path.starts_with("/api/") || path == "/ws";
    if !requires_bearer {
        req.extensions_mut().insert(RequestUser::Anonymous);
        return next.run(req).await;
    }

    // Bearer-protected routes
    let token = req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());

    let Some(token) = token else {
        return (StatusCode::UNAUTHORIZED, "missing authorization").into_response();
    };

    // Check daemon_secret (trusted service clients)
    if token == state.daemon_secret.as_ref() {
        let user_id = req.headers()
            .get("X-User-Id")
            .and_then(|v| v.to_str().ok())
            .map(|s| UserId::from_string(s.to_string()));
        req.extensions_mut().insert(RequestUser::Service(user_id));
        return next.run(req).await;
    }

    (StatusCode::UNAUTHORIZED, "invalid token").into_response()
}

// ---------------------------------------------------------------------------
// Admin handlers
// ---------------------------------------------------------------------------

fn find_admin_dist() -> Option<std::path::PathBuf> {
    // Compile-time path: the admin/dist dir next to this crate's Cargo.toml
    let compile_time = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("admin/dist");
    if compile_time.is_dir() {
        return Some(compile_time);
    }

    // Runtime fallback: relative to CWD
    for candidate in &["simply-daemon/admin/dist", "admin/dist"] {
        let path = std::path::PathBuf::from(candidate);
        if path.is_dir() { return Some(path); }
    }

    tracing::warn!("admin dist not found — using embedded fallback");
    None
}

async fn admin_connections(State(state): State<AppState>) -> Json<Vec<crate::net::server::ConnectionInfo>> {
    Json(state.tracker.list().await)
}

#[derive(serde::Serialize)]
struct RouteInfo {
    method: String,
    path: String,
    name: String,
    group: String,
    description: Option<String>,
    schema: Option<serde_json::Value>,
}

async fn admin_api_routes(State(state): State<AppState>) -> Json<Vec<RouteInfo>> {
    let metas = state.rest_dispatcher.route_metas();
    let mut routes: Vec<RouteInfo> = metas
        .iter()
        .map(|m| {
            let method = match m.kind {
                simply_rpc::RouteKind::Rest(hm) => match hm {
                    HttpMethod::Get => "GET",
                    HttpMethod::Post => "POST",
                    HttpMethod::Put => "PUT",
                    HttpMethod::Delete => "DELETE",
                },
                simply_rpc::RouteKind::Stream => "STREAM",
            };
            let group = m.method_name.split('.').next().unwrap_or("other").to_string();
            let schema = (m.tool_schema)()
                .and_then(|s| serde_json::to_value(s).ok());
            RouteInfo {
                method: method.to_string(),
                path: format!("/api{}", m.path_template),
                name: m.method_name.to_string(),
                group,
                description: m.description.map(|s| s.to_string()),
                schema,
            }
        })
        .collect();

    for (method, path, name, desc) in [
        ("GET", "/admin/api/setup-status", "admin.setup_status", "Check setup status"),
        ("GET", "/admin/api/settings", "admin.get_settings", "Get settings"),
        ("PUT", "/admin/api/settings", "admin.update_settings", "Update settings"),
        ("POST", "/admin/api/api-key", "admin.set_api_key", "Set API key"),
        ("DELETE", "/admin/api/api-key/{provider}", "admin.remove_api_key", "Remove API key"),
        ("GET", "/admin/api/users", "admin.list_users", "List users"),
        ("POST", "/admin/api/users", "admin.create_user", "Create user"),
        ("GET", "/admin/api/connections", "admin.connections", "List connections"),
        ("GET", "/admin/api/routes", "admin.routes", "List API routes"),
    ] {
        routes.push(RouteInfo {
            method: method.to_string(),
            path: path.to_string(),
            name: name.to_string(),
            group: "admin".to_string(),
            description: Some(desc.to_string()),
            schema: None,
        });
    }

    Json(routes)
}

// ---------------------------------------------------------------------------
// WebSocket upgrade handler
// ---------------------------------------------------------------------------

async fn ws_upgrade_handler(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    axum::Extension(request_user): axum::Extension<RequestUser>,
    ws: axum::extract::WebSocketUpgrade,
) -> Response {
    let dispatcher = Arc::clone(&state.rest_dispatcher);
    let tracker = state.tracker.clone();
    let ctx = match request_user.user_id() {
        Some(uid) => simply_rpc::RequestContext::with_scope(
            simply_rpc::Scope::user(uid.as_str()),
        ),
        None => simply_rpc::RequestContext::anonymous(),
    };
    let tools = Arc::clone(&state.tools);
    ws.on_upgrade(move |socket| handle_ws_connection(dispatcher, socket, tracker, addr, ctx, tools))
}

async fn handle_ws_connection(
    dispatcher: Arc<ServiceRouter>,
    socket: WebSocket,
    tracker: ConnectionTracker,
    addr: std::net::SocketAddr,
    ctx: simply_rpc::RequestContext,
    tools: Arc<crate::services::ToolRegistry>,
) {
    use futures_util::{SinkExt, StreamExt};

    tracing::info!(%addr, "WS client connected");
    let conn_id = tracker.add(addr).await;

    let (mut ws_sink, mut ws_source) = socket.split();
    let (write_tx, mut write_rx) = mpsc::channel::<String>(256);
    let mut input_sinks: std::collections::HashMap<String, mpsc::Sender<serde_json::Value>> = std::collections::HashMap::new();
    // Shared per-connection reverse-RPC state (pending map, id counter).
    // All WsToolProviders registered on this connection share this so their
    // reverse-call ids never collide and responses route unambiguously.
    let ws_conn_state = Arc::new(crate::services::providers::WsConnState::new(write_tx.clone()));
    let mut ws_providers: Vec<Arc<crate::services::providers::WsToolProvider>> = Vec::new();

    let writer_handle = tokio::spawn(async move {
        while let Some(text) = write_rx.recv().await {
            if ws_sink.send(Message::Text(text.into())).await.is_err() { break; }
        }
    });

    while let Some(msg) = ws_source.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(e) => { tracing::error!(error = %e, "WS read error"); break; }
        };

        let incoming: WsIncoming = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => { tracing::warn!(error = %e, "invalid WS message"); continue; }
        };

        // Notifications (no id) with a method ending in ".input" are bidi stream input
        if incoming.is_notification() {
            if let Some(method) = &incoming.method {
                if let Some(base) = method.strip_suffix(".input") {
                    if let Some(sink) = input_sinks.get(base) {
                        let _ = sink.send(incoming.params).await;
                    }
                }
            }
            continue;
        }

        // Handle responses to reverse RPC calls (tools.call responses from client).
        // Ids are unique across all skills on this connection (shared WsConnState),
        // so a single lookup on the shared pending map routes correctly.
        if incoming.id.is_some() && incoming.method.is_none() && (incoming.result.is_some() || incoming.error.is_some()) {
            let id = incoming.id.unwrap();
            let result = if let Some(r) = incoming.result {
                Ok(r)
            } else {
                Err(anyhow::anyhow!("tool call error: {:?}", incoming.error))
            };
            ws_conn_state.handle_response(id, result).await;
            continue;
        }

        if !incoming.is_request() { continue; }

        let id = incoming.id.unwrap();
        let method = incoming.method.unwrap();
        let params = incoming.params;

        // Built-in: client identification
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

        // Built-in: register a skill (tools + optional OAuth requirements) from this client.
        // Payload: { id, display_name, tools, oauth_requirements? }
        // Legacy payload with just `tools` is accepted and falls back to conn-derived id.
        if method == "tools.register" {
            let tool_defs: Vec<llm::ToolDefinition> = params.get("tools")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            if tool_defs.is_empty() {
                let response = WsResponse::err(id, "no tools provided");
                let text = serde_json::to_string(&response).unwrap_or_default();
                if write_tx.send(text).await.is_err() { break; }
                continue;
            }

            let skill_id = params.get("id").and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("ws-client-{conn_id}"));
            let display_name = params.get("display_name").and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| skill_id.clone());
            let oauth_reqs: Vec<simply_daemon_api::OAuthRequirement> = params.get("oauth_requirements")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();

            // Register OAuth providers declared by the skill (same as embedded register_skill)
            let mcp = tools.mcp_service();
            for req in &oauth_reqs {
                let register_req = simply_daemon_api::RegisterOAuthProviderRequest {
                    display_name: req.display_name.clone(),
                    authorization_url: req.authorization_url.clone(),
                    token_url: req.token_url.clone(),
                    scopes: req.scopes.clone(),
                    client_id: String::new(),
                    client_secret: None,
                    userinfo_url: req.userinfo_url.clone(),
                };
                if let Err(e) = simply_daemon_api::OAuthApi::register_oauth_provider(
                    mcp.as_ref(), &req.provider_id, register_req
                ).await {
                    tracing::warn!(error = %e, provider = %req.provider_id, "failed to register OAuth provider from WS skill");
                }
            }

            let count = tool_defs.len();
            let rmcp_tools: Vec<rmcp::model::Tool> = tool_defs.into_iter().map(|td| {
                let schema = serde_json::to_value(&td.input_schema).unwrap_or_default();
                let schema_map = schema.as_object().cloned().unwrap_or_default();
                rmcp::model::Tool::new(td.name, td.description.unwrap_or_default(), schema_map)
            }).collect();
            let provider = Arc::new(crate::services::providers::WsToolProvider::new(
                skill_id.clone(),
                display_name,
                rmcp_tools,
                oauth_reqs,
                Arc::clone(&ws_conn_state),
            ));
            tools.register(provider.clone()).await;
            ws_providers.push(provider);
            tracing::info!(conn_id, skill_id = %skill_id, count, "remote skill registered via WS");
            let response = WsResponse::ok(id, serde_json::json!({ "registered": count }));
            let text = serde_json::to_string(&response).unwrap_or_default();
            if write_tx.send(text).await.is_err() { break; }
            continue;
        }

        tracing::debug!(id, method = %method, "WS request");
        tracing::trace!(id, method = %method, params = %params, "WS request params");

        // Extract per-message ctx if present (from RemoteDaemon client), otherwise use connection ctx
        let msg_ctx = if let Some(ctx_val) = params.get("__ctx") {
            serde_json::from_value::<simply_rpc::RequestContext>(ctx_val.clone()).unwrap_or(ctx.clone())
        } else {
            ctx.clone()
        };
        // Remove __ctx from params before dispatching
        let params = match params {
            serde_json::Value::Object(mut map) => {
                map.remove("__ctx");
                serde_json::Value::Object(map)
            }
            other => other,
        };

        // Streaming dispatch must stay in-line because it returns an input_sink
        // that needs to be registered in the local `input_sinks` map.
        if let Some(ws_result) = dispatcher.ws_dispatch_by_method(&method, &msg_ctx, params.clone(), write_tx.clone()).await {
            let mut response = match ws_result.result {
                Ok(v) => WsResponse::ok(id, v),
                Err(e) => WsResponse::err(id, e),
            };
            response.id = id;
            input_sinks.insert(method.clone(), ws_result.input_sink);
            let is_err = response.error.is_some();
            tracing::debug!(id, method = %method, error = is_err, "WS response (stream)");
            let text = serde_json::to_string(&response).unwrap_or_default();
            if write_tx.send(text).await.is_err() { break; }
            continue;
        }

        // Regular RPC — spawn so the reader loop keeps advancing. This prevents
        // a deadlock when a tool call triggers a reverse-RPC back to this client:
        // the reverse-response arrives on the same WS connection and needs the
        // reader to process it while the dispatch future is awaiting.
        let dispatcher = Arc::clone(&dispatcher);
        let write_tx_task = write_tx.clone();
        tokio::spawn(async move {
            let response = match dispatcher.dispatch_by_method(&method, &msg_ctx, params).await {
                Some(Ok(v)) => WsResponse::ok(id, v),
                Some(Err(e)) => WsResponse::err(id, e),
                None => WsResponse::err(id, format!("unknown method: {method}")),
            };
            let is_err = response.error.is_some();
            tracing::debug!(id, method = %method, error = is_err, "WS response");
            if is_err {
                tracing::debug!(id, error = ?response.error, "WS response error");
            }
            let text = serde_json::to_string(&response).unwrap_or_default();
            let _ = write_tx_task.send(text).await;
        });
    }

    writer_handle.abort();
    tracker.remove(conn_id).await;
    for provider in &ws_providers {
        tools.unregister(<_ as simply_daemon_api::ToolProvider>::id(provider.as_ref())).await;
    }
    tracing::info!("WS client disconnected");
}

// ---------------------------------------------------------------------------
// Unified fallback — stream WS upgrade or REST dispatch
// ---------------------------------------------------------------------------

async fn rest_or_stream_handler(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Response {
    let path = req.uri().path().to_string();

    // Check if this is a WS upgrade for a stream route
    if req.headers().get("upgrade").and_then(|v| v.to_str().ok()) == Some("websocket")
        && state.rest_dispatcher.has_stream_route(&path)
    {
        let ws = match axum::extract::WebSocketUpgrade::from_request(req, &state).await {
            Ok(ws) => ws,
            Err(e) => return e.into_response(),
        };
        let dispatcher = Arc::clone(&state.rest_dispatcher);
        return ws.on_upgrade(move |socket| handle_stream_ws(dispatcher, path, socket));
    }

    rest_handler(state, req).await
}

async fn handle_stream_ws(
    dispatcher: Arc<ServiceRouter>,
    path: String,
    socket: WebSocket,
) {
    use futures_util::{SinkExt, StreamExt};

    let (mut ws_sink, mut ws_source) = socket.split();
    let (write_tx, mut write_rx) = mpsc::channel::<String>(256);

    let writer_handle = tokio::spawn(async move {
        while let Some(text) = write_rx.recv().await {
            if ws_sink.send(Message::Text(text.into())).await.is_err() { break; }
        }
        let _ = ws_sink.close().await;
    });

    let anon = simply_rpc::RequestContext::anonymous();
    let result = dispatcher.ws_dispatch(&path, &anon, serde_json::Value::Null, write_tx.clone()).await;

    let input_sink = match result {
        Some(dr) => {
            let response = match dr.result {
                Ok(v) => WsResponse::ok(0, v),
                Err(e) => WsResponse::err(0, e),
            };
            let text = serde_json::to_string(&response).unwrap_or_default();
            if write_tx.send(text).await.is_err() {
                writer_handle.abort();
                return;
            }
            dr.input_sink
        }
        None => {
            let err = WsResponse::err(0, "no stream handler for path");
            let text = serde_json::to_string(&err).unwrap_or_default();
            let _ = write_tx.send(text).await;
            writer_handle.abort();
            return;
        }
    };

    while let Some(msg) = ws_source.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    if input_sink.send(value).await.is_err() { break; }
                }
            }
            Ok(Message::Close(_)) => break,
            Err(e) => {
                tracing::error!("stream WS read error: {e}");
                break;
            }
            _ => {}
        }
    }

    drop(input_sink);
    writer_handle.abort();
    tracing::debug!(path, "stream WS disconnected");
}

async fn rest_handler(
    state: AppState,
    req: axum::extract::Request,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query_str = req.uri().query().unwrap_or("").to_string();
    let content_type = req.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    // Extract resolved user from auth middleware
    let request_user = req.extensions()
        .get::<RequestUser>()
        .cloned()
        .unwrap_or(RequestUser::Anonymous);

    tracing::info!(method = %method, path = %path, user = ?request_user, "REST request");

    let http_method = match method {
        Method::GET => HttpMethod::Get,
        Method::POST => HttpMethod::Post,
        Method::PUT => HttpMethod::Put,
        Method::DELETE => HttpMethod::Delete,
        _ => return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response(),
    };

    // Extract context header before consuming the body
    let ctx_header = req.headers()
        .get("X-Request-Context")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| serde_json::from_str::<simply_rpc::RequestContext>(s).ok());

    // Read raw body for POST/PUT
    let raw_bytes = match http_method {
        HttpMethod::Post | HttpMethod::Put => {
            match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
                Ok(b) => b,
                Err(_) => return (StatusCode::BAD_REQUEST, "failed to read body").into_response(),
            }
        }
        _ => axum::body::Bytes::new(),
    };

    let is_binary_upload = state.rest_dispatcher.is_binary_upload(http_method, &path);
    // Native HTTP binary upload = body is raw bytes, Content-Type header carries the mime type.
    // But internal clients (generated RemoteXxxApi → rest_call) send BinaryUpload as
    // JSON (serde base64-encodes data). If Content-Type is application/json, treat the
    // body as a regular JSON payload so BinaryUpload deserializes correctly.
    let native_binary_upload = is_binary_upload && !content_type.starts_with("application/json");

    let body = if native_binary_upload {
        serde_json::to_value(simply_rpc::BinaryUpload::new(raw_bytes.to_vec(), content_type))
            .unwrap_or(serde_json::Value::Null)
    } else if !raw_bytes.is_empty() {
        // POST/PUT — body is the JSON payload. Keys stay as-is: the
        // macro's per-field extraction looks up both snake_case and
        // camelCase variants, and struct-body endpoints deserialize via
        // their own serde config (typically `rename_all = "camelCase"`).
        serde_json::from_slice(&raw_bytes).unwrap_or(serde_json::Value::Null)
    } else if matches!(http_method, HttpMethod::Get | HttpMethod::Delete) && !query_str.is_empty() {
        // GET/DELETE — query string becomes the JSON param object so the
        // dispatcher can deserialize it into the trait's param struct.
        // Keep query keys as-sent; the macro tries both camelCase and
        // snake_case when extracting named params.
        let mut obj = serde_json::Map::new();
        for (k, v) in url::form_urlencoded::parse(query_str.as_bytes()) {
            obj.insert(k.into_owned(), serde_json::Value::String(v.into_owned()));
        }
        serde_json::Value::Object(obj)
    } else {
        serde_json::Value::Null
    };

    // Build RequestContext: prefer X-Request-Context header (from RemoteDaemon client),
    // fall back to auth middleware's user resolution
    let ctx = ctx_header.unwrap_or_else(|| match request_user.user_id() {
            Some(uid) => simply_rpc::RequestContext::with_scope(
                simply_rpc::Scope::user(uid.as_str()),
            ),
            None => simply_rpc::RequestContext::anonymous(),
        });

    let rest_result = match state.rest_dispatcher.dispatch(http_method, &path, &ctx, body).await {
        Some(r) => r,
        None => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };

    match rest_result.result {
        Ok(value) => {
            let mut response = if rest_result.meta.binary_response {
                let br: simply_rpc::BinaryResponse = match serde_json::from_value(value) {
                    Ok(br) => br,
                    Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
                };
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", &br.mime_type)
                    .header("Content-Length", br.data.len().to_string())
                    .body(Body::from(br.data))
                    .unwrap()
            } else {
                Json(value).into_response()
            };

            if rest_result.meta.immutable_cache {
                let etag = path.rsplit('/').next().unwrap_or("");
                let headers = response.headers_mut();
                headers.insert("Cache-Control", "public, max-age=31536000, immutable".parse().unwrap());
                headers.insert("ETag", format!("\"{etag}\"").parse().unwrap());
            }

            response
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") || msg.contains("No such file") {
                (StatusCode::NOT_FOUND, msg).into_response()
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
            }
        }
    }
}
