//! Google Docs and Drive API client
//!
//! Uses the Docs API to fetch structured content and converts it to Markdown.
//! Each tab has its own body content, so we get proper per-tab markdown.

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::trace;

const DRIVE_API_BASE: &str = "https://www.googleapis.com/drive/v3";
const DOCS_API_BASE: &str = "https://docs.googleapis.com/v1";

/// Google Docs API client
pub struct GoogleDocsClient {
    http_client: Client,
    access_token: String,
}

/// File metadata from Drive API
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    #[serde(default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub created_time: Option<String>,
}

/// Response from Drive API list files
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveFileList {
    pub files: Vec<DriveFile>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

/// A tab extracted from a Google Doc - now returns markdown directly
#[derive(Debug, Clone, Serialize)]
pub struct ExtractedTab {
    pub source_tab_id: String,
    pub title: String,
    pub icon: Option<String>,
    /// Markdown content for this specific tab (not HTML!)
    pub content_markdown: String,
    pub parent_tab_id: Option<String>,
    pub tab_index: i32,
}

/// An image extracted from a Google Doc
#[derive(Debug, Clone, Serialize)]
pub struct ExtractedImage {
    pub object_id: String,
    pub data: Vec<u8>,
    pub mime_type: String,
}

/// Result of extracting a Google Doc with tabs and images
#[derive(Debug, Clone, Serialize)]
pub struct ExtractedDocument {
    pub doc_id: String,
    pub title: String,
    pub tabs: Vec<ExtractedTab>,
    /// Maps inline object ID -> image index in images vec
    pub image_mapping: HashMap<String, usize>,
    #[serde(skip_serializing)]
    pub images: Vec<ExtractedImage>,
}

/// Document metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentInfo {
    pub id: String,
    pub title: String,
    pub revision_id: Option<String>,
}

// ============================================================================
// Google Docs API response types - structured content
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocsDocument {
    document_id: String,
    title: String,
    #[serde(default)]
    tabs: Vec<DocsTab>,
    /// Lists definitions (for checkbox detection)
    #[serde(default)]
    lists: HashMap<String, ListDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocsTab {
    tab_properties: TabProperties,
    #[serde(default)]
    document_tab: Option<DocumentTab>,
    #[serde(default)]
    child_tabs: Vec<DocsTab>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TabProperties {
    tab_id: String,
    title: String,
    #[serde(default)]
    icon_emoji: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentTab {
    #[serde(default)]
    body: Option<Body>,
    #[serde(default)]
    inline_objects: HashMap<String, InlineObject>,
    #[serde(default)]
    lists: HashMap<String, ListDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Body {
    #[serde(default)]
    content: Vec<StructuralElement>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StructuralElement {
    #[serde(default)]
    paragraph: Option<Paragraph>,
    #[serde(default)]
    table: Option<Table>,
    #[serde(default)]
    section_break: Option<SectionBreak>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Paragraph {
    #[serde(default)]
    elements: Vec<ParagraphElement>,
    #[serde(default)]
    paragraph_style: Option<ParagraphStyle>,
    #[serde(default)]
    bullet: Option<Bullet>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParagraphStyle {
    #[serde(default)]
    named_style_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bullet {
    #[serde(default)]
    list_id: Option<String>,
    #[serde(default)]
    nesting_level: i32,
    /// Text style of the bullet glyph - used for checked checkboxes (strikethrough)
    #[serde(default)]
    text_style: Option<TextStyle>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParagraphElement {
    #[serde(default)]
    text_run: Option<TextRun>,
    #[serde(default)]
    inline_object_element: Option<InlineObjectElement>,
    #[serde(default)]
    rich_link: Option<RichLink>,
    #[serde(default)]
    person: Option<Person>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextRun {
    #[serde(default)]
    content: String,
    #[serde(default)]
    text_style: Option<TextStyle>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextStyle {
    #[serde(default)]
    bold: bool,
    #[serde(default)]
    italic: bool,
    #[serde(default)]
    strikethrough: bool,
    #[serde(default)]
    underline: bool,
    #[serde(default)]
    link: Option<Link>,
    #[serde(default)]
    weighted_font_family: Option<WeightedFontFamily>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Link {
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WeightedFontFamily {
    #[serde(default)]
    font_family: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InlineObjectElement {
    #[serde(default)]
    inline_object_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RichLink {
    #[serde(default)]
    rich_link_properties: Option<RichLinkProperties>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RichLinkProperties {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    uri: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Person {
    #[serde(default)]
    person_properties: Option<PersonProperties>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersonProperties {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Table {
    #[serde(default)]
    table_rows: Vec<TableRow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TableRow {
    #[serde(default)]
    table_cells: Vec<TableCell>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TableCell {
    #[serde(default)]
    content: Vec<StructuralElement>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SectionBreak {}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InlineObject {
    inline_object_properties: InlineObjectProperties,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InlineObjectProperties {
    embedded_object: EmbeddedObject,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmbeddedObject {
    #[serde(default)]
    image_properties: Option<ImageProperties>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageProperties {
    content_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListDefinition {
    #[serde(default)]
    list_properties: Option<ListProperties>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListProperties {
    #[serde(default)]
    nesting_levels: Vec<NestingLevel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NestingLevel {
    #[serde(default)]
    glyph_type: Option<String>,
}

// ============================================================================
// Implementation
// ============================================================================

impl GoogleDocsClient {
    /// Create a new client with the given access token
    pub fn new(access_token: String) -> Self {
        Self {
            http_client: Client::new(),
            access_token,
        }
    }

    /// List Google Docs from Drive
    pub async fn list_documents(
        &self,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DriveFile>> {
        let mut url = format!(
            "{}/files?pageSize={}&fields=files(id,name,mimeType,modifiedTime,createdTime)",
            DRIVE_API_BASE,
            limit.min(100)
        );

        // Filter to only Google Docs
        let mut q = "mimeType='application/vnd.google-apps.document'".to_string();

        // Add user query if provided. Match the title (name) as well as content —
        // `fullText contains` alone only searches body text, so searching by a
        // document's title wouldn't reliably match.
        if let Some(user_query) = query {
            let esc = user_query.replace('\'', "\\'");
            q = format!("{q} and (name contains '{esc}' or fullText contains '{esc}')");
        }

        url = format!("{}&q={}", url, urlencoding::encode(&q));

        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Drive API error ({}): {}", status, body));
        }

        let list: DriveFileList = response.json().await?;
        Ok(list.files)
    }

    /// Extract a Google Doc with all tabs and images
    /// Returns markdown content for each tab (not HTML!)
    pub async fn extract_document(&self, doc_id: &str) -> Result<ExtractedDocument> {
        tracing::info!("extract_document: Starting extraction for doc_id={}", doc_id);

        // Fetch document with all tabs content
        let url = format!(
            "{}/documents/{}?includeTabsContent=true",
            DOCS_API_BASE, doc_id
        );

        tracing::debug!("extract_document: Fetching document from {}", url);

        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await?;

        tracing::debug!("extract_document: Got response status={}", response.status());

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Docs API error ({}): {}", status, body));
        }

        let doc: DocsDocument = response.json().await?;
        tracing::info!("extract_document: Parsed document '{}' with {} tabs", doc.title, doc.tabs.len());

        // Collect all inline objects from all tabs for image fetching
        let mut all_inline_objects: HashMap<String, String> = HashMap::new();
        Self::collect_inline_objects(&doc.tabs, &mut all_inline_objects);

        tracing::info!("extract_document: Found {} inline objects", all_inline_objects.len());

        // Fetch all images
        let mut images = Vec::new();
        let mut image_mapping: HashMap<String, usize> = HashMap::new();

        for (object_id, content_uri) in &all_inline_objects {
            match self.fetch_image(content_uri).await {
                Ok((data, mime_type)) => {
                    image_mapping.insert(object_id.clone(), images.len());
                    images.push(ExtractedImage {
                        object_id: object_id.clone(),
                        data,
                        mime_type,
                    });
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch image {}: {}", object_id, e);
                }
            }
        }

        tracing::info!("extract_document: Fetched {} images", images.len());

        // Extract tabs with markdown content (using structured body, not HTML export!)
        let tabs = self.extract_tabs_markdown(&doc.tabs, &image_mapping, &doc.lists);

        tracing::info!("extract_document: Extracted {} tabs", tabs.len());

        Ok(ExtractedDocument {
            doc_id: doc.document_id,
            title: doc.title,
            tabs,
            image_mapping,
            images,
        })
    }

    /// Collect all inline object URIs from tabs recursively
    fn collect_inline_objects(
        tabs: &[DocsTab],
        objects: &mut HashMap<String, String>,
    ) {
        for tab in tabs {
            if let Some(document_tab) = &tab.document_tab {
                for (object_id, inline_obj) in &document_tab.inline_objects {
                    if let Some(uri) = inline_obj
                        .inline_object_properties
                        .embedded_object
                        .image_properties
                        .as_ref()
                        .and_then(|p| p.content_uri.clone())
                    {
                        objects.insert(object_id.clone(), uri);
                    }
                }
            }
            // Recurse into child tabs
            Self::collect_inline_objects(&tab.child_tabs, objects);
        }
    }

    /// Extract tabs with markdown content from structured body
    /// Each tab has its own content - no sharing needed!
    fn extract_tabs_markdown(
        &self,
        tabs: &[DocsTab],
        image_mapping: &HashMap<String, usize>,
        doc_lists: &HashMap<String, ListDefinition>,
    ) -> Vec<ExtractedTab> {
        let mut result = Vec::new();
        let mut work_queue: Vec<(&DocsTab, Option<String>, i32)> = Vec::new();

        // Add top-level tabs to queue (in reverse order so we pop in correct order)
        for (i, tab) in tabs.iter().enumerate().rev() {
            work_queue.push((tab, None, i as i32));
        }

        while let Some((tab, parent_id, tab_index)) = work_queue.pop() {
            tracing::debug!("extract_tabs_markdown: Processing tab '{}' (index={})",
                tab.tab_properties.title, tab_index);

            // Get lists from the tab (or fall back to document-level lists)
            let tab_lists = tab.document_tab
                .as_ref()
                .map(|dt| &dt.lists)
                .unwrap_or(doc_lists);

            // Convert this tab's body to markdown
            let content_markdown = if let Some(document_tab) = &tab.document_tab {
                if let Some(body) = &document_tab.body {
                    self.convert_body_to_markdown(body, image_mapping, tab_lists)
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            result.push(ExtractedTab {
                source_tab_id: tab.tab_properties.tab_id.clone(),
                title: tab.tab_properties.title.clone(),
                icon: tab.tab_properties.icon_emoji.clone(),
                content_markdown,
                parent_tab_id: parent_id.clone(),
                tab_index,
            });

            // Add child tabs to queue (in reverse order)
            for (i, child) in tab.child_tabs.iter().enumerate().rev() {
                work_queue.push((child, Some(tab.tab_properties.tab_id.clone()), i as i32));
            }
        }

        result
    }

    /// Convert a document body to markdown
    fn convert_body_to_markdown(
        &self,
        body: &Body,
        image_mapping: &HashMap<String, usize>,
        lists: &HashMap<String, ListDefinition>,
    ) -> String {
        let mut markdown_parts = Vec::new();

        for element in &body.content {
            if let Some(paragraph) = &element.paragraph {
                let para_md = self.convert_paragraph(paragraph, image_mapping, lists);
                if !para_md.is_empty() {
                    markdown_parts.push(para_md);
                }
            } else if let Some(table) = &element.table {
                let table_md = self.convert_table(table, image_mapping, lists);
                markdown_parts.push(table_md);
            } else if element.section_break.is_some() {
                markdown_parts.push("\n---\n".to_string());
            }
        }

        markdown_parts.join("\n")
    }

    /// Convert a paragraph to markdown
    fn convert_paragraph(
        &self,
        paragraph: &Paragraph,
        image_mapping: &HashMap<String, usize>,
        lists: &HashMap<String, ListDefinition>,
    ) -> String {
        // Build paragraph text from elements
        let mut text_parts = Vec::new();

        for element in &paragraph.elements {
            if let Some(text_run) = &element.text_run {
                text_parts.push(self.convert_text_run(text_run));
            } else if let Some(inline_obj) = &element.inline_object_element {
                if let Some(object_id) = &inline_obj.inline_object_id {
                    if image_mapping.contains_key(object_id) {
                        // Reference to image by object_id (will be resolved by consumer)
                        text_parts.push(format!("![image](object:{})", object_id));
                    }
                }
            } else if let Some(rich_link) = &element.rich_link {
                if let Some(props) = &rich_link.rich_link_properties {
                    let title = props.title.as_deref().unwrap_or("");
                    let uri = props.uri.as_deref().unwrap_or("");
                    if !title.is_empty() && !uri.is_empty() {
                        text_parts.push(format!("[{}]({})", title, uri));
                    } else if !uri.is_empty() {
                        text_parts.push(format!("[{}]({})", uri, uri));
                    } else if !title.is_empty() {
                        text_parts.push(title.to_string());
                    }
                }
            } else if let Some(person) = &element.person {
                if let Some(props) = &person.person_properties {
                    let name = props.name.as_deref()
                        .or(props.email.as_deref())
                        .unwrap_or("");
                    if !name.is_empty() {
                        text_parts.push(format!("@{}", name));
                    }
                }
            }
        }

        let text = text_parts.join("").trim().to_string();
        if text.is_empty() {
            return String::new();
        }

        // Check for heading style
        let heading_level = paragraph.paragraph_style
            .as_ref()
            .and_then(|s| s.named_style_type.as_ref())
            .map(|style| self.get_heading_level(style))
            .unwrap_or(0);

        if heading_level > 0 {
            return format!("{} {}\n", "#".repeat(heading_level), text);
        }

        // Check for bullet/list
        if let Some(bullet) = &paragraph.bullet {
            let indent = "  ".repeat(bullet.nesting_level as usize);
            let list_id = bullet.list_id.as_deref().unwrap_or("<none>");

            trace!(
                "Found bullet: list_id={}, nesting_level={}, text='{}'",
                list_id, bullet.nesting_level, text
            );

            // Check if this is a checkbox list
            let list_def = bullet.list_id.as_ref().and_then(|id| lists.get(id));
            let nesting_level = list_def
                .and_then(|list| list.list_properties.as_ref())
                .and_then(|props| props.nesting_levels.get(bullet.nesting_level as usize));

            let glyph_type = nesting_level.and_then(|level| level.glyph_type.as_ref());
            let is_checkbox = glyph_type
                .map(|glyph| glyph == "GLYPH_TYPE_UNSPECIFIED")
                .unwrap_or(false);

            trace!(
                "  list_def exists={}, nesting_level exists={}, glyph_type={:?}, is_checkbox={}",
                list_def.is_some(),
                nesting_level.is_some(),
                glyph_type,
                is_checkbox
            );

            // Log the full list definition for debugging
            if let Some(def) = list_def {
                trace!("  list_definition: {:?}", def);
            }

            if is_checkbox {
                // Check if checkbox is checked (bullet text_style has strikethrough)
                let is_checked = bullet.text_style
                    .as_ref()
                    .map(|ts| ts.strikethrough)
                    .unwrap_or(false);

                trace!(
                    "  checkbox: text_style={:?}, strikethrough={}",
                    bullet.text_style.as_ref().map(|ts| format!("bold={},italic={},strike={}", ts.bold, ts.italic, ts.strikethrough)),
                    is_checked
                );

                let checkbox_marker = if is_checked { "[x]" } else { "[ ]" };
                let result = format!("{}- {} {}", indent, checkbox_marker, text);
                trace!("  -> checkbox result (checked={}): '{}'", is_checked, result);
                return result;
            }
            let result = format!("{}- {}", indent, text);
            trace!("  -> list item result: '{}'", result);
            return result;
        }

        format!("{}\n", text)
    }

    /// Convert a text run to markdown with formatting
    fn convert_text_run(&self, text_run: &TextRun) -> String {
        let content = &text_run.content;

        // Skip trailing newlines (handled at paragraph level)
        if content == "\n" {
            return String::new();
        }

        let mut text = content.trim_end_matches('\n').to_string();
        if text.is_empty() {
            return String::new();
        }

        if let Some(style) = &text_run.text_style {
            // Check for code/monospace font
            if let Some(font) = &style.weighted_font_family {
                if let Some(family) = &font.font_family {
                    let family_lower = family.to_lowercase();
                    if family_lower.contains("courier") ||
                       family_lower.contains("consolas") ||
                       family_lower.contains("monaco") ||
                       family_lower.contains("monospace") {
                        text = format!("`{}`", text);
                    }
                }
            }

            // Handle links first (don't apply underline to links - they're underlined by default)
            let is_link = style.link.as_ref().and_then(|l| l.url.as_ref()).is_some();
            // Also detect URL-like text (underlined URLs without link property)
            let looks_like_url = text.starts_with("http://") || text.starts_with("https://");

            // Apply formatting. Wrap markers *inside* leading/trailing whitespace
            // — `**foo **` doesn't render as bold in CommonMark; needs to be `**foo** `.
            if style.bold {
                text = wrap_with_markers(&text, "**");
            }
            if style.italic {
                text = wrap_with_markers(&text, "*");
            }
            if style.strikethrough {
                text = wrap_with_markers(&text, "~~");
            }
            // Only apply underline to non-links (links are underlined by default in rendered HTML)
            // Also skip underline for URL-like text
            if style.underline && !is_link && !looks_like_url && !style.bold && !style.italic {
                // Markdown doesn't have underline, use HTML
                text = format!("<u>{}</u>", text);
            }

            // Handle links - also auto-link URL-like text
            if let Some(link) = &style.link {
                if let Some(url) = &link.url {
                    text = format!("[{}]({})", text, url);
                }
            } else if looks_like_url {
                // Auto-link underlined URLs that don't have a link property
                let url = text.clone();
                text = format!("[{}]({})", text, url);
            }
        }

        text
    }

    /// Convert a table to markdown
    fn convert_table(
        &self,
        table: &Table,
        image_mapping: &HashMap<String, usize>,
        lists: &HashMap<String, ListDefinition>,
    ) -> String {
        if table.table_rows.is_empty() {
            return String::new();
        }

        let mut markdown_rows = Vec::new();

        for (i, row) in table.table_rows.iter().enumerate() {
            let mut cell_contents = Vec::new();

            for cell in &row.table_cells {
                let mut cell_text = String::new();
                for element in &cell.content {
                    if let Some(paragraph) = &element.paragraph {
                        let para_text = self.convert_paragraph(paragraph, image_mapping, lists);
                        cell_text.push_str(para_text.trim());
                        cell_text.push(' ');
                    }
                }
                // Clean up cell text for table format
                let cell_text = cell_text.trim()
                    .replace('|', "\\|")
                    .replace('\n', " ");
                cell_contents.push(cell_text);
            }

            markdown_rows.push(format!("| {} |", cell_contents.join(" | ")));

            // Add header separator after first row
            if i == 0 {
                let separator = format!("| {} |",
                    cell_contents.iter().map(|_| "---").collect::<Vec<_>>().join(" | "));
                markdown_rows.push(separator);
            }
        }

        markdown_rows.join("\n") + "\n"
    }

    /// Get markdown heading level from Google Docs named style
    fn get_heading_level(&self, named_style: &str) -> usize {
        match named_style {
            "HEADING_1" | "TITLE" => 1,
            "HEADING_2" | "SUBTITLE" => 2,
            "HEADING_3" => 3,
            "HEADING_4" => 4,
            "HEADING_5" => 5,
            "HEADING_6" => 6,
            _ => 0,
        }
    }

    /// Fetch an image from a Google-hosted URL
    async fn fetch_image(&self, url: &str) -> Result<(Vec<u8>, String)> {
        let response = self
            .http_client
            .get(url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(anyhow!("Failed to fetch image ({})", status));
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/png")
            .split(';')
            .next()
            .unwrap_or("image/png")
            .to_string();

        let data = response.bytes().await?.to_vec();
        Ok((data, content_type))
    }

    /// Get document content as plain text (simple export, no tabs)
    pub async fn get_document_as_text(&self, doc_id: &str) -> Result<String> {
        let url = format!(
            "{}/files/{}/export?mimeType=text/plain",
            DRIVE_API_BASE, doc_id
        );

        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Drive API export error ({}): {}", status, body));
        }

        Ok(response.text().await?)
    }

    /// Get document content as markdown (uses structured content, not HTML export)
    pub async fn get_document_as_markdown(&self, doc_id: &str) -> Result<String> {
        let extracted = self.extract_document(doc_id).await?;

        // Combine all tabs' markdown
        let markdown = extracted.tabs
            .iter()
            .map(|tab| {
                if extracted.tabs.len() > 1 {
                    format!("# {}\n\n{}", tab.title, tab.content_markdown)
                } else {
                    tab.content_markdown.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        Ok(markdown)
    }

    /// Get document metadata
    pub async fn get_document_info(&self, doc_id: &str) -> Result<DocumentInfo> {
        let url = format!("{}/documents/{}", DOCS_API_BASE, doc_id);

        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Docs API error ({}): {}", status, body));
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct DocResponse {
            document_id: String,
            title: String,
            revision_id: Option<String>,
        }

        let doc: DocResponse = response.json().await?;
        Ok(DocumentInfo {
            id: doc.document_id,
            title: doc.title,
            revision_id: doc.revision_id,
        })
    }

    /// Get document revision ID for change detection
    pub async fn get_revision_id(&self, doc_id: &str) -> Result<Option<String>> {
        let info = self.get_document_info(doc_id).await?;
        Ok(info.revision_id)
    }
}

// URL encoding helper
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut result = String::new();
        for c in s.chars() {
            match c {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => {
                    result.push(c);
                }
                _ => {
                    for b in c.to_string().as_bytes() {
                        result.push_str(&format!("%{:02X}", b));
                    }
                }
            }
        }
        result
    }
}

/// Wrap non-whitespace content with `marker` on both sides, preserving any
/// leading/trailing whitespace outside the markers.
///
/// CommonMark's emphasis rules require the opening `**`/`*`/`~~` to be
/// left-flanking (no whitespace after) and the closing to be right-flanking
/// (no whitespace before). Without this, `**Premise: **` renders literally
/// instead of bolding "Premise:".
fn wrap_with_markers(text: &str, marker: &str) -> String {
    let trimmed_start = text.trim_start();
    let lead = &text[..text.len() - trimmed_start.len()];
    let trimmed = trimmed_start.trim_end();
    if trimmed.is_empty() {
        return text.to_string();
    }
    let trail = &trimmed_start[trimmed.len()..];
    format!("{lead}{marker}{trimmed}{marker}{trail}")
}
