//! Application initialization + deep-link handling.

use simply_daemon::net;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::logging::log_message;
use crate::state::AppState;

/// Return the daemon HTTP base URL (e.g. `http://127.0.0.1:9800`) so the webview
/// can talk to the daemon over REST+WS — same protocol the web admin UI uses.
///
/// The Tauri `setup` hook kicks off `run_init` but doesn't await it, so the
/// webview may call this before `rest_base_url` is populated. Poll with a
/// short sleep up to a 30s budget; if init is still stuck past that, surface
/// an error rather than hanging the UI.
#[tauri::command]
pub async fn daemon_base_url(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    for _ in 0..300 {
        if let Some(url) = state.rest_base_url.get() {
            return Ok(url.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err("Daemon init timed out".to_string())
}

/// Run daemon initialization once. Called from Tauri's `setup` hook at app
/// launch — the admin UI doesn't know anything about `init_app`; by the time
/// it loads and invokes `daemon_base_url`, the daemon URL is already set.
pub async fn run_init(state: Arc<AppState>) -> Result<String, String> {
    if state.is_initialized() {
        return Ok(String::new());
    }
    if state.initializing.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Ok(String::new());
    }
    do_init(state).await
}

#[tauri::command]
pub async fn init_app(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    run_init(state.inner().clone()).await
}

/// Return the admin user id resolved during init, if any. The webview uses
/// this to set `localStorage['simply-admin-user-id']` so `@simply/client`
/// sends the right `X-Request-Context` header on REST calls — matching the
/// identity embedded-daemon handlers use via `state.admin_ctx`.
#[tauri::command]
pub async fn admin_user_id(state: State<'_, Arc<AppState>>) -> Result<Option<String>, String> {
    for _ in 0..300 {
        if let Some(ctx) = state.admin_ctx.get() {
            return Ok(ctx.scope.user_id.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err("Daemon init timed out".to_string())
}

async fn do_init(state: Arc<AppState>) -> Result<String, String> {
    log_message("Starting app initialization");

    config::load_env_file();
    let mut settings = config::Settings::load();
    let port = settings.daemon_port;
    let daemon_secret = settings.ensure_daemon_secret().to_string();
    let daemon_port = port.unwrap_or(config::DEFAULT_DAEMON_PORT);

    let handle = net::connect_or_host(port, "noema", None)
        .await
        .map_err(|e| format!("Failed to initialize daemon: {}", e))?;

    let is_host = handle.is_host();
    let daemon = handle.daemon();

    // Set the REST base URL — used by the webview (via `daemon_base_url`) and
    // any server-side REST clients. When remote, this points to wherever the
    // daemon is running.
    let rest_base_url: String = format!("http://127.0.0.1:{daemon_port}");
    let _ = state.rest_base_url.set(rest_base_url);

    // Build an authenticated HTTP client for REST proxying (e.g. asset protocol)
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {daemon_secret}")) {
        headers.insert(reqwest::header::AUTHORIZATION, v);
    }
    let http_client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let _ = state.http_client.set(http_client);

    // Resolve admin identity from settings. Prefer the email-based lookup so
    // we share a user row with admin's "Create user" flow (both go through
    // `user_store.get_or_create_user_by_email`). Only fall back to the
    // external-id map when no email is configured yet.
    let anon = simply_rpc::RequestContext::anonymous();
    let admin_scope = match settings.user_email.as_deref() {
        Some(email) => daemon.user()
            .get_or_create_user_by_email(&anon, email.to_string())
            .await
            .unwrap_or_default(),
        None => daemon.user()
            .resolve_or_create_user(&anon, "noema:admin".to_string())
            .await
            .unwrap_or_default(),
    };
    let admin_ctx = simply_rpc::RequestContext::with_scope(admin_scope);
    let _ = state.admin_ctx.set(admin_ctx.clone());

    let model_name = daemon.model().default_model_id().await;
    let _ = state.daemon.set(daemon);
    let _ = state._daemon_handle.set(handle);

    log_message(&format!(
        "Daemon initialized ({}), default model: {}",
        if is_host { "embedded" } else { "remote" },
        model_name
    ));
    Ok(model_name)
}

/// Handle incoming deep link URLs (e.g., `noema://oauth/callback?code=...&state=...`).
///
/// The webview can't register OS-level URL schemes, so Tauri receives them and
/// forwards to the daemon's OAuth completion endpoint. On success/failure we
/// emit a Tauri event the admin UI listens for.
pub async fn handle_deep_link(app: &AppHandle, urls: Vec<url::Url>) {
    let state: tauri::State<'_, Arc<AppState>> = app.state();
    let daemon = match state.get_daemon() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "Deep link received but daemon not initialized");
            return;
        }
    };

    for url in urls {
        tracing::info!(url = %url, "Deep link received");

        let is_oauth_callback = url.scheme() == "noema"
            && url.host_str() == Some("oauth")
            && url.path() == "/callback";
        if !is_oauth_callback { continue; }

        let code = url.query_pairs().find(|(k, _)| k == "code").map(|(_, v)| v.to_string());
        let oauth_state = url.query_pairs().find(|(k, _)| k == "state").map(|(_, v)| v.to_string());
        let (Some(code), Some(oauth_state)) = (code, oauth_state) else {
            tracing::warn!("Incomplete OAuth callback — missing code or state");
            continue;
        };

        let Some(server_id) = daemon.oauth().resolve_oauth_state(&oauth_state).await else {
            tracing::warn!(state = %oauth_state, "No pending OAuth flow for state");
            app.emit("oauth_error", "No pending OAuth flow found").ok();
            continue;
        };

        match daemon.oauth().complete_oauth(&server_id, &code, &oauth_state).await {
            Ok(()) => {
                tracing::info!(server_id, "OAuth completed via deep link");
                app.emit("oauth_complete", &server_id).ok();
            }
            Err(e) => {
                tracing::error!(error = %e, "OAuth deep link completion failed");
                app.emit("oauth_error", e.to_string()).ok();
            }
        }
    }
}
