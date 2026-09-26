//! Model listing and management service.

use async_trait::async_trait;
use tokio::sync::Mutex;
use crate::api::*;

pub struct ModelService {
    default_model_id: Mutex<String>,
    cached_models: Mutex<Option<Vec<llm::ModelInfo>>>,
}

impl ModelService {
    pub fn new(default_model_id: String) -> Self {
        Self {
            default_model_id: Mutex::new(default_model_id),
            cached_models: Mutex::new(None),
        }
    }

    pub async fn default_model(&self) -> String {
        self.default_model_id.lock().await.clone()
    }

    /// Whether a provider has credentials available (settings key or env var).
    /// Providers that need no API key (e.g. ollama) are always considered configured.
    fn provider_configured(settings: &config::Settings, info: &llm::ProviderInfo) -> bool {
        match &info.api_key_env {
            None => true,
            Some(env) => settings.has_api_key(&info.name) || std::env::var(env).is_ok(),
        }
    }

    async fn fetch_all_models(&self) -> Vec<llm::ModelInfo> {
        let settings = config::Settings::load();
        let mut all = Vec::new();
        for info in llm::list_providers() {
            // Skip providers the user never configured; failing to list their
            // models is expected and not worth a warning.
            if !Self::provider_configured(&settings, &info) {
                tracing::debug!(provider = %info.name, "skipping model fetch: provider not configured");
                continue;
            }
            match llm::list_models(&info.name).await {
                Ok(models) => all.extend(models),
                Err(e) => {
                    let msg = e.to_string();
                    let auth_failure = msg.contains("401") || msg.contains("403");
                    if auth_failure {
                        tracing::warn!(
                            provider = %info.name,
                            error = %e,
                            "failed to fetch models: stored API key appears invalid — update or remove it in settings"
                        );
                    } else {
                        tracing::warn!(provider = %info.name, error = %e, "failed to fetch models");
                    }
                }
            }
        }
        *self.cached_models.lock().await = Some(all.clone());
        all
    }
}

#[async_trait]
impl ModelApi for ModelService {
    async fn list_models(&self) -> anyhow::Result<Vec<llm::ModelInfo>> {
        if let Some(cached) = self.cached_models.lock().await.clone() {
            return Ok(cached);
        }
        Ok(self.fetch_all_models().await)
    }

    async fn list_providers(&self) -> Vec<llm::ProviderInfo> {
        llm::list_providers()
    }

    async fn default_model_id(&self) -> String {
        self.default_model_id.lock().await.clone()
    }

    async fn set_default_model(&self, model_id: &str) -> anyhow::Result<()> {
        let _ = llm::create_model(model_id)?;
        *self.default_model_id.lock().await = model_id.to_string();
        // Invalidate cache when model changes (provider config may have changed)
        *self.cached_models.lock().await = None;
        Ok(())
    }
}
