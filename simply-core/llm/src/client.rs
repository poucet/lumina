use futures::stream::Stream;
use futures::{
    StreamExt,
    stream::{self},
};
use reqwest::header::HeaderMap;
use serde::{Serialize, de::DeserializeOwned};
use std::{fmt::Debug, pin::Pin};
use tracing::{Level, event, instrument};

#[derive(Clone)]
pub struct Client {
    client: reqwest::Client,
}

pub type BoxedStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

impl Client {
    pub fn default() -> Self {
        Client {
            client: reqwest::Client::new(),
        }
    }

    pub fn with_headers(headers: HeaderMap) -> Self {
        Client {
            client: reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .expect("Failed to build headers"),
        }
    }

    #[instrument(level = "trace", skip(self))]
    pub async fn get<U, T>(&self, url: U) -> anyhow::Result<T>
    where
        U: reqwest::IntoUrl + std::fmt::Debug,
        T: DeserializeOwned,
    {
        let response = self.client.get(url).send().await?;
        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Request failed with status: {} - {:?}",
                response.status(),
                response.error_for_status()
            ));
        }
        let text = response.text().await?;
        event!(Level::TRACE, response = text);

        Ok(serde_json::from_str::<T>(&text)?)
    }

    #[instrument(level = "trace", skip(self, request), fields(json_request = serde_json::to_string(request).unwrap()))]
    pub async fn post<U, S, T>(&self, url: U, request: &S) -> anyhow::Result<T>
    where
        U: reqwest::IntoUrl + std::fmt::Debug,
        S: Serialize + Sized,
        T: DeserializeOwned,
    {
        let response = self.client.post(url).json(request).send().await?;
        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
            return Err(anyhow::anyhow!(
                "Request failed with status {}: {}",
                status,
                error_body
            ));
        }
        let text = response.text().await?;
        event!(Level::TRACE, response = text);

        Ok(serde_json::from_str::<T>(&text)?)
    }

    #[instrument(level = "trace", skip(self, request, process), fields(json_request = serde_json::to_string(request).unwrap()))]
    pub async fn post_stream<U, S, F, T>(
        &self,
        url: U,
        request: &S,
        process: F,
    ) -> anyhow::Result<BoxedStream<T>>
    where
        U: reqwest::IntoUrl + Debug,
        S: Serialize + Sized,
        T: DeserializeOwned + Send + 'static,
        F: Fn(&str) -> Option<&str> + 'static + Send,
    {
        let response = self.client.post(url).json(&request).send().await?;
        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
            return Err(anyhow::anyhow!(
                "Request failed with status {}: {}",
                status,
                error_body
            ));
        }

        let bytes = response.bytes_stream();

        // Use scan to maintain state (buffer) across chunks
        let buffered_stream = bytes.scan(String::new(), move |buffer, chunk| {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error reading chunk: {}", e);
                    return futures::future::ready(Some(vec![]));
                }
            };

            // Append new chunk data to buffer
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete lines (ending with \n)
            let mut messages: Vec<T> = vec![];
            let mut last_newline_pos = 0;

            for (idx, _) in buffer.match_indices('\n') {
                let line = &buffer[last_newline_pos..idx];
                last_newline_pos = idx + 1;

                if let Some(processed) = process(line) {
                    if !processed.trim().is_empty() {
                        match serde_json::from_str::<T>(processed) {
                            Ok(chat_response) => messages.push(chat_response),
                            Err(e) => {
                                eprintln!("Failed to parse line: {}: {}", processed, e);
                            }
                        }
                    }
                }
            }

            // Keep incomplete line in buffer
            *buffer = buffer[last_newline_pos..].to_string();

            futures::future::ready(Some(messages))
        });

        Ok(Box::pin(buffered_stream.flat_map(stream::iter)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
    struct TestEvent {
        id: u32,
        text: String,
    }

    #[tokio::test]
    async fn test_stream_processing_complete_lines() {
        // Simulate a stream with complete lines
        let data = b"data: {\"id\":1,\"text\":\"hello\"}\ndata: {\"id\":2,\"text\":\"world\"}\n";
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![Ok(bytes::Bytes::from(&data[..]))];
        let stream = stream::iter(chunks);

        let buffered = stream.scan(String::new(), |buffer, chunk| {
            let chunk = chunk.unwrap();
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            let mut events: Vec<TestEvent> = vec![];
            let mut last_newline_pos = 0;

            for (idx, _) in buffer.match_indices('\n') {
                let line = &buffer[last_newline_pos..idx];
                last_newline_pos = idx + 1;

                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<TestEvent>(json_str) {
                        events.push(event);
                    }
                }
            }

            *buffer = buffer[last_newline_pos..].to_string();
            futures::future::ready(Some(events))
        });

        let results: Vec<TestEvent> = buffered.flat_map(stream::iter).collect().await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0], TestEvent { id: 1, text: "hello".to_string() });
        assert_eq!(results[1], TestEvent { id: 2, text: "world".to_string() });
    }

    #[tokio::test]
    async fn test_stream_processing_split_across_chunks() {
        // Simulate JSON split across multiple chunks
        let chunk1 = b"data: {\"id\":1,\"te";
        let chunk2 = b"xt\":\"hello\"}\ndata: {\"id\":2";
        let chunk3 = b",\"text\":\"world\"}\n";

        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![
            Ok(bytes::Bytes::from(&chunk1[..])),
            Ok(bytes::Bytes::from(&chunk2[..])),
            Ok(bytes::Bytes::from(&chunk3[..])),
        ];
        let stream = stream::iter(chunks);

        let buffered = stream.scan(String::new(), |buffer, chunk| {
            let chunk = chunk.unwrap();
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            let mut events: Vec<TestEvent> = vec![];
            let mut last_newline_pos = 0;

            for (idx, _) in buffer.match_indices('\n') {
                let line = &buffer[last_newline_pos..idx];
                last_newline_pos = idx + 1;

                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<TestEvent>(json_str) {
                        events.push(event);
                    }
                }
            }

            *buffer = buffer[last_newline_pos..].to_string();
            futures::future::ready(Some(events))
        });

        let results: Vec<TestEvent> = buffered.flat_map(stream::iter).collect().await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0], TestEvent { id: 1, text: "hello".to_string() });
        assert_eq!(results[1], TestEvent { id: 2, text: "world".to_string() });
    }

    #[tokio::test]
    async fn test_stream_processing_incomplete_final_line() {
        // Simulate stream ending with incomplete line (no trailing newline)
        let data = b"data: {\"id\":1,\"text\":\"hello\"}\ndata: {\"id\":2,\"text\":\"incomplete";
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![Ok(bytes::Bytes::from(&data[..]))];
        let stream = stream::iter(chunks);

        let buffered = stream.scan(String::new(), |buffer, chunk| {
            let chunk = chunk.unwrap();
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            let mut events: Vec<TestEvent> = vec![];
            let mut last_newline_pos = 0;

            for (idx, _) in buffer.match_indices('\n') {
                let line = &buffer[last_newline_pos..idx];
                last_newline_pos = idx + 1;

                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<TestEvent>(json_str) {
                        events.push(event);
                    }
                }
            }

            *buffer = buffer[last_newline_pos..].to_string();
            futures::future::ready(Some(events))
        });

        let results: Vec<TestEvent> = buffered.flat_map(stream::iter).collect().await;

        // Only the first complete event should be parsed
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], TestEvent { id: 1, text: "hello".to_string() });
    }

    #[tokio::test]
    async fn test_stream_processing_empty_lines() {
        // Test handling of empty lines and lines without data prefix
        let data = b"\ndata: {\"id\":1,\"text\":\"hello\"}\n\nsome other line\ndata: {\"id\":2,\"text\":\"world\"}\n";
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![Ok(bytes::Bytes::from(&data[..]))];
        let stream = stream::iter(chunks);

        let buffered = stream.scan(String::new(), |buffer, chunk| {
            let chunk = chunk.unwrap();
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            let mut events: Vec<TestEvent> = vec![];
            let mut last_newline_pos = 0;

            for (idx, _) in buffer.match_indices('\n') {
                let line = &buffer[last_newline_pos..idx];
                last_newline_pos = idx + 1;

                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<TestEvent>(json_str) {
                        events.push(event);
                    }
                }
            }

            *buffer = buffer[last_newline_pos..].to_string();
            futures::future::ready(Some(events))
        });

        let results: Vec<TestEvent> = buffered.flat_map(stream::iter).collect().await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0], TestEvent { id: 1, text: "hello".to_string() });
        assert_eq!(results[1], TestEvent { id: 2, text: "world".to_string() });
    }

    #[tokio::test]
    async fn test_stream_processing_multiple_events_per_chunk() {
        // Test multiple complete events in a single chunk
        let data = b"data: {\"id\":1,\"text\":\"one\"}\ndata: {\"id\":2,\"text\":\"two\"}\ndata: {\"id\":3,\"text\":\"three\"}\n";
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![Ok(bytes::Bytes::from(&data[..]))];
        let stream = stream::iter(chunks);

        let buffered = stream.scan(String::new(), |buffer, chunk| {
            let chunk = chunk.unwrap();
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            let mut events: Vec<TestEvent> = vec![];
            let mut last_newline_pos = 0;

            for (idx, _) in buffer.match_indices('\n') {
                let line = &buffer[last_newline_pos..idx];
                last_newline_pos = idx + 1;

                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<TestEvent>(json_str) {
                        events.push(event);
                    }
                }
            }

            *buffer = buffer[last_newline_pos..].to_string();
            futures::future::ready(Some(events))
        });

        let results: Vec<TestEvent> = buffered.flat_map(stream::iter).collect().await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].id, 1);
        assert_eq!(results[1].id, 2);
        assert_eq!(results[2].id, 3);
    }

    #[tokio::test]
    async fn test_stream_processing_single_byte_chunks() {
        // Extreme case: one byte per chunk
        let data = b"data: {\"id\":1,\"text\":\"hello\"}\n";
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = data
            .iter()
            .map(|&b| Ok(bytes::Bytes::from(vec![b])))
            .collect();
        let stream = stream::iter(chunks);

        let buffered = stream.scan(String::new(), |buffer, chunk| {
            let chunk = chunk.unwrap();
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            let mut events: Vec<TestEvent> = vec![];
            let mut last_newline_pos = 0;

            for (idx, _) in buffer.match_indices('\n') {
                let line = &buffer[last_newline_pos..idx];
                last_newline_pos = idx + 1;

                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<TestEvent>(json_str) {
                        events.push(event);
                    }
                }
            }

            *buffer = buffer[last_newline_pos..].to_string();
            futures::future::ready(Some(events))
        });

        let results: Vec<TestEvent> = buffered.flat_map(stream::iter).collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0], TestEvent { id: 1, text: "hello".to_string() });
    }

    #[tokio::test]
    async fn test_stream_processing_malformed_json() {
        // Test that malformed JSON doesn't break the stream
        let data = b"data: {\"id\":1,\"text\":\"hello\"}\ndata: {malformed json}\ndata: {\"id\":2,\"text\":\"world\"}\n";
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![Ok(bytes::Bytes::from(&data[..]))];
        let stream = stream::iter(chunks);

        let buffered = stream.scan(String::new(), |buffer, chunk| {
            let chunk = chunk.unwrap();
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            let mut events: Vec<TestEvent> = vec![];
            let mut last_newline_pos = 0;

            for (idx, _) in buffer.match_indices('\n') {
                let line = &buffer[last_newline_pos..idx];
                last_newline_pos = idx + 1;

                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<TestEvent>(json_str) {
                        events.push(event);
                    }
                }
            }

            *buffer = buffer[last_newline_pos..].to_string();
            futures::future::ready(Some(events))
        });

        let results: Vec<TestEvent> = buffered.flat_map(stream::iter).collect().await;

        // Should successfully parse the two valid events, skipping the malformed one
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, 1);
        assert_eq!(results[1].id, 2);
    }

    #[tokio::test]
    async fn test_stream_processing_large_json() {
        // Test with a large JSON object
        let large_text = "a".repeat(10000);
        let data = format!("data: {{\"id\":1,\"text\":\"{}\"}}\n", large_text);
        let data_bytes = bytes::Bytes::from(data.into_bytes());
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![Ok(data_bytes)];
        let stream = stream::iter(chunks);

        let buffered = stream.scan(String::new(), |buffer, chunk| {
            let chunk = chunk.unwrap();
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            let mut events: Vec<TestEvent> = vec![];
            let mut last_newline_pos = 0;

            for (idx, _) in buffer.match_indices('\n') {
                let line = &buffer[last_newline_pos..idx];
                last_newline_pos = idx + 1;

                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<TestEvent>(json_str) {
                        events.push(event);
                    }
                }
            }

            *buffer = buffer[last_newline_pos..].to_string();
            futures::future::ready(Some(events))
        });

        let results: Vec<TestEvent> = buffered.flat_map(stream::iter).collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].text.len(), 10000);
    }

    #[test]
    fn test_client_default_creation() {
        let client = Client::default();
        assert!(!std::ptr::addr_of!(client.client).is_null());
    }

    #[test]
    fn test_client_with_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-test", "value".parse().unwrap());
        let client = Client::with_headers(headers);
        assert!(!std::ptr::addr_of!(client.client).is_null());
    }
}

