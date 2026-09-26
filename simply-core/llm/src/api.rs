use std::str::FromStr;

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Eq, Serialize, Default, schemars::JsonSchema)]
#[cfg_attr(feature = "ts", derive(TS))]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    #[default]
    Assistant,
    System,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
       match s {
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            "system" => Ok(Role::System),
            _ => Err(format!("{s} is not a valid role")),
        }
      }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: schemars::Schema,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Provider-specific metadata (e.g., Gemini's thought_signature)
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub extra: serde_json::Value,
}

/// Content within a tool result - can be text, images, audio, etc.
/// This is a subset of ContentBlock without recursive tool calls.
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts", derive(TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    Text { text: String },
    Image { data: String, mime_type: String },
    Audio { data: String, mime_type: String },
}

impl ToolResultContent {
    pub fn text(text: impl Into<String>) -> Self {
        ToolResultContent::Text { text: text.into() }
    }

    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        ToolResultContent::Image {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }

    pub fn audio(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        ToolResultContent::Audio {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }
}


#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: Vec<ToolResultContent>,
}

impl ToolResult {
    /// Get text content from this tool result, concatenated
    pub fn get_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                ToolResultContent::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "ts", derive(TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { data: String, mime_type: String },
    Audio { data: String, mime_type: String },
    /// Reference to an entity (by id). Stored as-is; an `EntityResolver`
    /// expands it to formatted text (title, kind, body / tabs) before the
    /// message reaches an LLM provider.
    EntityRef { id: String },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

impl ContentBlock {
    /// Get the mime_type for media content blocks
    pub fn mime_type(&self) -> Option<&str> {
        match self {
            ContentBlock::Image { mime_type, .. } => Some(mime_type),
            ContentBlock::Audio { mime_type, .. } => Some(mime_type),
            _ => None,
        }
    }

    /// Extract the EntityRef id, if any.
    pub fn entity_ref(&self) -> Option<&str> {
        match self {
            ContentBlock::EntityRef { id } => Some(id.as_str()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ChatPayload {
    pub content: Vec<ContentBlock>,
}

impl From<&String> for ChatPayload {
    fn from(text: &String) -> Self {
        ChatPayload::text(text)
    }
}

impl From<String> for ChatPayload {
    fn from(text: String) -> Self {
        ChatPayload::text(text)
    }
}

impl From<&str> for ChatPayload {
    fn from(text: &str) -> Self {
        ChatPayload::text(text)
    }
}

impl From<ChatMessage> for ChatPayload {
    fn from(msg: ChatMessage) -> Self {
        msg.payload
    }
}

impl ChatPayload {
    pub fn new(content: Vec<ContentBlock>) -> Self {
        ChatPayload { content }
    }

    /// Get all entity IDs referenced in this payload
    pub fn get_entity_refs(&self) -> Vec<&str> {
        self.content
            .iter()
            .filter_map(|block| block.entity_ref())
            .collect()
    }

    pub fn text(text: impl Into<String>) -> Self {
        ChatPayload {
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        ChatPayload {
            content: vec![ContentBlock::Image {
                data: data.into(),
                mime_type: mime_type.into(),
            }],
        }
    }

    pub fn audio(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        ChatPayload {
            content: vec![ContentBlock::Audio {
                data: data.into(),
                mime_type: mime_type.into(),
            }],
        }
    }

    pub fn with_tool_calls(text: String, tool_calls: Vec<ToolCall>) -> Self {
        let mut content = vec![ContentBlock::Text { text }];
        content.extend(tool_calls.into_iter().map(ContentBlock::ToolCall));
        ChatPayload { content }
    }

    /// Create a payload with a single tool call
    pub fn tool_call(tool_call: ToolCall) -> Self {
        ChatPayload {
            content: vec![ContentBlock::ToolCall(tool_call)],
        }
    }

    /// Create a tool result with multimodal content
    pub fn tool_result(tool_call_id: String, result_content: Vec<ToolResultContent>) -> Self {
        ChatPayload {
            content: vec![ContentBlock::ToolResult(ToolResult {
                tool_call_id,
                content: result_content,
            })],
        }
    }

    /// Create a simple text-only tool result (convenience method)
    pub fn tool_result_text(tool_call_id: String, text: String) -> Self {
        Self::tool_result(tool_call_id, vec![ToolResultContent::Text { text }])
    }

    pub fn get_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Get images from this payload
    pub fn get_images(&self) -> Vec<(&str, &str)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Image { data, mime_type } => Some((data.as_str(), mime_type.as_str())),
                _ => None,
            })
            .collect()
    }

    /// Get audio from this payload
    pub fn get_audio(&self) -> Vec<(&str, &str)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Audio { data, mime_type } => Some((data.as_str(), mime_type.as_str())),
                _ => None,
            })
            .collect()
    }

    pub fn get_tool_calls(&self) -> Vec<&ToolCall> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect()
    }

    pub fn get_tool_results(&self) -> Vec<&ToolResult> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ChatMessage {
    #[serde(default)]
    pub role: Role,
    #[serde(flatten)]
    pub payload: ChatPayload,
}

impl ChatMessage {
    pub fn new(role: Role, payload: ChatPayload) -> Self {
        Self { role, payload }
    }

    pub fn user(payload: ChatPayload) -> Self {
        Self::new(Role::User, payload)
    }
    
    pub fn assistant(payload: ChatPayload) -> Self {
        Self::new(Role::Assistant, payload)
    }

    pub fn system(payload: ChatPayload) -> Self {
        Self::new(Role::System, payload)
    }

    pub fn get_text(&self) -> String {
        self.payload.get_text()
    }
    
    pub fn get_entity_refs(&self) -> Vec<&str> {
        self.payload.get_entity_refs()
    }

    pub fn get_tool_calls(&self) -> Vec<&ToolCall> {
        self.payload.get_tool_calls()
    }

    pub fn get_tool_results(&self) -> Vec<&ToolResult> {
        self.payload.get_tool_results()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatChunk {
    pub role: Role,
    #[serde(flatten)]
    pub payload: ChatPayload,
}

impl ChatChunk {
    pub fn new(role: Role, payload: ChatPayload) -> Self {
        Self { role, payload }
    }

    pub fn user(payload: ChatPayload) -> Self {
        Self::new(Role::User, payload)
    }
    
    pub fn assistant(payload: ChatPayload) -> Self {
        Self::new(Role::Assistant, payload)
    }

    pub fn system(payload: ChatPayload) -> Self {
        Self::new(Role::System, payload)
    }

    pub fn get_text(&self) -> String {
        self.payload.get_text()
    }
}

impl From<ChatChunk> for ChatMessage {
    fn from(chunk: ChatChunk) -> Self {
        ChatMessage {
            role: chunk.role,
            payload: chunk.payload,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatRequest {
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) tools: Option<Vec<ToolDefinition>>,
}

impl ChatRequest {
    /// Create a new chat request from an iterator of message references
    ///
    /// This accepts any iterator that yields `&ChatMessage`, avoiding unnecessary clones:
    /// - `&[ChatMessage]` - slice
    /// - `Vec<&ChatMessage>` - vector of references
    /// - `context.iter()` - iterator from ConversationContext
    ///
    /// Messages are cloned only once when constructing the request.
    pub fn new<'a>(messages: impl IntoIterator<Item = &'a ChatMessage>) -> Self {
        ChatRequest {
            messages: messages.into_iter().cloned().collect(),
            tools: None,
        }
    }

    /// Create a chat request with tool definitions
    pub fn with_tools<'a>(
        messages: impl IntoIterator<Item = &'a ChatMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Self {
        ChatRequest {
            messages: messages.into_iter().cloned().collect(),
            tools: Some(tools),
        }
    }

    /// Get a reference to the messages
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Get a mutable reference to the messages (for external resolution)
    pub fn messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        &mut self.messages
    }


    /// Get all entity IDs referenced in this request
    pub fn get_entity_refs(&self) -> Vec<&str> {
        self.messages
            .iter()
            .flat_map(|msg| msg.get_entity_refs())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Deserialize, Serialize, JsonSchema)]
    struct TestInput {
        query: String,
    }

    #[test]
    fn test_chat_payload_text() {
        let payload = ChatPayload::text("Hello, world!");
        assert_eq!(payload.get_text(), "Hello, world!");
        assert_eq!(payload.content.len(), 1);
        assert!(matches!(
            payload.content[0],
            ContentBlock::Text { .. }
        ));
    }

    #[test]
    fn test_chat_payload_with_tool_calls() {
        let tool_call = ToolCall {
            id: "call_123".to_string(),
            name: "search".to_string(),
            arguments: serde_json::json!({"query": "test"}),
            extra: serde_json::Value::Null,
        };

        let payload = ChatPayload::with_tool_calls(
            "Let me search for that.".to_string(),
            vec![tool_call.clone()],
        );

        assert_eq!(payload.get_text(), "Let me search for that.");
        assert_eq!(payload.content.len(), 2);

        let tool_calls = payload.get_tool_calls();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "search");
    }

    #[test]
    fn test_chat_payload_tool_result() {
        let payload = ChatPayload::tool_result_text(
            "call_123".to_string(),
            "Search results: ...".to_string(),
        );

        let results = payload.get_tool_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_call_id, "call_123");
        assert_eq!(results[0].get_text(), "Search results: ...");
    }

    #[test]
    fn test_chat_message_constructors() {
        let payload = ChatPayload::text("Test");

        let user_msg = ChatMessage::user(payload.clone());
        assert_eq!(user_msg.role, Role::User);
        assert_eq!(user_msg.get_text(), "Test");

        let assistant_msg = ChatMessage::assistant(payload.clone());
        assert_eq!(assistant_msg.role, Role::Assistant);

        let system_msg = ChatMessage::system(payload);
        assert_eq!(system_msg.role, Role::System);
    }

    #[test]
    fn test_chat_message_get_tool_calls() {
        let tool_call = ToolCall {
            id: "call_456".to_string(),
            name: "calculator".to_string(),
            arguments: serde_json::json!({"a": 5, "b": 3}),
            extra: serde_json::Value::Null,
        };

        let payload = ChatPayload::with_tool_calls(
            "Calculating...".to_string(),
            vec![tool_call],
        );
        let msg = ChatMessage::assistant(payload);

        let calls = msg.get_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_456");
        assert_eq!(calls[0].name, "calculator");
    }

    #[test]
    fn test_chat_message_get_tool_results() {
        let payload = ChatPayload::tool_result_text("call_789".to_string(), "42".to_string());
        let msg = ChatMessage::user(payload);

        let results = msg.get_tool_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_call_id, "call_789");
        assert_eq!(results[0].get_text(), "42");
    }

    #[test]
    fn test_chat_request_new() {
        let messages = vec![ChatMessage::user(ChatPayload::text("Hello"))];
        let request = ChatRequest::new(&messages);

        assert_eq!(request.messages.len(), 1);
        assert!(request.tools.is_none());
    }

    #[test]
    fn test_chat_request_with_tools() {
        let messages = vec![ChatMessage::user(ChatPayload::text("Search for Rust"))];

        let schema = schemars::schema_for!(TestInput);
        let tool = ToolDefinition {
            name: "search".to_string(),
            description: Some("Searches the web".to_string()),
            input_schema: schema,
        };

        let request = ChatRequest::with_tools(&messages, vec![tool]);

        assert_eq!(request.messages.len(), 1);
        assert!(request.tools.is_some());
        assert_eq!(request.tools.as_ref().unwrap().len(), 1);
        assert_eq!(request.tools.as_ref().unwrap()[0].name, "search");
    }

    #[test]
    fn test_content_block_serialization() {
        let text_block = ContentBlock::Text {
            text: "Hello".to_string(),
        };
        let json = serde_json::to_string(&text_block).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        assert!(json.contains("\"text\":\"Hello\""));
    }

    #[test]
    fn test_tool_call_serialization() {
        let tool_call = ToolCall {
            id: "call_abc".to_string(),
            name: "test_tool".to_string(),
            arguments: serde_json::json!({"key": "value"}),
            extra: serde_json::Value::Null,
        };

        let block = ContentBlock::ToolCall(tool_call);
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"tool_call\""));
        assert!(json.contains("\"name\":\"test_tool\""));
    }

    #[test]
    fn test_tool_result_serialization() {
        let tool_result = ToolResult {
            tool_call_id: "call_xyz".to_string(),
            content: vec![ToolResultContent::Text {
                text: "Result data".to_string(),
            }],
        };

        let block = ContentBlock::ToolResult(tool_result);
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"tool_result\""));
        assert!(json.contains("\"tool_call_id\":\"call_xyz\""));
    }

    #[test]
    fn test_chat_payload_multiple_text_blocks() {
        let payload = ChatPayload::new(vec![
            ContentBlock::Text {
                text: "First ".to_string(),
            },
            ContentBlock::Text {
                text: "Second".to_string(),
            },
        ]);

        assert_eq!(payload.get_text(), "First Second");
    }

    #[test]
    fn test_chat_payload_mixed_content() {
        let payload = ChatPayload::new(vec![
            ContentBlock::Text {
                text: "Text before tool".to_string(),
            },
            ContentBlock::ToolCall(ToolCall {
                id: "call_1".to_string(),
                name: "tool1".to_string(),
                arguments: serde_json::json!({}),
                extra: serde_json::Value::Null,
            }),
            ContentBlock::Text {
                text: "Text after tool".to_string(),
            },
        ]);

        assert_eq!(payload.get_text(), "Text before toolText after tool");
        assert_eq!(payload.get_tool_calls().len(), 1);
        assert_eq!(payload.content.len(), 3);
    }
}
