//! Pure Markdown frontmatter parsing and serialization.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;

use crate::storage::ids::EntityId;
use crate::storage::types::EntityType;

const FRONTMATTER_DELIMITER: &str = "---";

/// Parsed Markdown document with optional YAML frontmatter.
#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownDocument {
    pub frontmatter: Option<Frontmatter>,
    pub body: String,
}

/// Borrowed split of a Markdown document before YAML parsing.
#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownSplit<'a> {
    pub raw_frontmatter: Option<&'a str>,
    pub body: &'a str,
}

/// YAML frontmatter split into Noema-owned fields and preserved user metadata.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Frontmatter {
    pub id: Option<EntityId>,
    pub kind: Option<EntityType>,
    pub origin: Option<String>,
    pub privacy: Option<String>,
    pub title: Option<String>,
    pub tags: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Canonical fields Noema owns or validates before accepting changes.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemFrontmatter {
    pub id: EntityId,
    pub kind: EntityType,
    pub origin: Option<String>,
    pub privacy: Option<String>,
}

/// A relative Markdown reference that may map to an entity asset.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MarkdownAssetReference {
    pub path: String,
    pub kind: MarkdownAssetReferenceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkdownAssetReferenceKind {
    Image,
    Link,
}

impl MarkdownDocument {
    pub fn without_frontmatter(body: impl Into<String>) -> Self {
        Self {
            frontmatter: None,
            body: body.into(),
        }
    }
}

impl Frontmatter {
    /// Return user-editable metadata with Noema-owned keys removed.
    pub fn user_metadata(&self) -> Map<String, Value> {
        let mut metadata = self.extra.clone();
        if let Some(title) = &self.title {
            metadata.insert("title".to_string(), Value::String(title.clone()));
        }
        if let Some(tags) = &self.tags {
            metadata.insert(
                "tags".to_string(),
                Value::Array(tags.iter().cloned().map(Value::String).collect()),
            );
        }
        metadata
    }

    fn from_value(value: Value) -> Result<Self> {
        let Value::Object(mut map) = value else {
            anyhow::bail!("frontmatter must be a YAML mapping");
        };

        let id = take_string(&mut map, "id")?.map(EntityId::from_string);
        let kind = take_string(&mut map, "kind")?.map(EntityType::new);
        let origin = take_string(&mut map, "origin")?;
        let privacy = take_string(&mut map, "privacy")?;
        let title = take_string(&mut map, "title")?;
        let tags = take_tags(&mut map)?;

        Ok(Self {
            id,
            kind,
            origin,
            privacy,
            title,
            tags,
            extra: map,
        })
    }

    fn to_value_with_system(&self, system: &SystemFrontmatter) -> Value {
        let mut map = Map::new();

        map.insert(
            "id".to_string(),
            Value::String(system.id.as_str().to_string()),
        );
        map.insert(
            "kind".to_string(),
            Value::String(system.kind.as_str().to_string()),
        );
        if let Some(origin) = &system.origin {
            map.insert("origin".to_string(), Value::String(origin.clone()));
        }
        if let Some(privacy) = &system.privacy {
            map.insert("privacy".to_string(), Value::String(privacy.clone()));
        }
        if let Some(title) = &self.title {
            map.insert("title".to_string(), Value::String(title.clone()));
        }
        if let Some(tags) = &self.tags {
            map.insert(
                "tags".to_string(),
                Value::Array(tags.iter().cloned().map(Value::String).collect()),
            );
        }

        for (key, value) in &self.extra {
            if !is_reserved_key(key) {
                map.insert(key.clone(), value.clone());
            }
        }

        Value::Object(map)
    }
}

/// Parse a Markdown document, splitting YAML frontmatter from the body.
pub fn parse_markdown(input: &str) -> Result<MarkdownDocument> {
    let split = split_markdown(input);
    let Some(raw_frontmatter) = split.raw_frontmatter else {
        return Ok(MarkdownDocument::without_frontmatter(split.body));
    };

    let value = if raw_frontmatter.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_yaml::from_str::<Value>(raw_frontmatter)
            .context("Failed to parse YAML frontmatter")?
    };

    Ok(MarkdownDocument {
        frontmatter: Some(Frontmatter::from_value(value)?),
        body: split.body.to_string(),
    })
}

/// Split Markdown into raw YAML frontmatter and body without parsing YAML.
pub fn split_markdown(input: &str) -> MarkdownSplit<'_> {
    let Some(remainder) = input.strip_prefix(FRONTMATTER_DELIMITER) else {
        return MarkdownSplit {
            raw_frontmatter: None,
            body: input,
        };
    };

    let Some(after_open) = remainder
        .strip_prefix('\n')
        .or_else(|| remainder.strip_prefix("\r\n"))
    else {
        return MarkdownSplit {
            raw_frontmatter: None,
            body: input,
        };
    };

    let Some((raw_frontmatter, body)) = split_frontmatter(after_open) else {
        return MarkdownSplit {
            raw_frontmatter: None,
            body: input,
        };
    };

    MarkdownSplit {
        raw_frontmatter: Some(raw_frontmatter),
        body,
    }
}

/// Serialize Markdown with canonical Noema system fields and preserved user metadata.
pub fn serialize_markdown(
    frontmatter: &Frontmatter,
    system: &SystemFrontmatter,
    body: &str,
) -> Result<String> {
    let value = frontmatter.to_value_with_system(system);
    let yaml = serde_yaml::to_string(&value).context("Failed to serialize YAML frontmatter")?;
    let yaml = strip_yaml_document_marker(&yaml);
    let yaml = yaml.trim_end();

    Ok(format!(
        "{FRONTMATTER_DELIMITER}\n{yaml}\n{FRONTMATTER_DELIMITER}\n{body}"
    ))
}

pub fn is_system_owned_key(key: &str) -> bool {
    matches!(key, "id" | "kind" | "origin")
}

pub fn is_policy_controlled_key(key: &str) -> bool {
    key == "privacy"
}

pub fn is_user_editable_key(key: &str) -> bool {
    matches!(key, "title" | "tags") || !is_reserved_key(key)
}

/// Extract relative inline Markdown link and image targets.
///
/// This does not update `entity_assets`; it only returns normalized targets for
/// the scanner/writer to project later.
pub fn extract_asset_references(markdown: &str) -> Vec<MarkdownAssetReference> {
    let mut refs = Vec::new();
    let mut seen = HashSet::new();
    let mut in_fence = false;

    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        extract_asset_references_from_line(line, &mut refs, &mut seen);
    }

    refs
}

fn is_reserved_key(key: &str) -> bool {
    is_system_owned_key(key) || is_policy_controlled_key(key) || matches!(key, "title" | "tags")
}

fn extract_asset_references_from_line(
    line: &str,
    refs: &mut Vec<MarkdownAssetReference>,
    seen: &mut HashSet<(MarkdownAssetReferenceKind, String)>,
) {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut in_code = false;

    while index < bytes.len() {
        match bytes[index] {
            b'`' => {
                in_code = !in_code;
                index += 1;
            }
            b'!' if !in_code && bytes.get(index + 1) == Some(&b'[') => {
                index = extract_inline_reference_at(
                    line,
                    index + 1,
                    MarkdownAssetReferenceKind::Image,
                    refs,
                    seen,
                )
                .unwrap_or(index + 1);
            }
            b'[' if !in_code => {
                index = extract_inline_reference_at(
                    line,
                    index,
                    MarkdownAssetReferenceKind::Link,
                    refs,
                    seen,
                )
                .unwrap_or(index + 1);
            }
            _ => index += 1,
        }
    }
}

fn extract_inline_reference_at(
    line: &str,
    open_bracket: usize,
    kind: MarkdownAssetReferenceKind,
    refs: &mut Vec<MarkdownAssetReference>,
    seen: &mut HashSet<(MarkdownAssetReferenceKind, String)>,
) -> Option<usize> {
    let close_bracket = find_unescaped_byte(line, open_bracket + 1, b']')?;
    let open_paren = close_bracket + 1;
    if line.as_bytes().get(open_paren) != Some(&b'(') {
        return Some(close_bracket + 1);
    }

    let close_paren = find_unescaped_byte(line, open_paren + 1, b')')?;
    let raw_target = &line[open_paren + 1..close_paren];
    if let Some(path) = normalize_asset_target(raw_target) {
        if seen.insert((kind, path.clone())) {
            refs.push(MarkdownAssetReference { path, kind });
        }
    }

    Some(close_paren + 1)
}

fn find_unescaped_byte(input: &str, start: usize, needle: u8) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut index = start;
    while index < bytes.len() {
        if bytes[index] == needle && (index == 0 || bytes[index - 1] != b'\\') {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn normalize_asset_target(raw: &str) -> Option<String> {
    let target = raw.trim();
    if target.is_empty() {
        return None;
    }

    let target = if let Some(rest) = target.strip_prefix('<') {
        rest.find('>').map(|end| &rest[..end])?
    } else {
        target.split_whitespace().next()?
    };

    let target = target.trim();
    if is_relative_asset_target(target) {
        Some(strip_fragment_and_query(target).to_string())
    } else {
        None
    }
}

fn is_relative_asset_target(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with('#')
        && !target.starts_with('/')
        && !target.starts_with('~')
        && !target.starts_with("//")
        && !has_uri_scheme(target)
}

fn has_uri_scheme(target: &str) -> bool {
    let Some((scheme, _)) = target.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn strip_fragment_and_query(target: &str) -> &str {
    target
        .find(['?', '#'])
        .map(|index| &target[..index])
        .unwrap_or(target)
}

fn split_frontmatter(input: &str) -> Option<(&str, &str)> {
    if input == FRONTMATTER_DELIMITER {
        return Some(("", ""));
    }

    if let Some(body) = input.strip_prefix(&format!("{FRONTMATTER_DELIMITER}\n")) {
        return Some(("", body));
    }
    if let Some(body) = input.strip_prefix(&format!("{FRONTMATTER_DELIMITER}\r\n")) {
        return Some(("", body));
    }

    let marker = format!("\n{FRONTMATTER_DELIMITER}\n");
    if let Some(index) = input.find(&marker) {
        return Some((&input[..index], &input[index + marker.len()..]));
    }

    let marker = format!("\r\n{FRONTMATTER_DELIMITER}\r\n");
    if let Some(index) = input.find(&marker) {
        return Some((&input[..index], &input[index + marker.len()..]));
    }

    let marker = format!("\n{FRONTMATTER_DELIMITER}");
    if input.ends_with(&marker) {
        let index = input.len() - marker.len();
        return Some((&input[..index], ""));
    }

    let marker = format!("\r\n{FRONTMATTER_DELIMITER}");
    if input.ends_with(&marker) {
        let index = input.len() - marker.len();
        return Some((&input[..index], ""));
    }

    None
}

fn strip_yaml_document_marker(yaml: &str) -> &str {
    yaml.strip_prefix("---\n")
        .or_else(|| yaml.strip_prefix("---\r\n"))
        .unwrap_or(yaml)
}

fn take_string(map: &mut Map<String, Value>, key: &str) -> Result<Option<String>> {
    let Some(value) = map.remove(key) else {
        return Ok(None);
    };

    match value {
        Value::String(s) => Ok(Some(s)),
        Value::Null => Ok(None),
        other => anyhow::bail!("frontmatter key `{key}` must be a string, got {other}"),
    }
}

fn take_tags(map: &mut Map<String, Value>) -> Result<Option<Vec<String>>> {
    let Some(value) = map.remove("tags") else {
        return Ok(None);
    };

    match value {
        Value::Array(values) => values
            .into_iter()
            .map(|value| match value {
                Value::String(tag) => Ok(tag),
                other => anyhow::bail!("frontmatter `tags` entries must be strings, got {other}"),
            })
            .collect::<Result<Vec<_>>>()
            .map(Some),
        Value::Null => Ok(None),
        other => anyhow::bail!("frontmatter key `tags` must be a string array, got {other}"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let parsed = parse_markdown(
            "---\nid: ent_1\nkind: document::note\ntitle: Notes\ntags: [rust, sqlite]\ncustom: true\n---\n# Body",
        )
        .unwrap();

        let frontmatter = parsed.frontmatter.unwrap();
        assert_eq!(frontmatter.id.unwrap().as_str(), "ent_1");
        assert_eq!(frontmatter.kind.unwrap().as_str(), "document::note");
        assert_eq!(frontmatter.title.as_deref(), Some("Notes"));
        assert_eq!(
            frontmatter.tags,
            Some(vec!["rust".to_string(), "sqlite".to_string()])
        );
        assert_eq!(frontmatter.extra.get("custom"), Some(&json!(true)));
        assert_eq!(parsed.body, "# Body");
    }

    #[test]
    fn preserves_plain_markdown_without_frontmatter() {
        let parsed = parse_markdown("# Body\n---\nnot frontmatter").unwrap();

        assert!(parsed.frontmatter.is_none());
        assert_eq!(parsed.body, "# Body\n---\nnot frontmatter");
    }

    #[test]
    fn supports_crlf_frontmatter_boundaries() {
        let parsed =
            parse_markdown("---\r\nid: ent_1\r\nkind: document::note\r\n---\r\nBody").unwrap();

        assert_eq!(parsed.frontmatter.unwrap().id.unwrap().as_str(), "ent_1");
        assert_eq!(parsed.body, "Body");
    }

    #[test]
    fn serializes_system_fields_and_preserves_extra_metadata() {
        let frontmatter = Frontmatter {
            title: Some("Title".to_string()),
            tags: Some(vec!["one".to_string()]),
            extra: Map::from_iter([("custom".to_string(), json!({"nested": true}))]),
            ..Default::default()
        };
        let system = SystemFrontmatter {
            id: EntityId::from_string("ent_1"),
            kind: EntityType::new("document::note"),
            origin: Some("google_drive:gdoc_1".to_string()),
            privacy: Some("private".to_string()),
        };

        let markdown = serialize_markdown(&frontmatter, &system, "Body").unwrap();

        assert!(markdown.starts_with("---\n"));
        assert!(markdown.contains("id: ent_1\n"));
        assert!(markdown.contains("kind: document::note\n"));
        assert!(markdown.contains("origin: google_drive:gdoc_1\n"));
        assert!(markdown.contains("privacy: private\n"));
        assert!(markdown.contains("title: Title\n"));
        assert!(markdown.contains("custom:\n"));
        assert!(markdown.ends_with("---\nBody"));
    }

    #[test]
    fn identifies_frontmatter_field_ownership() {
        assert!(is_system_owned_key("id"));
        assert!(is_system_owned_key("kind"));
        assert!(is_system_owned_key("origin"));
        assert!(is_policy_controlled_key("privacy"));
        assert!(is_user_editable_key("title"));
        assert!(is_user_editable_key("tags"));
        assert!(is_user_editable_key("custom"));
    }

    #[test]
    fn extracts_relative_asset_references() {
        let refs = extract_asset_references(
            r#"
![Image](attachments/image.png)
[PDF](docs/report.pdf "Report")
[Escaped \] label](../assets/file.txt)
[Remote](https://example.com/file.png)
[Anchor](#section)
`![Code](ignored.png)`
```
![Fenced](ignored-too.png)
```
"#,
        );

        assert_eq!(
            refs,
            vec![
                MarkdownAssetReference {
                    path: "attachments/image.png".to_string(),
                    kind: MarkdownAssetReferenceKind::Image,
                },
                MarkdownAssetReference {
                    path: "docs/report.pdf".to_string(),
                    kind: MarkdownAssetReferenceKind::Link,
                },
                MarkdownAssetReference {
                    path: "../assets/file.txt".to_string(),
                    kind: MarkdownAssetReferenceKind::Link,
                },
            ]
        );
    }

    #[test]
    fn extracts_angle_bracket_targets_and_strips_fragments() {
        let refs = extract_asset_references("[File](<folder/my file.pdf#page=2>)");

        assert_eq!(
            refs,
            vec![MarkdownAssetReference {
                path: "folder/my file.pdf".to_string(),
                kind: MarkdownAssetReferenceKind::Link,
            }]
        );
    }
}
