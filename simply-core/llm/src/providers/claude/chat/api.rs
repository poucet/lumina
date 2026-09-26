use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

impl TryFrom<crate::api::Role> for Role {
    type Error = anyhow::Error;

    fn try_from(value: crate::api::Role) -> Result<Self, Self::Error> {
        match value {
            crate::api::Role::User => Ok(Role::User),
            crate::api::Role::Assistant => Ok(Role::Assistant),
            crate::api::Role::System => Err(anyhow::anyhow!(
                "Claude does not support system messages directly in role field."
            )),
        }
    }
}

impl From<Role> for crate::api::Role {
    fn from(value: Role) -> Self {
        match value {
            Role::User => crate::api::Role::User,
            Role::Assistant => crate::api::Role::Assistant,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub(crate) enum Citation {
    // TODO
}

/// Claude image source - base64 or URL
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ImageSource {
    Base64 {
        media_type: String,
        data: String,
    },
    Url {
        url: String,
    },
}

/// Content block within a tool result (subset of Content types)
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ToolResultContentBlock {
    Text { text: String },
    Image { source: ImageSource },
}

/// Tool result content - can be a string or array of content blocks
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultContentBlock>),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Content {
    Text {
        text: String,

        #[serde(skip_serializing_if = "Option::is_none")]
        citations: Option<Vec<Citation>>,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
    },
}

impl TryFrom<&Content> for crate::api::ContentBlock {
    type Error = anyhow::Error;

    fn try_from(content: &Content) -> Result<Self, Self::Error> {
        match content {
            Content::Text { citations: _, text } => {
                Ok(crate::api::ContentBlock::Text { text: text.clone() })
            }
            Content::Image { source } => match source {
                ImageSource::Base64 { media_type, data } => {
                    Ok(crate::api::ContentBlock::Image {
                        data: data.clone(),
                        mime_type: media_type.clone(),
                    })
                }
                ImageSource::Url { url } => {
                    // URL images aren't directly supported in our generic format
                    // Return as text placeholder for now
                    Ok(crate::api::ContentBlock::Text {
                        text: format!("[Image URL: {}]", url),
                    })
                }
            },
            Content::ToolUse { id, name, input } => {
                Ok(crate::api::ContentBlock::ToolCall(crate::api::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: input.clone(),
                    extra: serde_json::Value::Null,
                }))
            }
            Content::ToolResult {
                tool_use_id,
                content,
            } => {
                let result_content = match content {
                    ToolResultContent::Text(text) => {
                        vec![crate::api::ToolResultContent::Text { text: text.clone() }]
                    }
                    ToolResultContent::Blocks(blocks) => {
                        blocks.iter().filter_map(|b| match b {
                            ToolResultContentBlock::Text { text } => {
                                Some(crate::api::ToolResultContent::Text { text: text.clone() })
                            }
                            ToolResultContentBlock::Image { source } => match source {
                                ImageSource::Base64 { media_type, data } => {
                                    Some(crate::api::ToolResultContent::Image {
                                        data: data.clone(),
                                        mime_type: media_type.clone(),
                                    })
                                }
                                ImageSource::Url { .. } => None, // Skip URL images
                            },
                        }).collect()
                    }
                };
                Ok(crate::api::ContentBlock::ToolResult(
                    crate::api::ToolResult {
                        tool_call_id: tool_use_id.clone(),
                        content: result_content,
                    },
                ))
            }
        }
    }
}

impl TryFrom<Vec<Content>> for crate::api::ChatPayload {
    type Error = anyhow::Error;

    fn try_from(contents: Vec<Content>) -> Result<Self, Self::Error> {
        let content_blocks: Vec<crate::api::ContentBlock> = contents
            .iter()
            .filter_map(|c| c.try_into().ok())
            .collect();
        Ok(crate::api::ChatPayload {
            content: content_blocks,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct InputMessage {
    pub(crate) content: Vec<Content>,

    pub(crate) role: Role,
}

impl From<&crate::api::ContentBlock> for Content {
    fn from(block: &crate::api::ContentBlock) -> Self {
        match block {
            crate::api::ContentBlock::Text { text } => Content::Text {
                citations: None,
                text: text.clone(),
            },
            crate::api::ContentBlock::ToolCall(call) => Content::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                // Claude requires `input` to be a JSON object. If the LLM
                // called a zero-arg tool we may have a null/string instead;
                // normalise to an empty object so the request validates.
                input: match &call.arguments {
                    serde_json::Value::Object(_) => call.arguments.clone(),
                    _ => serde_json::Value::Object(Default::default()),
                },
            },
            crate::api::ContentBlock::ToolResult(result) => {
                // Convert multimodal tool result content to Claude format
                let blocks: Vec<ToolResultContentBlock> = result.content.iter().filter_map(|c| {
                    match c {
                        crate::api::ToolResultContent::Text { text } => {
                            Some(ToolResultContentBlock::Text { text: text.clone() })
                        }
                        crate::api::ToolResultContent::Image { data, mime_type } => {
                            Some(ToolResultContentBlock::Image {
                                source: ImageSource::Base64 {
                                    media_type: mime_type.clone(),
                                    data: data.clone(),
                                },
                            })
                        }
                        crate::api::ToolResultContent::Audio { .. } => {
                            // Claude doesn't support audio in tool results yet
                            None
                        }
                    }
                }).collect();

                Content::ToolResult {
                    tool_use_id: result.tool_call_id.clone(),
                    content: if blocks.is_empty() {
                        ToolResultContent::Text(String::new())
                    } else if blocks.len() == 1 {
                        if let ToolResultContentBlock::Text { text } = &blocks[0] {
                            ToolResultContent::Text(text.clone())
                        } else {
                            ToolResultContent::Blocks(blocks)
                        }
                    } else {
                        ToolResultContent::Blocks(blocks)
                    },
                }
            }
            crate::api::ContentBlock::Image { data, mime_type } => Content::Image {
                source: ImageSource::Base64 {
                    media_type: mime_type.clone(),
                    data: data.clone(),
                },
            },
            crate::api::ContentBlock::Audio { .. } => {
                // Claude doesn't support audio in messages yet
                Content::Text {
                    citations: None,
                    text: "[Audio]".to_string(),
                }
            }
            crate::api::ContentBlock::EntityRef { .. } => {
                // EntityRef should be resolved before sending to LLM
                unreachable!("EntityRef should be resolved before sending to provider")
            }
        }
    }
}

impl From<&crate::ChatMessage> for InputMessage {
    fn from(msg: &crate::ChatMessage) -> InputMessage {
        InputMessage {
            role: msg.role.try_into().expect("Role not understood"),
            content: msg.payload.content.iter().map(|b| b.into()).collect(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum SystemPrompt {
    Text { text: String },
}

impl SystemPrompt {
    fn new(text: &str) -> Self {
        SystemPrompt::Text {
            text: text.to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Tool {
    pub(crate) name: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,

    pub(crate) input_schema: serde_json::Value,
}

impl From<&crate::api::ToolDefinition> for Tool {
    fn from(def: &crate::api::ToolDefinition) -> Self {
        let input_schema = serde_json::to_value(&def.input_schema)
            .expect("Failed to serialize tool schema");

        // Mirror the Gemini provider: log the exact shape we hand to the
        // Claude API so schema-validation 400s (`tools.N.custom...`) point
        // straight at the offending tool.
        if tracing::enabled!(tracing::Level::DEBUG) {
            let pretty = serde_json::to_string_pretty(&input_schema).unwrap_or_default();
            tracing::debug!(
                tool_name = %def.name,
                description = def.description.as_deref().unwrap_or(""),
                schema = %pretty,
                "claude tool definition",
            );
        }

        Tool {
            name: def.name.clone(),
            description: def.description.clone(),
            input_schema,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct MessagesRequest {
    pub(crate) model: String,

    pub(crate) messages: Vec<InputMessage>,

    pub(crate) max_tokens: u32,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream: Option<bool>,

    // Claude expects `system` as a list of content blocks, not a single
    // object — recent API versions reject the scalar form with
    // "system: Input should be a valid list".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system: Option<Vec<SystemPrompt>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<Vec<Tool>>,
}

impl MessagesRequest {
    pub(crate) fn from_chat_request(
        model_name: &str,
        request: &crate::ChatRequest,
        stream: bool,
    ) -> Self {
        // Separate system messages because they need to go into the system_messages field.
        let system_instruction = request
            .messages
            .iter()
            .filter(|m| m.role == crate::api::Role::System)
            .map(|m| m.get_text())
            .collect::<Vec<String>>()
            .join("\n");

        let messages = request
            .messages
            .iter()
            .filter(|m| m.role != crate::api::Role::System)
            .map(|msg: &crate::ChatMessage| msg.into())
            .collect::<Vec<InputMessage>>();

        let tools = request
            .tools
            .as_ref()
            .map(|tools| tools.iter().map(|t| t.into()).collect());

        MessagesRequest {
            model: model_name.to_string(),
            messages,
            // TODO: Don't hardcode
            max_tokens: 32000,
            stream: Some(stream),
            system: if system_instruction.is_empty() {
                None
            } else {
                Some(vec![SystemPrompt::new(&system_instruction)])
            },
            tools,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct MessagesResponse {
    pub(crate) id: String,

    pub(crate) role: Role,

    pub(crate) content: Vec<Content>,

    pub(crate) model: String,

    // Turn into an enum later.
    pub(crate) stop_reason: Option<String>,

    pub(crate) stop_sequence: Option<String>,

    #[serde(flatten)]
    pub(crate) extra: serde_json::Value,
}

impl From<MessagesResponse> for crate::ChatMessage {
    fn from(response: MessagesResponse) -> Self {
        let payload: crate::api::ChatPayload = response
            .content
            .try_into()
            .expect("Failed to convert Claude response");

        match response.role {
            Role::User => crate::ChatMessage::user(payload),
            Role::Assistant => crate::ChatMessage::assistant(payload),
        }
    }
}

impl From<MessagesResponse> for crate::ChatChunk {
    fn from(response: MessagesResponse) -> Self {
        let payload: crate::api::ChatPayload = response
            .content
            .try_into()
            .expect("Failed to convert Claude response");

        match response.role {
            Role::User => crate::ChatChunk::user(payload),
            Role::Assistant => crate::ChatChunk::assistant(payload),
        }
    }
}

// Streaming event types
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StreamEvent {
    MessageStart {
        message: MessageStartPayload,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDeltaPayload,
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    Error {
        error: ErrorPayload,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct MessageStartPayload {
    pub id: String,
    #[serde(rename = "type")]
    pub message_type: String,
    pub role: Role,
    pub content: Vec<Content>,
    pub model: String,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    Thinking {
        thinking: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // variant names are the Anthropic wire tags (text_delta, ...)
pub(crate) enum Delta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct MessageDeltaPayload {
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ErrorPayload {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}
