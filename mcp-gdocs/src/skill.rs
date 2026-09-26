//! Google Docs skill — provides import tools to the daemon.
//!
//! Uses GoogleDocsClient for Google API access.
//! Uses `Arc<dyn Daemon>` for daemon API access (document creation, asset storage).
//! Auth tokens come from the caller's `RequestContext` — injected by the
//! daemon, transparent to the LLM.
//! Requires the `skill` feature flag.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use simply_daemon_api::{
    Daemon, Skill, AssetId,
    AddRelationRequest, CreateEntityRequest,
};
use simply_rpc::RequestContext;
use llm::ToolDefinition;
use rmcp::model::{CallToolResult, Content};

use crate::GoogleDocsClient;

/// Provider ID for Google OAuth — shared across all Google-based skills and servers.
/// Tokens are stored in TransientTokenStore keyed by (user_id, "google").
const GOOGLE_PROVIDER_ID: &str = "google";

/// Google Docs skill — import documents from Google Drive.
///
/// Takes `Arc<dyn Daemon>` — works with both embedded and remote daemons.
/// Google OAuth tokens come from the daemon's token store via the caller's
/// `RequestContext`.
pub struct GDocsSkill {
    daemon: Arc<dyn Daemon>,
}

impl GDocsSkill {
    pub fn new(daemon: Arc<dyn Daemon>) -> Self {
        Self { daemon }
    }

    fn get_google_token(ctx: &RequestContext) -> Result<String> {
        ctx.tokens.get(GOOGLE_PROVIDER_ID)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!(
                "No Google OAuth token found. Please authenticate with Google first."
            ))
    }

    async fn handle_import(&self, args: ImportArgs, ctx: &RequestContext) -> Result<CallToolResult> {
        let token = Self::get_google_token(ctx)?;
        let client = GoogleDocsClient::new(token);
        let rpc_ctx = ctx.clone();
        let entity = self.daemon.entity();

        info!(doc_id = %args.doc_id, "importing Google Doc");

        // Fetch from Google first — if this fails we leave any existing
        // imported copy of this doc alone. Re-imports only swap in the new
        // version after extraction + upload succeed.
        let extracted = client.extract_document(&args.doc_id).await?;
        info!(title = %extracted.title, tabs = extracted.tabs.len(), images = extracted.images.len(), "extracted");

        let origin = format!("google_drive:{}", args.doc_id);
        let prior = entity.get_entity_by_origin(&rpc_ctx, &origin).await?;

        // Store images as assets. Keep both AssetId (for GC refs) and BlobHash
        // (for content URLs) per Google object_id.
        let mut image_map: std::collections::HashMap<String, (AssetId, String)> = std::collections::HashMap::new();
        for image in &extracted.images {
            // `BinaryUpload.data` is raw bytes; serde base64s it on the wire.
            let info = self.daemon.asset().store_asset(
                &rpc_ctx,
                simply_daemon_api::BinaryUpload {
                    data: image.data.clone(),
                    mime_type: image.mime_type.clone(),
                },
            ).await?;
            image_map.insert(image.object_id.clone(), (info.id, info.blob_hash.as_str().to_string()));
        }

        // Create the container entity. Child tabs reference it via
        // `structure::contained_in` — the tab is `from`, the container is `to`.
        let doc = entity.create_entity(&rpc_ctx, CreateEntityRequest {
            kind: "document::tabbed".to_string(),
            title: Some(extracted.title.clone()),
            content: None,
            origin: Some(origin.clone()),
            referenced_assets: Vec::new(),
        }).await?;

        // Topologically walk the tab tree: parents before children. Tabs with
        // no `parent_tab_id` hang off the container; nested tabs hang off
        // another tab.
        let mut id_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut pending: Vec<_> = extracted.tabs.into_iter().collect();
        let mut max_passes = pending.len() + 1;

        while !pending.is_empty() && max_passes > 0 {
            max_passes -= 1;
            let mut deferred = Vec::new();
            for tab in pending.drain(..) {
                if let Some(ref parent) = tab.parent_tab_id {
                    if !id_map.contains_key(parent) { deferred.push(tab); continue; }
                }
                let container_id = tab.parent_tab_id.as_ref()
                    .and_then(|pid| id_map.get(pid).cloned())
                    .unwrap_or_else(|| doc.id.clone());

                // Substitute object refs with blob URLs, and collect AssetIds for
                // images that actually appear in this tab's content.
                let mut content = tab.content_markdown.clone();
                let mut referenced_assets: Vec<AssetId> = Vec::new();
                for (oid, (asset_id, hash)) in &image_map {
                    let needle = format!("object:{oid}");
                    if content.contains(&needle) {
                        referenced_assets.push(asset_id.clone());
                        content = content.replace(&needle, &format!("/api/blob/{hash}"));
                    }
                }

                let title = match &tab.icon {
                    Some(icon) => format!("{icon} {}", tab.title),
                    None => tab.title.clone(),
                };

                let tab_entity = entity.create_entity(&rpc_ctx, CreateEntityRequest {
                    kind: "document::tab".to_string(),
                    title: Some(title),
                    content: Some(content),
                    origin: None,
                    referenced_assets,
                }).await?;

                entity.add_relation(&rpc_ctx, AddRelationRequest {
                    from_id: tab_entity.id.clone(),
                    to_id: container_id,
                    relation: "structure::contained_in".to_string(),
                    position: Some(tab.tab_index as i64),
                }).await?;

                id_map.insert(tab.source_tab_id.clone(), tab_entity.id);
            }
            pending = deferred;
        }

        // New version is fully written — now it's safe to retire the old one.
        // If this step fails, both versions briefly coexist at the same
        // origin; the user's next import will find the newer one (via
        // `get_entity_by_origin` + the stable id) and clean up.
        if let Some(old) = prior {
            if let Err(e) = entity.delete_entity(&rpc_ctx, &old.id).await {
                info!(old_id = %old.id, error = %e, "gdocs_import: failed to delete prior version (new version is live)");
            }
        }

        info!(tabs = id_map.len(), images = image_map.len(), "gdocs_import: done");
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Imported '{}' with {} tabs and {} images (entity_id: {})",
            extracted.title, id_map.len(), image_map.len(), doc.id,
        ))]))
    }

    async fn handle_list(&self, args: ListArgs, ctx: &RequestContext) -> Result<CallToolResult> {
        let token = Self::get_google_token(ctx)?;
        let client = GoogleDocsClient::new(token);
        let files = client.list_documents(args.query.as_deref(), args.limit.unwrap_or(20)).await?;
        let result: Vec<serde_json::Value> = files.into_iter().map(|f| {
            serde_json::json!({ "id": f.id, "name": f.name, "modified_time": f.modified_time })
        }).collect();
        Ok(CallToolResult::success(vec![Content::text(serde_json::to_string_pretty(&result)?)]))
    }
}

#[derive(Deserialize)]
struct ImportArgs { doc_id: String }

#[derive(Deserialize)]
struct ListArgs { query: Option<String>, limit: Option<usize> }

#[async_trait]
impl Skill for GDocsSkill {
    fn name(&self) -> &str { "gdocs" }

    fn oauth_requirements(&self) -> Vec<simply_daemon_api::skill::OAuthRequirement> {
        vec![simply_daemon_api::skill::OAuthRequirement {
            provider_id: GOOGLE_PROVIDER_ID.to_string(),
            display_name: "Google".to_string(),
            authorization_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
            token_url: "https://oauth2.googleapis.com/token".to_string(),
            scopes: vec![
                "https://www.googleapis.com/auth/documents.readonly".to_string(),
                "https://www.googleapis.com/auth/drive.readonly".to_string(),
                // Needed by the userinfo endpoint so OAuth callback can resolve the user's identity.
                "https://www.googleapis.com/auth/userinfo.email".to_string(),
            ],
            userinfo_url: Some("https://www.googleapis.com/oauth2/v2/userinfo".to_string()),
        }]
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "gdocs_import".to_string(),
                description: Some("Import a Google Doc into the document store with all tabs and images.".to_string()),
                input_schema: schemars::schema_for!(ImportToolInput),
            },
            ToolDefinition {
                name: "gdocs_list".to_string(),
                description: Some("List Google Docs from the user's Drive.".to_string()),
                input_schema: schemars::schema_for!(ListToolInput),
            },
        ]
    }

    async fn call_tool(&self, name: &str, arguments: serde_json::Value, ctx: &RequestContext) -> Result<CallToolResult> {
        match name {
            "gdocs_import" => self.handle_import(serde_json::from_value(arguments)?, ctx).await,
            "gdocs_list" => self.handle_list(serde_json::from_value(arguments)?, ctx).await,
            _ => anyhow::bail!("unknown tool: {name}"),
        }
    }
}

// No access_token in tool schemas — daemon injects it via RequestContext.tokens
#[derive(schemars::JsonSchema)]
#[allow(dead_code)] // schema-only: fields describe the tool input, arguments are read as JSON
struct ImportToolInput {
    /// The Google Doc ID to import.
    doc_id: String,
}

#[derive(schemars::JsonSchema)]
#[allow(dead_code)] // schema-only: fields describe the tool input, arguments are read as JSON
struct ListToolInput {
    /// Optional search query to filter documents.
    query: Option<String>,
    /// Maximum number of documents to return (default: 20).
    limit: Option<usize>,
}
