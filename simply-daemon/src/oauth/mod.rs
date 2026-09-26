//! OAuth support — for MCP servers and skill OAuth providers.
//!
//! `OAuthService` drives the OAuth lifecycle: pending state tracking,
//! callback server, and token exchange.
//! Tokens are stored per-user in `TransientTokenStore` — never globally.

pub mod callback;
pub mod providers;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use simply_core::McpRegistry;
use simply_core::storage::ids::UserId;

use crate::api::OAuthFlowInfo;
use crate::mcp::auth::DaemonMcpConfig;
use crate::token_store::{McpUserToken, TransientTokenStore};
use callback::CallbackTracker;
use providers::{resolve_server_auth, ResolvedOAuth};

pub struct OAuthService {
    /// The registry — needed to reconnect after token exchange.
    registry: Arc<Mutex<McpRegistry>>,
    /// MCP server configs.
    daemon_config: Arc<Mutex<DaemonMcpConfig>>,
    /// Skill-declared scopes per provider (runtime union).
    skill_scopes: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    /// Token store — where all tokens (including server-level) live.
    token_store: Arc<TransientTokenStore>,
    tracker: Arc<CallbackTracker>,
    /// state -> server_id for flows in progress.
    pending_states: Mutex<HashMap<String, String>>,
}

impl OAuthService {
    pub fn new(
        registry: Arc<Mutex<McpRegistry>>,
        public_url: String,
        daemon_config: Arc<Mutex<DaemonMcpConfig>>,
        skill_scopes: Arc<Mutex<HashMap<String, HashSet<String>>>>,
        token_store: Arc<TransientTokenStore>,
    ) -> Self {
        Self {
            registry,
            daemon_config,
            skill_scopes,
            token_store,
            tracker: Arc::new(CallbackTracker::new(public_url)),
            pending_states: Mutex::new(HashMap::new()),
        }
    }

    pub fn tracker(&self) -> Arc<CallbackTracker> {
        Arc::clone(&self.tracker)
    }

    pub fn redirect_uri(&self) -> String {
        self.tracker.redirect_uri()
    }

    /// Start an OAuth flow for an MCP server (admin-UI-triggered server-level auth).
    pub async fn start_flow(&self, server_id: &str) -> anyhow::Result<OAuthFlowInfo> {
        let resolved = {
            let cfg = self.daemon_config.lock().await;
            let server = cfg
                .get_server(server_id)
                .ok_or_else(|| anyhow::anyhow!("server not found: {server_id}"))?;
            resolve_server_auth(&server.auth)
                .ok_or_else(|| anyhow::anyhow!("server is not configured for OAuth: {server_id}"))?
        };

        if resolved.client_id.is_empty() {
            anyhow::bail!("Configure client_id for this provider in oauth_providers.toml first.");
        }

        let redirect_uri = self.tracker.redirect_uri();
        let state_param = uuid::Uuid::new_v4().to_string();
        let callback_rx = self.tracker.register(&state_param).await;
        self.pending_states.lock().await.insert(state_param.clone(), server_id.to_string());

        let scope_str = if resolved.scopes.is_empty() { "openid".to_string() } else { resolved.scopes.join(" ") };
        let auth_url_str = build_auth_url(&resolved.authorization_url, &resolved.client_id, &redirect_uri, &state_param, &scope_str)?;

        let registry = Arc::clone(&self.registry);
        let daemon_config = Arc::clone(&self.daemon_config);
        let token_store = Arc::clone(&self.token_store);
        let server_id_owned = server_id.to_string();
        let state_clone = state_param.clone();

        tokio::spawn(async move {
            match callback_rx.await {
                Ok(Ok((code, received_state))) => {
                    if received_state != state_clone {
                        tracing::error!("OAuth state mismatch");
                        return;
                    }
                    if let Err(e) = complete_server_token_exchange(
                        &registry,
                        &daemon_config,
                        &token_store,
                        &server_id_owned,
                        &code,
                        &redirect_uri,
                    ).await {
                        tracing::error!(error = %e, "OAuth token exchange failed");
                    }
                }
                Ok(Err(e)) => tracing::error!(error = %e, "OAuth callback error"),
                Err(_) => tracing::error!("OAuth callback cancelled"),
            }
        });

        Ok(OAuthFlowInfo { auth_url: auth_url_str, state: state_param })
    }

    pub async fn complete_with_state(
        &self,
        server_id: &str,
        code: &str,
        state: &str,
    ) -> anyhow::Result<()> {
        let expected_server = self
            .pending_states.lock().await
            .remove(state)
            .ok_or_else(|| anyhow::anyhow!("unknown or expired OAuth state"))?;

        if expected_server != server_id {
            anyhow::bail!("OAuth state does not match server_id");
        }

        self.tracker.cancel(state).await;

        complete_server_token_exchange(
            &self.registry,
            &self.daemon_config,
            &self.token_store,
            server_id,
            code,
            &self.tracker.redirect_uri(),
        ).await
    }

    pub async fn complete_with_code(&self, server_id: &str, code: &str) -> anyhow::Result<()> {
        complete_server_token_exchange(
            &self.registry,
            &self.daemon_config,
            &self.token_store,
            server_id,
            code,
            &self.tracker.redirect_uri(),
        ).await
    }

    pub async fn resolve_state(&self, state: &str) -> Option<String> {
        self.pending_states.lock().await.get(state).cloned()
    }

    /// Skill-declared scopes accumulated for a provider (used by mcp_auth.rs).
    pub async fn skill_scopes_for(&self, provider_id: &str) -> Vec<String> {
        self.skill_scopes.lock().await
            .get(provider_id)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Fetch `.well-known/oauth-authorization-server` from a URL.
pub async fn fetch_well_known(base_url: &str) -> anyhow::Result<serde_json::Value> {
    let base = url::Url::parse(base_url)?;
    let well_known_url = base.join("/.well-known/oauth-authorization-server")?;
    let client = reqwest::Client::new();
    let resp = client.get(well_known_url.as_str()).send().await?;
    Ok(resp.json().await?)
}

fn build_auth_url(
    auth_url: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    scope: &str,
) -> anyhow::Result<String> {
    let mut url = url::Url::parse(auth_url)?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", state)
        .append_pair("scope", scope)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    Ok(url.to_string())
}

/// Complete an OAuth flow for an MCP server (triggered from admin UI).
/// Tokens are stored in TransientTokenStore under synthetic user "server:{id}".
async fn complete_server_token_exchange(
    registry: &Arc<Mutex<McpRegistry>>,
    daemon_config: &Arc<Mutex<DaemonMcpConfig>>,
    token_store: &Arc<TransientTokenStore>,
    server_id: &str,
    code: &str,
    redirect_uri: &str,
) -> anyhow::Result<()> {
    let resolved: ResolvedOAuth = {
        let cfg = daemon_config.lock().await;
        let server = cfg
            .get_server(server_id)
            .ok_or_else(|| anyhow::anyhow!("server not found: {server_id}"))?;
        resolve_server_auth(&server.auth)
            .ok_or_else(|| anyhow::anyhow!("server is not configured for OAuth"))?
    };

    let tokens = exchange_code_for_tokens(
        &resolved.token_url,
        code,
        redirect_uri,
        &resolved.client_id,
        resolved.client_secret.as_deref(),
    ).await?;

    let server_user = UserId::from_string(format!("server:{server_id}"));
    token_store.store(&server_user, server_id, McpUserToken {
        access_token: tokens.access_token.clone(),
        refresh_token: None,
        expires_at: None,
        identity: None,
    });

    let mut reg = registry.lock().await;
    if reg.is_connected(server_id) {
        reg.disconnect(server_id).await?;
    }
    reg.connect(server_id, Some(&tokens.access_token)).await?;

    tracing::info!(server_id, "OAuth complete, reconnected with new token");
    Ok(())
}

async fn exchange_code_for_tokens(
    token_url: &str,
    code: &str,
    redirect_uri: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> anyhow::Result<TokenResponse> {
    let http_client = reqwest::Client::new();

    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
    ];

    let secret_owned;
    if let Some(secret) = client_secret {
        secret_owned = secret.to_string();
        params.push(("client_secret", &secret_owned));
    }

    let resp = http_client.post(token_url).form(&params).send().await?;

    if !resp.status().is_success() {
        let error_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token exchange failed: {}", error_text);
    }

    let token_response: serde_json::Value = resp.json().await?;

    let access_token = token_response["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No access_token in response"))?
        .to_string();

    Ok(TokenResponse { access_token })
}

struct TokenResponse {
    access_token: String,
}
