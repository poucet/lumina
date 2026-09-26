//! Daemon-specific RPC connection — implements `RpcConnection` over WebSocket.


use async_trait::async_trait;
use serde_json::Value;
use simply_rpc::ws_client::{ConnectionState, WsConnection};
use simply_rpc::RpcConnection;
use tokio::sync::{mpsc, watch};

/// Daemon RPC connection over WebSocket.
///
/// Implements `RpcConnection` for use by generated `RemoteXxxApi` structs.
pub struct DaemonRpcConnection {
    conn: WsConnection,
    base_url: String,
    http: reqwest::Client,
}

impl DaemonRpcConnection {
    pub async fn connect(addr: &str, name: &str, daemon_secret: &str, user_id: Option<&str>) -> anyhow::Result<Self> {
        let mut ws_headers = vec![
            ("Authorization".to_string(), format!("Bearer {daemon_secret}")),
        ];
        if let Some(uid) = user_id {
            ws_headers.push(("X-User-Id".to_string(), uid.to_string()));
        }
        let conn = WsConnection::connect_with_headers(addr, ws_headers).await?;
        let _ = conn.rpc_call("client.identify", serde_json::json!({ "name": name })).await;

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {daemon_secret}"))?,
        );
        if let Some(uid) = user_id {
            headers.insert(
                reqwest::header::HeaderName::from_static("x-user-id"),
                reqwest::header::HeaderValue::from_str(uid)?,
            );
        }

        Ok(Self {
            conn,
            base_url: format!("http://{addr}"),
            http: reqwest::Client::builder().default_headers(headers).build()?,
        })
    }

    /// Access the underlying WsConnection (for reverse call setup, etc.)
    pub fn ws(&self) -> &WsConnection {
        &self.conn
    }

    pub fn connection_state(&self) -> ConnectionState {
        self.conn.connection_state()
    }

    pub fn watch_state(&self) -> watch::Receiver<ConnectionState> {
        self.conn.watch_state()
    }
}

#[async_trait]
impl RpcConnection for DaemonRpcConnection {
    async fn rpc_call(&self, method: &str, params: Value, ctx: &simply_rpc::RequestContext) -> anyhow::Result<Value> {
        // Inject ctx into params as __ctx field for WS transport
        let params = match params {
            Value::Object(mut map) => {
                map.insert("__ctx".to_string(), serde_json::to_value(ctx).unwrap_or_default());
                Value::Object(map)
            }
            Value::Null => serde_json::json!({ "__ctx": ctx }),
            other => other,
        };
        self.conn.rpc_call(method, params).await
    }

    async fn rest_call(
        &self,
        http_method: simply_rpc::HttpMethod,
        path: &str,
        body: Value,
        ctx: &simply_rpc::RequestContext,
    ) -> anyhow::Result<Value> {
        let url = format!("{}/api{}", self.base_url, path);
        let max_retries = 2;

        for attempt in 0..=max_retries {
            let mut req = match http_method {
                simply_rpc::HttpMethod::Get => self.http.get(&url),
                simply_rpc::HttpMethod::Post => self.http.post(&url).json(&body),
                simply_rpc::HttpMethod::Put => self.http.put(&url).json(&body),
                simply_rpc::HttpMethod::Delete => self.http.delete(&url),
            };

            // Send the full RequestContext as a JSON header
            if let Ok(ctx_json) = serde_json::to_string(ctx) {
                req = req.header("X-Request-Context", ctx_json);
            }

            let resp = req.send().await?;

            let status = resp.status();
            if status.is_success() {
                let text = resp.text().await?;
                if text.is_empty() {
                    return Ok(Value::Null);
                } else {
                    return Ok(serde_json::from_str(&text)?);
                }
            }

            let msg = resp.text().await.unwrap_or_else(|_| status.to_string());

            // Retry on 500+ server errors
            if status.is_server_error() && attempt < max_retries {
                tracing::warn!(
                    attempt = attempt + 1,
                    status = %status,
                    path,
                    "REST server error, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt as u64 + 1))).await;
                continue;
            }

            return Err(anyhow::anyhow!("REST error {}: {}", status, msg));
        }

        unreachable!()
    }

    async fn register_stream(
        &self,
        method: &str,
    ) -> anyhow::Result<(mpsc::Sender<Value>, mpsc::Receiver<Value>)> {
        let event_method = format!("{method}.event");
        let input_method = format!("{method}.input");

        // Subscribe to events from the server
        let mut raw_rx = self.conn.register_sink(&event_method).await;

        // Get a write channel for sending notifications
        let write_tx = self.conn.write_tx().await
            .ok_or_else(|| anyhow::anyhow!("disconnected"))?;

        // Input: caller sends Value → we send as WS notification
        let (input_tx, mut input_rx) = mpsc::channel::<Value>(64);
        tokio::spawn(async move {
            while let Some(value) = input_rx.recv().await {
                let notif = simply_rpc::protocol::WsNotification {
                    method: input_method.clone(),
                    params: value,
                };
                let text = serde_json::to_string(&notif).unwrap_or_default();
                if write_tx.send(text).await.is_err() { break; }
            }
        });

        // Output: raw events from the server
        let (output_tx, output_rx) = mpsc::channel::<Value>(64);
        tokio::spawn(async move {
            while let Some(value) = raw_rx.recv().await {
                if output_tx.send(value).await.is_err() { break; }
            }
        });

        Ok((input_tx, output_rx))
    }
}
