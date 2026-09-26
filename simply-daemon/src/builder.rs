//! Daemon builder — constructs services and assembles the EmbeddedDaemon.
//!
//! This module provides `build_daemon()` which is the single entry point for
//! constructing a fully wired EmbeddedDaemon. Used by:
//! - `simply-daemon/src/main.rs` (standalone daemon)
//! - `lumina/src/main.rs` (embedded in Discord bot)
//! - Any future host that needs an in-process daemon

use std::sync::Arc;

use simply_core::storage::traits::{StorageTypes, Stores, UserStore};
use simply_core::storage::coordinator::StorageCoordinator;
use simply_rpc::ServiceRouter;

use crate::api::*;
use crate::embedded::EmbeddedDaemon;
use crate::embedding_queue::{ChannelEmbeddingQueue, EmbeddingQueue};
use crate::mcp::{McpService, McpServiceConfig};
use crate::services::*;

/// Configuration for building an EmbeddedDaemon.
pub struct DaemonBuilder<S: StorageTypes> {
    pub stores: Arc<dyn Stores<S>>,
    pub coordinator: Arc<StorageCoordinator<S>>,
    pub vector_store: Arc<dyn simply_core::embedding::VectorStore>,
    pub token_store: Arc<crate::services::token_store::TransientTokenStore>,
    pub voice: VoiceService,
}

/// A running daemon — call `serve()` to block until shutdown.
pub struct DaemonHandle<S: StorageTypes> {
    pub daemon: Arc<EmbeddedDaemon<S>>,
    kill_rx: tokio::sync::mpsc::Receiver<()>,
    token_store: Arc<crate::services::token_store::TransientTokenStore>,
}

impl<S: StorageTypes> DaemonHandle<S>
where
    S::User: simply_core::storage::traits::UserStore,
{
    /// Start the HTTP/WS server and block until shutdown (Ctrl+C, SIGTERM, or /daemon/kill).
    pub async fn serve(mut self, port: u16, daemon_secret: String) -> anyhow::Result<()>
    where
        S::User: simply_core::storage::traits::UserStore,
    {
        let rest_dispatcher = build_service_router(&self.daemon);
        let tracker = crate::net::server::ConnectionTracker::new();
        let oauth_tracker = self.daemon.oauth_tracker();
        let user_store: Arc<dyn simply_core::storage::traits::UserStore> = self.daemon.stores().user();
        let tools = Arc::clone(self.daemon.tool_registry());

        let server = crate::net::rest::start(crate::net::rest::ServerConfig {
            rest_dispatcher,
            port,
            tracker,
            daemon_secret,
            user_store,
            token_store: self.token_store,
            oauth_tracker: Some(oauth_tracker),
            tools,
        }).await?;

        let actual_port = server.port();
        tracing::info!(port = actual_port, "daemon ready");
        println!("{actual_port}");

        // Wait for shutdown
        tokio::select! {
            _ = shutdown_signal() => {}
            _ = self.kill_rx.recv() => { tracing::info!("kill received via REST"); }
        }

        tracing::info!("shutting down");
        self.daemon.session().close_all_sessions().await?;
        tracing::info!("shutdown complete");
        Ok(())
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }
}

impl<S: StorageTypes> DaemonBuilder<S>
where
    S::User: simply_core::storage::traits::UserStore,
{
    /// Build the daemon and return a handle for serving.
    pub async fn build(self) -> anyhow::Result<DaemonHandle<S>> {
        let (kill_tx, kill_rx) = tokio::sync::mpsc::channel(1);
        let token_store = Arc::clone(&self.token_store);
        let settings = config::Settings::load();

        // On a fresh database the admin UI and clients send X-User-Id for a user
        // that may not yet exist in `users`, causing FK failures on the first
        // entity/document insert. If settings specify a `user_email`, ensure a
        // matching row is present up front.
        if let Some(email) = settings
            .user_email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match self.stores.user().get_or_create_user_by_email(email).await {
                Ok(user) => tracing::info!(user_id = %user.id, email = %email, "bootstrapped user from settings"),
                Err(e) => tracing::warn!(email = %email, error = %e, "failed to bootstrap user from settings"),
            }
        }

        const FALLBACK_MODEL_ID: &str = "claude/models/claude-sonnet-4-5-20250929";
        let default_model_id = settings.default_model.clone()
            .unwrap_or_else(|| FALLBACK_MODEL_ID.to_string());

        let daemon_port = settings.daemon_port.unwrap_or(config::DEFAULT_DAEMON_PORT);
        let public_url = settings.public_url.clone()
            .unwrap_or_else(|| format!("http://localhost:{}", daemon_port));

        let mcp = Arc::new(McpService::start(
            McpServiceConfig { public_url },
            Arc::clone(&token_store),
        ).await?);

        let model = Arc::new(ModelService::new(default_model_id));
        let asset = Arc::new(AssetService::new(
            Arc::clone(&self.coordinator),
            Arc::clone(&self.stores),
        ));
        let voice = Arc::new(self.voice);

        // Embedding / RAG
        let embedding_config = &settings.embedding;
        let embedding_provider = create_embedding_provider(embedding_config, &settings)?;
        let chunker: Arc<dyn simply_core::embedding::Chunker> = Arc::new(
            simply_core::embedding::RecursiveCharacterChunker::new(
                embedding_config.chunk_size,
                embedding_config.chunk_overlap,
            ),
        );
        let embedding_queue = Arc::new(ChannelEmbeddingQueue::new(
            Arc::clone(&embedding_provider),
            Arc::clone(&chunker),
            Arc::clone(&self.vector_store),
        ));
        tracing::info!(
            provider = embedding_config.provider.as_str(),
            model = embedding_config.model.as_str(),
            "embedding provider initialized"
        );

        let vault_root = settings
            .ensure_vault_root_exists()?
            .ok_or_else(|| anyhow::anyhow!("vault root unavailable"))?;
        let user_email_cache = Arc::new(crate::services::user::UserEmailCache::new());
        let entity = Arc::new(
            EntityService::new(
                Arc::clone(&self.coordinator),
                Arc::clone(&self.stores),
                vault_root.clone(),
            )
            .with_embedding(embedding_queue.clone() as Arc<dyn EmbeddingQueue>)
            .with_vector_store(Arc::clone(&self.vector_store))
            .with_user_email_cache(Arc::clone(&user_email_cache))
        );
        let core = Arc::new(CoreService::new(kill_tx));
        let user_svc = Arc::new(UserService::new(Arc::clone(&self.stores)));
        let search = Arc::new(SearchService::new(
            embedding_provider,
            Arc::clone(&self.vector_store),
            embedding_queue.clone() as Arc<dyn EmbeddingQueue>,
            Arc::clone(&self.stores),
        ));
        let vault = Arc::new(
            VaultService::new(
                vault_root,
                Arc::clone(&self.coordinator),
                Arc::clone(&self.stores),
            )
            .with_embedding(embedding_queue.clone() as Arc<dyn EmbeddingQueue>)
        );
        Arc::clone(&vault).spawn_polling_watcher();

        // Note: McpApi is NOT registered here — it's added to the ServiceRouter
        // separately, pointing at ToolRegistry (which delegates management to
        // McpService but handles list_all_tools/call_tool_direct across all providers).
        let daemon_tools = Arc::new(
            DaemonToolService::new()
                .register(<dyn AssetApi>::service(asset.clone()))
                .register(<dyn EntityApi>::service(entity.clone()))
                .register(<dyn ModelApi>::service(model.clone()))
                .register(<dyn CoreApi>::service(core.clone()))
                .register(<dyn OAuthApi>::service(mcp.clone()))
                .register(<dyn VoiceApi>::service(voice.clone()))
                .register(<dyn SearchApi>::service(search.clone()))
                .register(<dyn VaultApi>::service(vault.clone()))
                .register(<dyn UserApi>::service(user_svc.clone())),
        );

        let tools = Arc::new(ToolRegistry::new(
            daemon_tools,
            Arc::clone(&token_store),
            Arc::clone(&mcp),
        ));
        // MCP servers are sourced live from McpService on every
        // get_definitions/call_tool — no startup snapshot is needed,
        // so servers that connect asynchronously (or later, at runtime)
        // are picked up automatically.

        let daemon = EmbeddedDaemon::assemble(
            self.coordinator,
            self.stores,
            mcp,
            model,
            asset,
            entity,
            voice,
            core,
            search,
            vault,
            user_svc,
            tools.clone(),
        )?;

        Ok(DaemonHandle { daemon, kill_rx, token_store })
    }
}

/// Build the ServiceRouter from a daemon.
///
/// Two categories of services are registered:
/// - **Directly-on-daemon APIs** (`SessionApi`, `ConversationApi`,
///   `McpApi`, `SkillsApi`): implemented on `EmbeddedDaemon` itself, so
///   they're added here.
/// - **Split-out services** (`EntityApi`, `AssetApi`, `ModelApi`,
///   `OAuthApi`, `VoiceApi`, `SearchApi`, `UserApi`, `CoreApi`): each
///   has its own struct registered into the daemon's
///   `DaemonToolService` at build time; this loop re-registers them with
///   the `ServiceRouter` via `daemon_tool_services()`.
pub fn build_service_router<S: StorageTypes>(
    daemon: &Arc<EmbeddedDaemon<S>>,
) -> Arc<ServiceRouter>
{
    let session_svc: Arc<dyn SessionApi> = daemon.clone();
    let conversation_svc: Arc<dyn ConversationApi> = daemon.clone();
    let mcp_svc: Arc<dyn McpApi> = daemon.tool_registry().clone();
    let skills_svc: Arc<dyn SkillsApi> = daemon.tool_registry().clone();

    let mut router = ServiceRouter::new()
        .register(<dyn SessionApi>::service(session_svc))
        .register(<dyn ConversationApi>::service(conversation_svc))
        .register(<dyn McpApi>::service(mcp_svc))
        .register(<dyn SkillsApi>::service(skills_svc));

    for svc in daemon.daemon_tool_services() {
        router = router.register(svc);
    }

    Arc::new(router)
}

/// Create voice service with all configured providers.
pub fn create_voice_service() -> VoiceService {
    let settings = config::Settings::load();
    let mut voice = VoiceService::new();

    #[cfg(feature = "whisper")]
    if let Some(path) = config::PathManager::whisper_model_path() {
        if path.exists() {
            match simply_voice::WhisperProvider::new(&path) {
                Ok(p) => {
                    tracing::info!("whisper STT loaded from {}", path.display());
                    voice = voice.register_stt("whisper", "Whisper (local)", Arc::new(p));
                }
                Err(e) => tracing::warn!("failed to load whisper model: {e}"),
            }
        }
    }

    // These load large local models at construction — warn and skip on
    // failure rather than taking the daemon down.
    #[cfg(feature = "kyutai")]
    {
        let hf_repo = std::env::var("KYUTAI_STT_REPO").ok();
        match simply_voice::KyutaiSttProvider::new(hf_repo, false) {
            Ok(p) => {
                tracing::info!("kyutai STT loaded");
                voice = voice.register_stt("kyutai", "Kyutai STT (local)", Arc::new(p));
            }
            Err(e) => tracing::warn!("failed to load kyutai STT model: {e:#}"),
        }
    }

    #[cfg(feature = "pocket-tts")]
    {
        // Custom voices (`<stem>.safetensors` / `<stem>.wav`) live in
        // `<data>/models/voices/` and are offered by their stem.
        match simply_voice::PocketTtsProvider::new("alba") {
            Ok(p) => {
                let p = match config::PathManager::models_dir() {
                    Some(models) => p.with_voice_dir(models.join("voices")),
                    None => p,
                };
                tracing::info!("pocket-tts TTS loaded");
                voice = voice.register_tts("pocket-tts", "Pocket TTS (local)", Arc::new(p));
            }
            Err(e) => tracing::warn!("failed to load pocket-tts model: {e:#}"),
        }
    }

    if let Ok(base_url) = std::env::var("VOXTRAL_BASE_URL") {
        let voxtral = Arc::new(simply_voice::VoxtralProvider::local(&base_url));
        voice = voice
            .register_stt("voxtral", "Voxtral (local)", voxtral.clone())
            .register_tts("voxtral", "Voxtral (local)", voxtral);
        tracing::info!(base_url = %base_url, "voxtral local STT + TTS registered");
    } else if let Some(key) = settings.get_api_key("mistral")
        .or_else(|| std::env::var("MISTRAL_API_KEY").ok()) {
        let voxtral = Arc::new(simply_voice::VoxtralProvider::new(key));
        voice = voice
            .register_stt("voxtral", "Voxtral (Mistral)", voxtral.clone())
            .register_tts("voxtral", "Voxtral (Mistral)", voxtral);
        tracing::info!("voxtral STT + TTS registered (Mistral API)");
    }

    if let Some(key) = settings.get_api_key("elevenlabs")
        .or_else(|| std::env::var("ELEVENLABS_API_KEY").ok()) {
        let elevenlabs = Arc::new(simply_voice::ElevenLabsProvider::new(key));
        voice = voice
            .register_stt("elevenlabs", "ElevenLabs", elevenlabs.clone())
            .register_tts("elevenlabs", "ElevenLabs", elevenlabs);
        tracing::info!("elevenlabs STT + TTS registered");
    }

    if let Some(key) = settings.get_api_key("google")
        .or_else(|| std::env::var("GOOGLE_API_KEY").ok()) {
        let gemini = Arc::new(simply_voice::GeminiProvider::new(key));
        voice = voice
            .register_stt("gemini", "Gemini (STT)", gemini.clone())
            .register_tts("gemini", "Gemini (TTS)", gemini);
        tracing::info!("gemini STT + TTS registered");
    }

    voice
}

/// Create an embedding provider from config.
pub fn create_embedding_provider(
    config: &config::EmbeddingConfig,
    settings: &config::Settings,
) -> anyhow::Result<Arc<dyn llm::EmbeddingProvider>> {
    match config.provider.as_str() {
        "local" => Ok(Arc::new(llm::providers::LocalEmbeddingProvider::new(&config.model)?)),
        "ollama" => {
            let provider = match std::env::var("OLLAMA_BASE_URL") {
                Ok(base_url) => llm::providers::OllamaProvider::new(&base_url),
                Err(_) => llm::providers::OllamaProvider::default(),
            };
            Ok(Arc::new(provider.create_embedding_provider(&config.model)))
        }
        "mistral" => {
            let api_key = settings.get_api_key("mistral")
                .or_else(|| std::env::var("MISTRAL_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("mistral API key not configured"))?;
            Ok(Arc::new(llm::providers::MistralProvider::default(&api_key).create_embedding_provider(&config.model)))
        }
        "gemini" => {
            let api_key = settings.get_api_key("google")
                .or_else(|| std::env::var("GOOGLE_API_KEY").ok())
                .or_else(|| settings.get_api_key("gemini"))
                .or_else(|| std::env::var("GEMINI_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("google/gemini API key not configured"))?;
            Ok(Arc::new(llm::providers::GeminiProvider::default(&api_key).create_embedding_provider(&config.model)))
        }
        "claude" | "voyage" => {
            let api_key = settings.get_api_key("voyage")
                .or_else(|| std::env::var("VOYAGE_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("voyage API key not configured"))?;
            Ok(Arc::new(llm::providers::VoyageEmbeddingProvider::new(&api_key, config.model.clone())))
        }
        other => anyhow::bail!("unknown embedding provider: {other}"),
    }
}
