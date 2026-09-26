use std::vec;

use serde::{Deserialize, Serialize};

use crate::{ChatPayload, ChatRequest};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelDefinition {
    pub(crate) name: String,

    pub(crate) version: String,

    pub(crate) display_name: Option<String>,

    pub(crate) description: Option<String>,

    pub(crate) input_token_limit: Option<u32>,

    pub(crate) output_token_limit: Option<u32>,

    pub(crate) thinking: Option<bool>,

    pub(crate) supported_generation_methods: Option<Vec<String>>,
    //TODO:
    // temperature
    // maxTemperature
    // topP
    // topK
}

impl From<ModelDefinition> for crate::ModelDefinition {
    fn from(model: ModelDefinition) -> Self {
        let mut capabilities = Vec::new();

        if let Some(methods) = &model.supported_generation_methods {
            for method in methods {
                match method.as_str() {
                    "generateContent" => {
                        if !capabilities.contains(&crate::ModelCapability::Text) {
                            capabilities.push(crate::ModelCapability::Text);
                        }
                        // Note: Gemini models with generateContent support multimodal input (text + images)
                        // but this is not explicitly indicated in supportedGenerationMethods.
                        // Vision capability refers to multimodal input support, which is inherent
                        // to generateContent models in Gemini.
                        if !capabilities.contains(&crate::ModelCapability::Vision) {
                            capabilities.push(crate::ModelCapability::Vision);
                        }
                    }
                    "embedContent" => {
                        if !capabilities.contains(&crate::ModelCapability::Embedding) {
                            capabilities.push(crate::ModelCapability::Embedding);
                        }
                    }
                    _ => {}
                }
            }
        }

        // No fallback - if no supported methods found, capabilities will be empty
        // and the model will be filtered out by list_models

        match model.display_name {
            Some(display_name) => {
                crate::ModelDefinition::with_display_name(model.name, display_name, capabilities)
            }
            None => crate::ModelDefinition::new(model.name, capabilities),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListModelsResponse {
    pub(crate) models: Vec<ModelDefinition>,

    pub(crate) next_page_token: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Model,
}

impl TryFrom<crate::api::Role> for Role {
    type Error = anyhow::Error;

    fn try_from(value: crate::api::Role) -> Result<Self, Self::Error> {
        match value {
            crate::api::Role::User => Ok(Role::User),
            crate::api::Role::Assistant => Ok(Role::Model),
            crate::api::Role::System => Err(anyhow::anyhow!(
                "Gemini does not support system messages directly."
            )),
        }
    }
}

impl From<Role> for crate::api::Role {
    fn from(value: Role) -> Self {
        match value {
            Role::User => crate::api::Role::User,
            Role::Model => crate::api::Role::Assistant,
        }
    }
}

/// Gemini inline data for images/audio
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InlineData {
    pub(crate) mime_type: String,
    pub(crate) data: String, // base64-encoded
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct GeminiFunctionCall {
    pub(crate) name: String,
    pub(crate) args: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct GeminiFunctionResponse {
    pub(crate) name: String,
    pub(crate) response: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PartType {
    Text(String),
    InlineData(InlineData),
    FunctionCall(GeminiFunctionCall),
    FunctionResponse(GeminiFunctionResponse),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thought: Option<bool>,

    /// Required by Gemini for function calls in conversation history.
    /// Must be preserved from the original response and echoed back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thought_signature: Option<String>,

    #[serde(flatten)]
    pub(crate) data: PartType,

    #[serde(flatten)]
    pub(crate) extra: Option<serde_json::Value>,
}

impl Part {
    pub fn new_text(text: String) -> Self {
        Part {
            thought: None,
            thought_signature: None,
            data: PartType::Text(text),
            extra: None,
        }
    }
}

impl From<&Part> for Option<crate::api::ContentBlock> {
    fn from(part: &Part) -> Self {
        match &part.data {
            PartType::Text(t) => Some(crate::api::ContentBlock::Text { text: t.clone() }),
            PartType::InlineData(data) => {
                // Determine if it's image or audio based on mime type
                if data.mime_type.starts_with("image/") {
                    Some(crate::api::ContentBlock::Image {
                        data: data.data.clone(),
                        mime_type: data.mime_type.clone(),
                    })
                } else if data.mime_type.starts_with("audio/") {
                    Some(crate::api::ContentBlock::Audio {
                        data: data.data.clone(),
                        mime_type: data.mime_type.clone(),
                    })
                } else {
                    // Unknown type, skip
                    None
                }
            }
            PartType::FunctionCall(fc) => {
                // Preserve thought_signature in extra for round-tripping to Gemini
                let extra = if let Some(sig) = &part.thought_signature {
                    serde_json::json!({ "thought_signature": sig })
                } else {
                    serde_json::Value::Null
                };
                Some(crate::api::ContentBlock::ToolCall(
                    crate::api::ToolCall {
                        id: format!("gemini_{}", fc.name), // Gemini doesn't provide IDs
                        name: fc.name.clone(),
                        arguments: fc.args.clone(),
                        extra,
                    },
                ))
            }
            PartType::FunctionResponse(fr) => Some(crate::api::ContentBlock::ToolResult(
                crate::api::ToolResult {
                    tool_call_id: format!("gemini_{}", fr.name),
                    content: vec![crate::api::ToolResultContent::Text {
                        text: serde_json::to_string(&fr.response).unwrap_or_default(),
                    }],
                },
            )),
        }
    }
}

/// Convert a ContentBlock to one or more Gemini Parts.
/// Tool results with multimodal content become multiple parts:
/// - functionResponse with text
/// - inlineData for each image/audio
fn content_block_to_parts(block: &crate::api::ContentBlock) -> Vec<Part> {
    match block {
        crate::api::ContentBlock::Text { text } => vec![Part::new_text(text.clone())],
        crate::api::ContentBlock::ToolCall(call) => vec![Part {
            thought: None,
            thought_signature: call.extra.get("thought_signature").and_then(|v| v.as_str()).map(|s| s.to_string()),
            data: PartType::FunctionCall(GeminiFunctionCall {
                name: call.name.clone(),
                args: call.arguments.clone(),
            }),
            extra: None,
        }],
        crate::api::ContentBlock::ToolResult(result) => {
            let mut parts = Vec::new();

            // First, add the functionResponse with text content
            let text = result.get_text();
            // Gemini requires function_response.response to be an object (Struct),
            // not a plain string. Wrap non-object values in a result object.
            let response = match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) if v.is_object() => v,
                Ok(v) => serde_json::json!({ "result": v }),
                Err(_) => serde_json::json!({ "result": text }),
            };
            parts.push(Part {
                thought: None,
                thought_signature: None,
                data: PartType::FunctionResponse(GeminiFunctionResponse {
                    name: result.tool_call_id.clone(),
                    response,
                }),
                extra: None,
            });

            // Then add any images/audio as separate inlineData parts
            for content in &result.content {
                match content {
                    crate::api::ToolResultContent::Image { data, mime_type } => {
                        parts.push(Part {
                            thought: None,
                            thought_signature: None,
                            data: PartType::InlineData(InlineData {
                                mime_type: mime_type.clone(),
                                data: data.clone(),
                            }),
                            extra: None,
                        });
                    }
                    crate::api::ToolResultContent::Audio { data, mime_type } => {
                        parts.push(Part {
                            thought: None,
                            thought_signature: None,
                            data: PartType::InlineData(InlineData {
                                mime_type: mime_type.clone(),
                                data: data.clone(),
                            }),
                            extra: None,
                        });
                    }
                    crate::api::ToolResultContent::Text { .. } => {
                        // Already included in functionResponse
                    }
                }
            }

            parts
        }
        crate::api::ContentBlock::Image { data, mime_type } => vec![Part {
            thought: None,
            thought_signature: None,
            data: PartType::InlineData(InlineData {
                mime_type: mime_type.clone(),
                data: data.clone(),
            }),
            extra: None,
        }],
        crate::api::ContentBlock::Audio { data, mime_type } => vec![Part {
            thought: None,
            thought_signature: None,
            data: PartType::InlineData(InlineData {
                mime_type: mime_type.clone(),
                data: data.clone(),
            }),
            extra: None,
        }],
        crate::api::ContentBlock::EntityRef { id } => {
            // EntityRef should be resolved before sending to LLM
            panic!("EntityRef (id={}) was not resolved before sending to provider. Ensure EntityResolver is configured.", id)
        }
    }
}

impl From<&crate::api::ContentBlock> for Part {
    fn from(block: &crate::api::ContentBlock) -> Self {
        // For backwards compatibility, return the first part
        // Use content_block_to_parts() directly when multiple parts are needed
        content_block_to_parts(block).into_iter().next().unwrap()
    }
}

// Gemini representation of messages.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Content {
    pub(crate) role: Role,
    // Gemini occasionally returns candidates with a bare `{role: "model"}`
    // (empty turn, finishReason=STOP). Treat missing `parts` as an empty
    // content block rather than a parse error.
    #[serde(default)]
    pub(crate) parts: Vec<Part>,
}

impl From<&Content> for crate::ChatMessage {
    fn from(content: &Content) -> Self {
        let blocks: Vec<crate::api::ContentBlock> = content
            .parts
            .iter()
            .filter_map(Option::<crate::api::ContentBlock>::from)
            .collect();

        crate::ChatMessage::new(content.role.into(), ChatPayload::new(blocks))
    }
}

impl From<Content> for crate::ChatMessage {
    fn from(content: Content) -> Self {
        Self::from(&content)
    }
}

impl From<&Content> for crate::ChatChunk {
    fn from(content: &Content) -> Self {
        let blocks: Vec<crate::api::ContentBlock> = content
            .parts
            .iter()
            .filter_map(Option::<crate::api::ContentBlock>::from)
            .collect();

        crate::ChatChunk::new(content.role.into(), ChatPayload::new(blocks))
    }
}

impl From<Content> for crate::ChatChunk {
    fn from(content: Content) -> Self {
        Self::from(&content)
    }
}

trait FromWithRole<T> {
   fn from_with_role(t: T, role: crate::api::Role) -> Self;
}

impl FromWithRole<&crate::ChatPayload> for Content {
    fn from_with_role(payload: &crate::ChatPayload, role: crate::api::Role) -> Self {
        Content {
            role: role.try_into().expect("Invalid role"),
            // Use flat_map to handle tool results with multimodal content (images/audio)
            parts: payload.content.iter().flat_map(content_block_to_parts).collect(),
        }
    }
}

impl From<&crate::ChatMessage> for Content {
    fn from(msg: &crate::ChatMessage) -> Self {
        Content::from_with_role(&msg.payload, msg.role)
    }
}

impl From<crate::ChatMessage> for Content {
    // TODO: Don't rely on the const-ref version to avoid cloning, but do this in a way that keeps code-maintenance easy.
    fn from(msg: crate::ChatMessage) -> Self {
        Content::from_with_role(&msg.payload, msg.role)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct GeminiFunctionDeclaration {
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    pub(crate) parameters: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiTool {
    pub(crate) function_declarations: Vec<GeminiFunctionDeclaration>,
}

/// Keys that Gemini API does not support in JSON Schema
const UNSUPPORTED_SCHEMA_KEYS: &[&str] = &[
    "$schema",
    "$id",
    "$anchor",
    "$dynamicRef",
    "$dynamicAnchor",
    "$vocabulary",
    "$comment",
    "additionalProperties",
    "default",
    "examples",
    "format",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "pattern",
    "uniqueItems",
    "minItems",
    "maxItems",
    // Note: $ref and $defs are handled specially - $ref is resolved, then both are removed
    // Note: anyOf/oneOf/allOf are handled specially - first option is used
];

/// Sanitize a JSON Schema for Gemini API compatibility.
///
/// Gemini rejects schemas with advanced JSON Schema features like $schema, $ref, $defs, etc.
/// This function recursively removes unsupported keys and resolves $ref references by inlining
/// the referenced definitions.
fn sanitize_schema_for_gemini(schema: serde_json::Value) -> serde_json::Value {
    // Extract $defs or definitions from root for reference resolution
    // Note: $defs is JSON Schema draft 2019-09+, "definitions" is draft 4-7
    let defs = schema
        .as_object()
        .and_then(|obj| obj.get("$defs").or_else(|| obj.get("definitions")))
        .and_then(|d| d.as_object())
        .cloned();

    sanitize_schema_recursive(schema, defs.as_ref())
}

fn sanitize_schema_recursive(
    schema: serde_json::Value,
    defs: Option<&serde_json::Map<String, serde_json::Value>>,
) -> serde_json::Value {
    let obj = match schema {
        serde_json::Value::Object(obj) => obj,
        other => return other,
    };

    // Handle $ref - resolve before removing
    if let Some(ref_value) = obj.get("$ref") {
        if let Some(ref_str) = ref_value.as_str() {
            // Handle both #/$defs/ (draft 2019-09+) and #/definitions/ (draft 4-7)
            let ref_name = ref_str
                .strip_prefix("#/$defs/")
                .or_else(|| ref_str.strip_prefix("#/definitions/"));
            if let Some(ref_name) = ref_name {
                if let Some(defs_map) = defs {
                    if let Some(definition) = defs_map.get(ref_name) {
                        // Inline and recursively sanitize the definition
                        return sanitize_schema_recursive(definition.clone(), defs);
                    }
                }
            }
        }
        // Can't resolve $ref - return empty object
        return serde_json::json!({});
    }

    // Handle anyOf/oneOf/allOf - take the first option and merge with parent
    // This is a simplification but handles common cases like optional types
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = obj.get(key) {
            if let Some(arr) = variants.as_array() {
                // Find first non-null variant
                for variant in arr {
                    if let Some(variant_obj) = variant.as_object() {
                        // Skip null types (used for optional fields)
                        if variant_obj.get("type") == Some(&serde_json::json!("null")) {
                            continue;
                        }
                        // Use this variant, but merge any other keys from parent
                        let mut merged = variant_obj.clone();
                        for (k, v) in &obj {
                            if k != key && !merged.contains_key(k) {
                                merged.insert(k.clone(), v.clone());
                            }
                        }
                        return sanitize_schema_recursive(serde_json::Value::Object(merged), defs);
                    }
                }
            }
        }
    }

    // Build new object without unsupported keys.
    //
    // `const: X` is JSON Schema draft 2019-09+; Gemini rejects it but
    // accepts the equivalent `enum: [X]`. Rewrite in place so tools
    // that pin a literal string/number still express the restriction.
    let mut obj = obj;
    if let Some(const_val) = obj.remove("const") {
        obj.entry("enum".to_string())
            .or_insert_with(|| serde_json::Value::Array(vec![const_val]));
    }

    let mut result = serde_json::Map::new();
    for (key, value) in obj {
        // Skip unsupported keys
        // Note: "definitions" is JSON Schema draft 4-7, "$defs" is draft 2019-09+
        // Note: anyOf/oneOf/allOf are handled above
        if UNSUPPORTED_SCHEMA_KEYS.contains(&key.as_str())
            || key == "$defs"
            || key == "definitions"
            || key == "$ref"
            || key == "anyOf"
            || key == "oneOf"
            || key == "allOf"
        {
            continue;
        }

        // Gemini expects UPPERCASE type names (STRING, INTEGER, etc.)
        // Also handle array types like ["string", "null"] by picking first non-null
        let sanitized_value = if key == "type" {
            match &value {
                serde_json::Value::String(t) => {
                    serde_json::Value::String(t.to_uppercase())
                }
                serde_json::Value::Array(arr) => {
                    // Find first non-null type
                    let first_type = arr
                        .iter()
                        .filter_map(|v| v.as_str())
                        .find(|t| *t != "null")
                        .unwrap_or("string");
                    serde_json::Value::String(first_type.to_uppercase())
                }
                _ => value,
            }
        } else {
            match value {
                serde_json::Value::Object(_) => sanitize_schema_recursive(value, defs),
                serde_json::Value::Array(arr) => serde_json::Value::Array(
                    arr.into_iter()
                        .map(|item| {
                            if item.is_object() {
                                sanitize_schema_recursive(item, defs)
                            } else {
                                item
                            }
                        })
                        .collect(),
                ),
                other => other,
            }
        };
        result.insert(key, sanitized_value);
    }

    serde_json::Value::Object(result)
}

impl From<&Vec<crate::api::ToolDefinition>> for GeminiTool {
    fn from(tools: &Vec<crate::api::ToolDefinition>) -> Self {
        GeminiTool {
            function_declarations: tools
                .iter()
                .map(|t| {
                    let raw_schema = serde_json::to_value(&t.input_schema)
                        .expect("Failed to serialize tool schema");

                    // Log the raw schema before transformation
                    if let Ok(pretty) = serde_json::to_string_pretty(&raw_schema) {
                        tracing::debug!(
                            tool_name = %t.name,
                            schema = %pretty,
                            "MCP tool schema BEFORE Gemini sanitization"
                        );
                    }

                    let sanitized_schema = sanitize_schema_for_gemini(raw_schema);

                    // Log the sanitized schema after transformation
                    if let Ok(pretty) = serde_json::to_string_pretty(&sanitized_schema) {
                        tracing::debug!(
                            tool_name = %t.name,
                            schema = %pretty,
                            "MCP tool schema AFTER Gemini sanitization"
                        );
                    }

                    GeminiFunctionDeclaration {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: sanitized_schema,
                    }
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GenerateContentRequest {
    pub(crate) contents: Vec<Content>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<Vec<GeminiTool>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system_instruction: Option<Content>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) generation_config: Option<serde_json::Value>,
}

impl GenerateContentRequest {
    pub fn new(contents: Vec<Content>, system_instruction: Option<Content>) -> Self {
        GenerateContentRequest {
            contents,
            tools: None,
            system_instruction,
            generation_config: None,
        }
    }
}

impl From<&ChatRequest> for GenerateContentRequest {
    fn from(request: &ChatRequest) -> Self {
        // Separate system messages because they need to go into the system_messages field.
        let system_instruction = Content {
            parts: request
                .messages
                .iter()
                .filter(|m| m.role == crate::api::Role::System)
                .flat_map(|m| m.payload.content.iter().map(|p| p.into()))
                .collect::<Vec<Part>>(),
            role: Role::User, // Role is ignored for system messages
        };
        let contents = request
            .messages
            .iter()
            .filter(|m| m.role != crate::api::Role::System)
            .map(|msg| msg.into())
            .collect::<Vec<Content>>();

        let tools = request
            .tools
            .as_ref()
            .map(|tools| vec![GeminiTool::from(tools)]);

        let mut req = GenerateContentRequest::new(contents, Some(system_instruction));
        req.tools = tools;
        req
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Candidate {
    pub(crate) content: Content,

    #[serde(flatten)]
    pub(crate) extra: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct GenerateContentResponse {
    pub(crate) candidates: Vec<Candidate>,

    #[serde(flatten)]
    pub(crate) extra: Option<serde_json::Value>,
}

impl From<GenerateContentResponse> for crate::ChatMessage {
    fn from(response: GenerateContentResponse) -> Self {
        // TODO: Move out of candidates instead of cloning.
        response.candidates.first().unwrap().content.clone().into()
    }
}

impl From<GenerateContentResponse> for crate::ChatChunk {
    fn from(response: GenerateContentResponse) -> Self {
        // TODO: Move out of candidates instead of cloning.
        response.candidates.first().unwrap().content.clone().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_content_serialization() {
        let content = Content {
            role: Role::User,
            parts: vec![Part {
                thought: Some(true),
                thought_signature: None,
                data: PartType::Text("Hello, world!".to_string()),
                extra: Some(serde_json::json!({"foo": "bar"})),
            }],
        };
        let json = serde_json::to_string(&content).unwrap();
        assert_eq!(
            json,
            r#"{"role":"user","parts":[{"thought":true,"text":"Hello, world!","foo":"bar"}]}"#
        );
    }
}
