//! Message, author role, and content modelling.
//!
//! Deserialization policy: *stable* concepts (roles, the text/parts shape) are
//! strongly typed; *unstable* ones (new `content_type` values OpenAI adds
//! without notice) degrade into [`MessageContent::Other`] / [`ContentPart::Unknown`]
//! carrying the raw JSON, so a schema change can never fail a load.

use serde::Deserialize;
use serde_json::Value;

use crate::text;

/// Who authored a message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Role {
    User,
    Assistant,
    System,
    Developer,
    Tool,
    /// A role we do not know about yet; preserved verbatim.
    Other(String),
}

impl Role {
    /// Stable lowercase identifier, used in JSON output and comparisons.
    pub fn as_str(&self) -> &str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Developer => "developer",
            Role::Tool => "tool",
            Role::Other(other) => other,
        }
    }

    /// Title-cased heading used in Markdown transcripts.
    pub fn heading(&self) -> String {
        match self {
            Role::User => "User".into(),
            Role::Assistant => "Assistant".into(),
            Role::System => "System".into(),
            Role::Developer => "Developer".into(),
            Role::Tool => "Tool".into(),
            Role::Other(other) => {
                let clean = text::sanitize_display(other);
                let mut chars = clean.chars();
                match chars.next() {
                    None => "Unknown".into(),
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                }
            }
        }
    }
}

impl From<&str> for Role {
    fn from(raw: &str) -> Self {
        match raw {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "system" => Role::System,
            "developer" => Role::Developer,
            "tool" => Role::Tool,
            other => Role::Other(other.to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(de)?;
        Ok(Role::from(raw.as_str()))
    }
}

impl serde::Serialize for Role {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

/// Message author. `name` carries the tool name for `role: "tool"` messages.
#[derive(Debug, Clone, Deserialize)]
pub struct Author {
    #[serde(default = "default_role")]
    pub role: Role,
    #[serde(default)]
    pub name: Option<String>,
}

fn default_role() -> Role {
    Role::Other("unknown".to_string())
}

impl Default for Author {
    fn default() -> Self {
        Author {
            role: default_role(),
            name: None,
        }
    }
}

/// Message-level metadata we actually act on. Everything else is ignored.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MessageMetadata {
    /// Set by ChatGPT for scaffolding messages the UI never shows.
    #[serde(default)]
    pub is_visually_hidden_from_conversation: bool,
    #[serde(default)]
    pub model_slug: Option<String>,
}

/// One element of a multimodal message.
#[derive(Debug, Clone)]
pub enum ContentPart {
    Text(String),
    Image {
        pointer: Option<String>,
    },
    Audio {
        transcript: Option<String>,
    },
    File {
        name: Option<String>,
    },
    /// A part shape we do not recognise.
    Unknown {
        content_type: String,
    },
}

impl ContentPart {
    fn from_value(value: &Value) -> Self {
        if let Some(text) = value.as_str() {
            return ContentPart::Text(text.to_string());
        }
        let content_type = value
            .get("content_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match content_type {
            "text" => ContentPart::Text(
                value
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
            "image_asset_pointer" => ContentPart::Image {
                pointer: value
                    .get("asset_pointer")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            "audio_transcription" => ContentPart::Audio {
                transcript: value
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            "audio_asset_pointer" | "real_time_user_audio_video_asset_pointer" => {
                ContentPart::Audio { transcript: None }
            }
            "file" | "video_container_asset_pointer" => ContentPart::File {
                name: value
                    .get("name")
                    .or_else(|| value.get("file_name"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            other => ContentPart::Unknown {
                content_type: other.to_string(),
            },
        }
    }

    /// Render as Markdown. Non-textual parts become a concise marker rather
    /// than a raw JSON dump.
    pub fn render(&self) -> String {
        match self {
            ContentPart::Text(text) => text.clone(),
            ContentPart::Image { .. } => "[image attachment]".into(),
            ContentPart::Audio { transcript } => match transcript {
                Some(t) if !t.trim().is_empty() => format!("[audio transcript] {t}"),
                _ => "[audio attachment]".into(),
            },
            ContentPart::File { name } => match name {
                Some(n) => format!("[file attachment: {}]", text::sanitize_display(n)),
                None => "[file attachment]".into(),
            },
            ContentPart::Unknown { content_type } => {
                format!(
                    "[unsupported content: {}]",
                    text::sanitize_display(content_type)
                )
            }
        }
    }
}

/// The body of a message.
#[derive(Debug, Clone)]
pub enum MessageContent {
    /// `content_type: "text"` — the overwhelmingly common case.
    Text { parts: Vec<String> },
    /// `content_type: "multimodal_text"`.
    Multimodal { parts: Vec<ContentPart> },
    /// `content_type: "code"` — tool/analysis input.
    Code {
        language: Option<String>,
        text: String,
    },
    /// `content_type: "execution_output"` and friends — tool output.
    ToolOutput { text: String },
    /// Any `content_type` we do not model. `salvaged` holds recoverable plain
    /// text if there was any; `raw` is kept for `inspect` but never rendered.
    Other {
        content_type: String,
        salvaged: Option<String>,
        raw: Box<Value>,
    },
    /// Absent or empty content.
    Empty,
}

impl Default for MessageContent {
    fn default() -> Self {
        MessageContent::Empty
    }
}

impl MessageContent {
    /// Total-function conversion: never fails, never panics, never loses the
    /// original for unknown shapes.
    pub fn from_value(value: Value) -> Self {
        let Some(object) = value.as_object() else {
            return match value.as_str() {
                Some(s) if !s.is_empty() => MessageContent::Text {
                    parts: vec![s.to_string()],
                },
                _ => MessageContent::Empty,
            };
        };

        let content_type = object
            .get("content_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        match content_type.as_str() {
            "text" => {
                let parts = string_parts(object.get("parts"));
                if parts.iter().all(|p| p.trim().is_empty()) {
                    MessageContent::Empty
                } else {
                    MessageContent::Text { parts }
                }
            }
            "multimodal_text" => {
                let parts: Vec<ContentPart> = object
                    .get("parts")
                    .and_then(Value::as_array)
                    .map(|array| array.iter().map(ContentPart::from_value).collect())
                    .unwrap_or_default();
                if parts.is_empty() {
                    MessageContent::Empty
                } else {
                    MessageContent::Multimodal { parts }
                }
            }
            "code" => MessageContent::Code {
                language: object
                    .get("language")
                    .and_then(Value::as_str)
                    .filter(|l| !l.is_empty() && *l != "unknown")
                    .map(str::to_string),
                text: object
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            "execution_output" => MessageContent::ToolOutput {
                text: object
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            "" => MessageContent::Empty,
            other => {
                let salvaged = object
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        let parts = string_parts(object.get("parts"));
                        (!parts.is_empty()).then(|| parts.join("\n\n"))
                    })
                    .filter(|s| !s.trim().is_empty());
                MessageContent::Other {
                    content_type: other.to_string(),
                    salvaged,
                    raw: Box::new(value),
                }
            }
        }
    }

    /// Plain-text projection used for statistics and heuristics. Markers for
    /// non-textual content are *not* included, so word counts stay meaningful.
    pub fn plain_text(&self) -> String {
        match self {
            MessageContent::Text { parts } => parts.join("\n\n"),
            MessageContent::Multimodal { parts } => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text(t) => Some(t.as_str()),
                    ContentPart::Audio {
                        transcript: Some(t),
                    } => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
            MessageContent::Code { text, .. } | MessageContent::ToolOutput { text } => text.clone(),
            MessageContent::Other { salvaged, .. } => salvaged.clone().unwrap_or_default(),
            MessageContent::Empty => String::new(),
        }
    }

    /// Markdown rendering for transcripts.
    pub fn render_markdown(&self) -> String {
        match self {
            MessageContent::Text { parts } => parts
                .iter()
                .map(|p| p.trim_end())
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
            MessageContent::Multimodal { parts } => parts
                .iter()
                .map(ContentPart::render)
                .map(|p| p.trim_end().to_string())
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
            MessageContent::Code { language, text } => {
                fenced(language.as_deref().unwrap_or(""), text)
            }
            MessageContent::ToolOutput { text } => {
                if text.trim().is_empty() {
                    "[tool output omitted]".into()
                } else {
                    fenced("", text)
                }
            }
            MessageContent::Other {
                content_type,
                salvaged,
                ..
            } => match salvaged {
                Some(text) => text.trim_end().to_string(),
                None => format!("[{} content omitted]", text::sanitize_display(content_type)),
            },
            MessageContent::Empty => String::new(),
        }
    }

    /// The `content_type` string this content came from.
    pub fn content_type(&self) -> &str {
        match self {
            MessageContent::Text { .. } => "text",
            MessageContent::Multimodal { .. } => "multimodal_text",
            MessageContent::Code { .. } => "code",
            MessageContent::ToolOutput { .. } => "execution_output",
            MessageContent::Other { content_type, .. } => content_type,
            MessageContent::Empty => "empty",
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, MessageContent::Empty) || self.render_markdown().trim().is_empty()
    }
}

fn string_parts(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|array| {
            array
                .iter()
                .map(|part| match part {
                    Value::String(s) => s.clone(),
                    // Occasionally a "text" message carries an object part.
                    other => ContentPart::from_value(other).render(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build a fenced code block whose fence is always longer than any backtick
/// run inside the body, so untrusted content cannot break out of the fence.
fn fenced(language: &str, body: &str) -> String {
    let longest_run = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest_run.max(2) + 1);
    format!("{fence}{language}\n{}\n{fence}", body.trim_end())
}

impl<'de> Deserialize<'de> for MessageContent {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        Ok(MessageContent::from_value(Value::deserialize(de)?))
    }
}

/// A single ChatGPT message.
#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub author: Author,
    #[serde(default)]
    pub create_time: Option<f64>,
    #[serde(default)]
    pub content: MessageContent,
    #[serde(default)]
    pub metadata: MessageMetadata,
    #[serde(default)]
    pub recipient: Option<String>,
}

impl Message {
    pub fn role(&self) -> &Role {
        &self.author.role
    }

    /// True for scaffolding the ChatGPT UI itself hides.
    pub fn is_hidden(&self) -> bool {
        self.metadata.is_visually_hidden_from_conversation
    }

    /// True when there is nothing a reader would want to see.
    pub fn is_renderable(&self) -> bool {
        !self.content.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_plain_text_content() {
        let content = MessageContent::from_value(json!({
            "content_type": "text",
            "parts": ["hello", "world"]
        }));
        assert_eq!(content.plain_text(), "hello\n\nworld");
        assert_eq!(content.content_type(), "text");
    }

    #[test]
    fn empty_text_parts_collapse_to_empty() {
        let content = MessageContent::from_value(json!({
            "content_type": "text", "parts": ["", "   "]
        }));
        assert!(content.is_empty());
    }

    #[test]
    fn multimodal_renders_markers_not_json() {
        let content = MessageContent::from_value(json!({
            "content_type": "multimodal_text",
            "parts": [
                {"content_type": "image_asset_pointer", "asset_pointer": "file-service://x"},
                "describe this"
            ]
        }));
        let rendered = content.render_markdown();
        assert!(rendered.contains("[image attachment]"));
        assert!(rendered.contains("describe this"));
        assert!(!rendered.contains("file-service"));
        // Markers are excluded from the statistical projection.
        assert_eq!(content.plain_text(), "describe this");
    }

    #[test]
    fn unknown_content_type_survives_and_salvages_text() {
        let content = MessageContent::from_value(json!({
            "content_type": "some_future_type_2027",
            "text": "recoverable body"
        }));
        assert_eq!(content.content_type(), "some_future_type_2027");
        assert_eq!(content.render_markdown(), "recoverable body");
    }

    #[test]
    fn unknown_content_type_without_text_becomes_a_marker() {
        let content = MessageContent::from_value(json!({
            "content_type": "sea_shanty", "verses": 12
        }));
        let rendered = content.render_markdown();
        assert_eq!(rendered, "[sea_shanty content omitted]");
        assert!(!rendered.contains("verses"));
    }

    #[test]
    fn code_fence_cannot_be_escaped_by_hostile_content() {
        let content = MessageContent::from_value(json!({
            "content_type": "code",
            "language": "python",
            "text": "```\nnot really the end\n```"
        }));
        let rendered = content.render_markdown();
        assert!(rendered.starts_with("````python"));
        assert!(rendered.ends_with("````"));
    }

    #[test]
    fn missing_content_type_is_empty_not_an_error() {
        assert!(MessageContent::from_value(json!({})).is_empty());
        assert!(MessageContent::from_value(Value::Null).is_empty());
    }

    #[test]
    fn unknown_roles_are_preserved() {
        assert_eq!(Role::from("critic"), Role::Other("critic".into()));
        assert_eq!(Role::from("critic").heading(), "Critic");
        assert_eq!(Role::from("tool"), Role::Tool);
    }

    #[test]
    fn message_deserializes_with_missing_fields() {
        let message: Message = serde_json::from_value(json!({
            "author": {"role": "user"},
            "content": {"content_type": "text", "parts": ["hi"]},
            "unknown_future_field": {"nested": true}
        }))
        .expect("missing optional fields must not fail");
        assert_eq!(*message.role(), Role::User);
        assert!(!message.is_hidden());
    }
}
