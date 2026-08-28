//! Session lifecycle and event streaming.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use simply_rpc::RequestContext;
#[cfg(feature = "ts")]
use ts_rs::TS;
use tokio::sync::broadcast;

use crate::types::{DaemonEvent, InboundEvent, InputContent, SessionId};

pub use simply_core::Persistence;

/// Options when creating a new session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts", derive(TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ts/src/generated/types/"))]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionOptions {
    pub persistence: Option<Persistence>,
    pub system_prompt: Option<String>,
    pub model_id: Option<String>,
    #[serde(default)]
    pub seed: Vec<SeedMessage>,
    /// Restrict the session's tools to these exact names; `None` (default)
    /// exposes every registered tool. Names follow the daemon's REST tool
    /// naming — `__`-separated where RPC method names use dots (e.g.
    /// `aurora.switch_model` is `aurora__switch_model`). Large tool sets
    /// dominate prompt-eval time on local models, so latency-sensitive
    /// clients should pass a lean set — e.g. a voice agent might use
    /// `["aurora__switch_model", "aurora__list_models",
    /// "aurora__current_model"]` plus its memory tools.
    #[serde(default)]
    pub tool_filter: Option<Vec<String>>,
}

/// Information about a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ts/src/generated/types/"))]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: SessionId,
    pub persistence: Persistence,
    pub model_id: String,
    pub created_at: String,
}

/// A user message sent to a session.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts", derive(TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ts/src/generated/types/"))]
pub struct UserMessage {
    pub content: Vec<InputContent>,
}

/// A seed message.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts", derive(TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ts/src/generated/types/"))]
pub struct SeedMessage {
    pub role: llm::Role,
    pub content: Vec<InputContent>,
}

#[simply_rpc::rpc_service("session")]
#[async_trait]
pub trait SessionApi: Send + Sync {
    #[rpc(stream = "/session/new", no_tool)]
    async fn create_session(
        &self,
        ctx: &RequestContext,
        options: CreateSessionOptions,
    ) -> anyhow::Result<(SessionInfo, broadcast::Receiver<DaemonEvent>)>;

    #[rpc(stream = "/session/{session_id}/subscribe", no_tool)]
    async fn subscribe_session(
        &self,
        ctx: &RequestContext,
        session_id: &SessionId,
    ) -> anyhow::Result<broadcast::Receiver<DaemonEvent>>;

    #[rpc(get = "/session")]
    async fn list_sessions(&self, ctx: &RequestContext) -> anyhow::Result<Vec<SessionInfo>>;

    #[rpc(post = "/session/{session_id}/message", no_tool)]
    async fn send_message(
        &self,
        ctx: &RequestContext,
        session_id: &SessionId,
        message: UserMessage,
    ) -> anyhow::Result<()>;

    #[rpc(put = "/session/{session_id}/model", no_tool)]
    async fn set_model(&self, ctx: &RequestContext, session_id: &SessionId, model_id: &str) -> anyhow::Result<()>;

    #[rpc(delete = "/session/{session_id}", no_tool)]
    async fn close_session(&self, ctx: &RequestContext, session_id: &SessionId) -> anyhow::Result<()>;

    #[rpc(delete = "/session", no_tool)]
    async fn close_all_sessions(&self) -> anyhow::Result<()>;

    #[rpc(post = "/session/event", no_tool)]
    async fn push_event(&self, ctx: &RequestContext, event: InboundEvent) -> anyhow::Result<()>;
}
