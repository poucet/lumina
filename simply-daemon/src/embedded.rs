//! In-process implementation of the daemon API traits.
//!
//! EmbeddedDaemon is a thin assembly — it holds pre-built services and
//! implements SessionApi/ConversationApi directly (tightly coupled to
//! session state). All other traits delegate to services.
//!
//! Use `DaemonBuilder` (in builder.rs) to construct an EmbeddedDaemon.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc, Mutex};

use simply_core::storage::coordinator::StorageCoordinator;
use simply_core::storage::ids::ConversationId;
use simply_core::storage::session::ResolvedMessage;
use simply_core::storage::traits::{StorageTypes, Stores};
use simply_core::storage::{EntityResolver, StoreEntityResolver};
use simply_core::{
    Persistence, SessionEvent, SessionEventSender, SessionManager, ToolService,
};

use crate::api::*;
use crate::mcp::McpService;
use crate::services::*;

// ---------------------------------------------------------------------------
// Session bookkeeping
// ---------------------------------------------------------------------------

struct ManagedSession {
    info: SessionInfo,
    manager: SessionManager,
    event_broadcast: broadcast::Sender<DaemonEvent>,
}

// ---------------------------------------------------------------------------
// EmbeddedDaemon
// ---------------------------------------------------------------------------

pub struct EmbeddedDaemon<S: StorageTypes> {
    coordinator: Arc<StorageCoordinator<S>>,
    stores: Arc<dyn Stores<S>>,
    sessions: Mutex<HashMap<SessionId, ManagedSession>>,
    mcp: Arc<McpService>,
    model: Arc<ModelService>,
    asset: Arc<AssetService<S>>,
    entity: Arc<EntityService<S>>,
    voice: Arc<VoiceService>,
    core: Arc<CoreService>,
    search: Arc<SearchService<S>>,
    vault: Arc<VaultService<S>>,
    user_svc: Arc<UserService<S>>,
    tools: Arc<ToolRegistry>,
}

impl<S: StorageTypes> EmbeddedDaemon<S>
{
    /// Assemble from pre-built services. Use `DaemonBuilder` to construct.
    #[allow(clippy::too_many_arguments)] // one slot per service; only DaemonBuilder calls it
    pub(crate) fn assemble(
        coordinator: Arc<StorageCoordinator<S>>,
        stores: Arc<dyn Stores<S>>,
        mcp: Arc<McpService>,
        model: Arc<ModelService>,
        asset: Arc<AssetService<S>>,
        entity: Arc<EntityService<S>>,
        voice: Arc<VoiceService>,
        core: Arc<CoreService>,
        search: Arc<SearchService<S>>,
        vault: Arc<VaultService<S>>,
        user_svc: Arc<UserService<S>>,
        tools: Arc<ToolRegistry>,
    ) -> anyhow::Result<Arc<Self>> {
        let daemon = Arc::new(Self {
            coordinator,
            stores,
            sessions: Mutex::new(HashMap::new()),
            mcp,
            model,
            asset,
            entity,
            voice,
            core,
            search,
            vault,
            user_svc,
            tools,
        });
        Self::spawn_session_reaper(Arc::clone(&daemon));
        Ok(daemon)
    }

    // -- Service accessors for main.rs / ServiceRouter registration ----------

    pub fn mcp_service(&self) -> Arc<dyn McpApi> { self.tools.clone() }
    pub fn oauth_service(&self) -> Arc<dyn OAuthApi> { self.mcp.clone() }
    pub fn model_service(&self) -> Arc<ModelService> { Arc::clone(&self.model) }
    pub fn asset_service(&self) -> Arc<AssetService<S>> { Arc::clone(&self.asset) }
    pub fn entity_service(&self) -> Arc<EntityService<S>> { Arc::clone(&self.entity) }
    pub fn voice_service(&self) -> Arc<VoiceService> { Arc::clone(&self.voice) }
    pub fn core_service(&self) -> Arc<CoreService> { Arc::clone(&self.core) }
    pub fn search_service(&self) -> Arc<SearchService<S>> { Arc::clone(&self.search) }
    pub fn vault_service(&self) -> Arc<VaultService<S>> { Arc::clone(&self.vault) }
    pub fn user_service(&self) -> Arc<UserService<S>> { Arc::clone(&self.user_svc) }

    /// Get the ToolRegistry (for DaemonHandle to pass to the server).
    pub fn tool_registry(&self) -> &Arc<ToolRegistry> { &self.tools }

    /// Get the daemon tool services (for ServiceRouter registration).
    pub fn daemon_tool_services(&self) -> Vec<Arc<dyn simply_rpc::RestService>> {
        self.tools.daemon_tool_services().to_vec()
    }

    pub fn oauth_redirect_uri(&self) -> String { self.mcp.oauth_redirect_uri() }
    pub fn oauth_tracker(&self) -> Arc<crate::oauth::callback::CallbackTracker> { self.mcp.oauth_tracker() }
    pub fn stores(&self) -> &Arc<dyn Stores<S>> { &self.stores }
    pub fn coordinator(&self) -> &Arc<StorageCoordinator<S>> { &self.coordinator }

    // -- Internal helpers -----------------------------------------------------

    fn spawn_session_reaper(daemon: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                let mut sessions = daemon.sessions.lock().await;
                let before = sessions.len();
                sessions.retain(|sid, managed| {
                    if managed.event_broadcast.receiver_count() == 0 {
                        tracing::info!(session_id = %sid, "reaping orphaned session");
                        false
                    } else {
                        true
                    }
                });
                let reaped = before - sessions.len();
                if reaped > 0 {
                    tracing::info!(reaped, remaining = sessions.len(), "session reaper sweep");
                }
            }
        });
    }

    fn spawn_event_forwarder(
        session_id: &SessionId,
        broadcast_tx: &broadcast::Sender<DaemonEvent>,
    ) -> SessionEventSender {
        let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<(String, SessionEvent)>();
        let broadcast_tx = broadcast_tx.clone();
        let sid = session_id.clone();

        tokio::spawn(async move {
            while let Some((_key, event)) = evt_rx.recv().await {
                for de in Self::session_event_to_daemon_events(event) {
                    let _ = broadcast_tx.send(de);
                }
            }
            tracing::debug!(session_id = %sid, "event forwarder stopped");
        });

        evt_tx
    }

    fn session_event_to_daemon_events(event: SessionEvent) -> Vec<DaemonEvent> {
        match event {
            SessionEvent::UserMessageAdded(msg) => vec![DaemonEvent::UserMessage(msg)],
            SessionEvent::TextDelta(text) => vec![DaemonEvent::TextDelta(text)],
            SessionEvent::ContentBlock(block) => vec![DaemonEvent::AssistantContent(block)],
            SessionEvent::ToolCallStart { id, name, arguments } => {
                vec![DaemonEvent::ToolCall { id, name, arguments }]
            }
            SessionEvent::ToolCallResult { id, content } => {
                let result = serde_json::to_value(&content).unwrap_or_default();
                vec![DaemonEvent::ToolResult { id, result }]
            }
            SessionEvent::AssistantMessage(msg) => {
                msg.payload.content.into_iter().map(DaemonEvent::AssistantContent).collect()
            }
            SessionEvent::TurnComplete { .. } => vec![DaemonEvent::TurnComplete],
            SessionEvent::Error(err) => vec![DaemonEvent::Error(err)],
            SessionEvent::ModelChanged(_) => vec![],
        }
    }

    fn convert_seed(seed: Vec<SeedMessage>) -> Vec<llm::ChatMessage> {
        seed.into_iter()
            .map(|sm| {
                let blocks: Vec<llm::ContentBlock> = sm
                    .content
                    .into_iter()
                    .filter_map(|c| c.into_content_block_inline())
                    .collect();
                llm::ChatMessage::new(sm.role, llm::ChatPayload::new(blocks))
            })
            .collect()
    }

    fn convert_input_inline(content: Vec<simply_core::storage::content::InputContent>) -> Vec<llm::ContentBlock> {
        content
            .into_iter()
            .filter_map(|c| c.into_content_block_inline())
            .collect()
    }

    fn require_user(&self, ctx: &simply_rpc::RequestContext) -> anyhow::Result<simply_core::storage::ids::UserId> {
        ctx.scope.user_id.as_ref()
            .map(simply_core::storage::ids::UserId::from_string)
            .ok_or_else(|| anyhow::anyhow!("authentication required"))
    }

    fn make_session_info(session_id: &SessionId, persistence: Persistence, model_id: String) -> SessionInfo {
        SessionInfo {
            id: session_id.clone(),
            persistence,
            model_id,
            created_at: {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                format!("{}", now.as_secs())
            },
        }
    }
}

// ---------------------------------------------------------------------------
// SessionApi
// ---------------------------------------------------------------------------

#[async_trait]
impl<S: StorageTypes> SessionApi for EmbeddedDaemon<S>
{
    async fn create_session(
        &self,
        ctx: &simply_rpc::RequestContext,
        options: CreateSessionOptions,
    ) -> anyhow::Result<(SessionInfo, broadcast::Receiver<DaemonEvent>)> {
        let persistence = options.persistence.clone().unwrap_or(Persistence::Ephemeral);

        if let Persistence::Persistent { ref conversation_id } = persistence {
            let sessions = self.sessions.lock().await;
            if let Some(managed) = sessions.values().find(|m| matches!(
                &m.info.persistence,
                Persistence::Persistent { conversation_id: cid } if cid == conversation_id
            )) {
                return Ok((managed.info.clone(), managed.event_broadcast.subscribe()));
            }
        }

        let session_id = SessionId::generate();
        let model_id = match options.model_id {
            Some(id) => id,
            None => self.model.default_model().await,
        };
        let model = llm::create_model(&model_id)?;
        let seed_messages = Self::convert_seed(options.seed);
        let (broadcast_tx, broadcast_rx) = broadcast::channel(256);
        let info = Self::make_session_info(&session_id, persistence.clone(), model_id);

        // Bind the session's tool service to the caller's ctx — preserves
        // origin metadata (e.g. discord.guild_id) and augments with any
        // stored OAuth tokens for the user.
        let tools: Arc<dyn ToolService> = self.tools.for_ctx(ctx.clone()).await;
        // Optionally restrict to the caller's allow-list (exact REST tool
        // names, `__`-separated) — lean tool sets cut local-model prompt-eval
        // time dramatically.
        let tools: Arc<dyn ToolService> = match options.tool_filter {
            Some(filter) => Arc::new(FilteredToolService::new(tools, filter)),
            None => tools,
        };
        let entity_resolver: Arc<dyn EntityResolver> = Arc::new(StoreEntityResolver::new(
            self.stores.entity(),
            self.stores.text(),
        ));
        let evt_tx = Self::spawn_event_forwarder(&session_id, &broadcast_tx);

        let manager = SessionManager::create(
            session_id.as_str().to_string(),
            persistence,
            seed_messages,
            options.system_prompt,
            model,
            tools,
            Arc::clone(&self.coordinator),
            entity_resolver,
            simply_core::ExecutionContext::default(),
            evt_tx,
        ).await?;

        self.sessions.lock().await.insert(session_id.clone(), ManagedSession {
            info: info.clone(),
            manager,
            event_broadcast: broadcast_tx,
        });

        tracing::info!(session_id = %session_id, "session created");
        Ok((info, broadcast_rx))
    }

    async fn subscribe_session(&self, _ctx: &simply_rpc::RequestContext, session_id: &SessionId) -> anyhow::Result<broadcast::Receiver<DaemonEvent>> {
        let sessions = self.sessions.lock().await;
        let managed = sessions.get(session_id)
            .ok_or_else(|| anyhow::anyhow!("session not found: {session_id}"))?;
        Ok(managed.event_broadcast.subscribe())
    }

    async fn close_session(&self, _ctx: &simply_rpc::RequestContext, session_id: &SessionId) -> anyhow::Result<()> {
        if self.sessions.lock().await.remove(session_id).is_some() {
            tracing::info!(session_id = %session_id, "session closed");
        }
        Ok(())
    }

    async fn close_all_sessions(&self) -> anyhow::Result<()> {
        let mut sessions = self.sessions.lock().await;
        let count = sessions.len();
        sessions.clear();
        tracing::info!(count, "all sessions closed");
        Ok(())
    }

    async fn list_sessions(&self, _ctx: &simply_rpc::RequestContext) -> anyhow::Result<Vec<SessionInfo>> {
        Ok(self.sessions.lock().await.values().map(|s| s.info.clone()).collect())
    }

    async fn send_message(&self, _ctx: &simply_rpc::RequestContext, session_id: &SessionId, message: UserMessage) -> anyhow::Result<()> {
        let sessions = self.sessions.lock().await;
        let managed = sessions.get(session_id)
            .ok_or_else(|| anyhow::anyhow!("session not found: {session_id}"))?;
        managed.manager.send_message(Self::convert_input_inline(message.content));
        Ok(())
    }

    async fn set_model(&self, _ctx: &simply_rpc::RequestContext, session_id: &SessionId, model_id: &str) -> anyhow::Result<()> {
        let new_model = llm::create_model(model_id)?;
        let mut sessions = self.sessions.lock().await;
        let managed = sessions.get_mut(session_id)
            .ok_or_else(|| anyhow::anyhow!("session not found: {session_id}"))?;
        managed.manager.set_model(new_model);
        managed.info.model_id = model_id.to_string();
        Ok(())
    }

    async fn push_event(&self, _ctx: &simply_rpc::RequestContext, event: InboundEvent) -> anyhow::Result<()> {
        tracing::info!(event_type = %event.event_type, "inbound event received");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ConversationApi
// ---------------------------------------------------------------------------

#[async_trait]
impl<S: StorageTypes> ConversationApi for EmbeddedDaemon<S>
{
    async fn create_conversation(&self, ctx: &simply_rpc::RequestContext, name: Option<String>) -> anyhow::Result<ConversationId> {
        let user_id = self.require_user(ctx)?;
        self.coordinator.create_conversation(&user_id, name.as_deref()).await
    }

    async fn list_conversations(&self, ctx: &simply_rpc::RequestContext) -> anyhow::Result<Vec<ConversationInfo>> {
        use simply_core::storage::{EntityStore, EntityType, TurnStore};
        let user_id = self.require_user(ctx)?;
        let entities = self.stores.entity()
            .list_entities(&user_id, Some(&EntityType::conversation())).await?;
        let mut result = Vec::with_capacity(entities.len());
        for entity in entities {
            let turn_count = self.stores.turn().get_turn_count(&entity.id).await.unwrap_or(0);
            result.push(ConversationInfo {
                id: entity.id.clone(),
                name: entity.name.clone(),
                message_count: turn_count,
                created_at: entity.created_at,
            });
        }
        Ok(result)
    }

    async fn delete_conversation(&self, ctx: &simply_rpc::RequestContext, conversation_id: &ConversationId) -> anyhow::Result<()> {
        use simply_core::storage::EntityStore;
        let session_id = {
            let sessions = self.sessions.lock().await;
            sessions.iter()
                .find(|(_, m)| matches!(
                    &m.info.persistence,
                    Persistence::Persistent { conversation_id: cid } if cid == conversation_id
                ))
                .map(|(sid, _)| sid.clone())
        };
        if let Some(sid) = session_id {
            let _ = self.close_session(ctx, &sid).await;
        }
        self.stores.entity().delete_entity(conversation_id).await
    }

    async fn rename_conversation(&self, _ctx: &simply_rpc::RequestContext, conversation_id: &ConversationId, name: &str) -> anyhow::Result<()> {
        use simply_core::storage::EntityStore;
        let mut entity = self.stores.entity().get_entity(conversation_id).await?
            .ok_or_else(|| anyhow::anyhow!("conversation not found: {conversation_id}"))?;
        entity.name = if name.trim().is_empty() { None } else { Some(name.to_string()) };
        self.stores.entity().update_entity(conversation_id, &entity).await
    }

    async fn get_messages(&self, _ctx: &simply_rpc::RequestContext, conversation_id: &ConversationId) -> anyhow::Result<Vec<ResolvedMessage>> {
        self.coordinator.open_session(conversation_id).await
    }
}

// ---------------------------------------------------------------------------
// Daemon trait
// ---------------------------------------------------------------------------

#[async_trait]
impl<S: StorageTypes> Daemon for EmbeddedDaemon<S>
{
    fn session(&self) -> &dyn SessionApi { self }
    fn conversation(&self) -> &dyn ConversationApi { self }
    fn entity(&self) -> &dyn EntityApi { &*self.entity }
    fn mcp(&self) -> &dyn McpApi { &*self.tools }
    fn oauth(&self) -> &dyn OAuthApi { &*self.mcp }
    fn model(&self) -> &dyn ModelApi { &*self.model }
    fn asset(&self) -> &dyn AssetApi { &*self.asset }
    fn voice(&self) -> &dyn VoiceApi { &*self.voice }
    fn core(&self) -> &dyn CoreApi { &*self.core }
    fn search(&self) -> &dyn SearchApi { &*self.search }
    fn vault(&self) -> &dyn VaultApi { &*self.vault }
    fn user(&self) -> &dyn UserApi { &*self.user_svc }
    fn skills(&self) -> &dyn SkillsApi { &*self.tools }
    fn tools(&self) -> &dyn ToolService { &*self.tools }

    async fn register_skill(&self, skill: Arc<dyn simply_daemon_api::Skill>) -> anyhow::Result<()> {
        // Handle OAuth requirements
        let oauth_reqs = skill.oauth_requirements();
        for req in &oauth_reqs {
            self.register_oauth_provider(req).await?;
        }

        // Wrap as EmbeddedToolProvider and register directly — no handler conversion
        let count = skill.tools().len();
        let provider = Arc::new(crate::services::providers::EmbeddedToolProvider::new(skill));
        self.tools.register(provider).await;
        tracing::info!(count, "skill registered (embedded, direct)");
        Ok(())
    }

    async fn register_client_tools(
        &self,
        tools: Vec<llm::ToolDefinition>,
        handler: simply_daemon_api::ToolCallHandler,
    ) -> anyhow::Result<()> {
        // Embedded: wrap as a ClientToolProvider and register with ToolRegistry
        let count = tools.len();
        let id = format!("client-{}", count);
        let provider = Arc::new(crate::services::providers::ClientToolProvider::from_definitions(
            id, tools, handler,
        ));
        self.tools.register(provider).await;
        tracing::info!(count, "client tools registered (embedded, direct)");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tool filtering
// ---------------------------------------------------------------------------

/// Restricts a [`ToolService`] to an allow-list of exact tool names
/// (daemon REST naming: RPC dots become `__`, e.g. `aurora__switch_model`).
/// Filtered-out tools are hidden from `get_definitions()` and rejected by
/// `call_tool()`.
struct FilteredToolService {
    inner: Arc<dyn ToolService>,
    allow: std::collections::HashSet<String>,
}

impl FilteredToolService {
    fn new(inner: Arc<dyn ToolService>, allow: Vec<String>) -> Self {
        Self { inner, allow: allow.into_iter().collect() }
    }
}

#[async_trait]
impl ToolService for FilteredToolService {
    async fn get_definitions(&self) -> Vec<llm::ToolDefinition> {
        self.inner
            .get_definitions()
            .await
            .into_iter()
            .filter(|d| self.allow.contains(&d.name))
            .collect()
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<Vec<llm::ToolResultContent>> {
        if !self.allow.contains(name) {
            anyhow::bail!("tool '{name}' is not in this session's tool_filter");
        }
        self.inner.call_tool(name, arguments).await
    }
}
