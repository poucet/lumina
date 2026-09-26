use super::chat::api::ListModelsResponse;
use super::chat::model::OllamaChatModel;
use super::embedding::OllamaEmbeddingProvider;
use crate::{ChatModel, ModelProvider};
use crate::client::Client;
use async_trait::async_trait;
use std::sync::Arc;

/// How long Ollama keeps a model resident in memory after a request
/// (`keep_alive`: a duration string, or "-1" for indefinitely). Ollama's own
/// default is 5m, after which the next turn pays the full model cold-load
/// (10s+ for multi-GB models) — far longer than typical conversation gaps.
/// 30m keeps interactive sessions warm without pinning the model forever.
/// Provider-level for now; there is no ollama section in the config crate to
/// plumb it from — override per provider via [`OllamaProvider::with_keep_alive`].
pub const DEFAULT_KEEP_ALIVE: &str = "30m";

pub struct OllamaProvider {
    client: Client,
    base_url: String,
    keep_alive: String,
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new("http://localhost:11434")
    }
}

impl OllamaProvider {
    pub fn new(base_url: &str) -> Self {
        OllamaProvider {
            client: Client::default(),
            base_url: base_url.to_string(),
            keep_alive: DEFAULT_KEEP_ALIVE.to_string(),
        }
    }

    /// Override the `keep_alive` sent with every chat request (duration
    /// string like "10m", or "-1" to keep the model loaded indefinitely).
    pub fn with_keep_alive(mut self, keep_alive: impl Into<String>) -> Self {
        self.keep_alive = keep_alive.into();
        self
    }

    /// Create an embedding provider using this provider's client and base URL.
    pub fn create_embedding_provider(&self, model: &str) -> OllamaEmbeddingProvider {
        OllamaEmbeddingProvider::new(self.client.clone(), self.base_url.clone(), model.to_string())
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    async fn list_models(&self) -> anyhow::Result<Vec<crate::ModelDefinition>> {
        let url = format!("{}/api/tags", self.base_url);
        let response: ListModelsResponse = self.client.get(&url).await?;
        Ok(response.models.into_iter().map(|m| m.into()).collect())
    }

    fn create_chat_model(&self, model_name: &str) -> Option<Arc<dyn ChatModel + Send + Sync>> {
        Some(Arc::new(OllamaChatModel::new(
            self.client.clone(),
            self.base_url.clone(),
            model_name.to_string(),
            self.keep_alive.clone(),
        )))
    }
}
