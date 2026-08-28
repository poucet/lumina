use super::api::{OllamaRequest, OllamaResponse};
use crate::client::Client;
use crate::traffic_log;
use crate::{ChatMessage, ChatModel, ChatRequest, ChatStream};
use async_trait::async_trait;
use futures::StreamExt;

pub struct OllamaChatModel {
    client: Client,
    base_url: String,
    model_name: String,
    /// `keep_alive` sent with every chat request — see the provider's
    /// `DEFAULT_KEEP_ALIVE` for the rationale.
    keep_alive: String,
}

impl OllamaChatModel {
    pub fn new(client: Client, base_url: String, model_name: String, keep_alive: String) -> Self {
        OllamaChatModel {
            client,
            base_url,
            model_name,
            keep_alive,
        }
    }
}

#[async_trait]
impl ChatModel for OllamaChatModel {
    fn id(&self) -> &str {
        &self.model_name
    }

    fn name(&self) -> &str {
        &self.model_name
    }

    async fn chat(&self, request: &ChatRequest) -> anyhow::Result<ChatMessage> {
        let url = format!("{}/api/chat", self.base_url);

        let api_request = OllamaRequest::from_chat_request(
            &self.model_name,
            request,
            false,
            Some(self.keep_alive.clone()),
        );
        traffic_log::log_request(&self.model_name, &api_request);

        match self.client.post(url, &api_request).await {
            Ok(response) => {
                traffic_log::log_response(&self.model_name, &response);
                let response: OllamaResponse = response;
                Ok(response.into())
            }
            Err(e) => {
                traffic_log::log_error(&self.model_name, &e.to_string());
                Err(e)
            }
        }
    }

    async fn stream_chat(&self, request: &ChatRequest) -> anyhow::Result<ChatStream> {
        let url = format!("{}/api/chat", self.base_url);

        let api_request = OllamaRequest::from_chat_request(
            &self.model_name,
            request,
            true,
            Some(self.keep_alive.clone()),
        );
        traffic_log::log_stream_start(&self.model_name, &api_request);

        let streamed_response = self.client.post_stream(url, &api_request, |m| Some(m)).await?;
        Ok(Box::pin(
            streamed_response.map(|chunk: OllamaResponse| chunk.into()),
        ))
    }
}
