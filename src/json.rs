//! Minimal, dependency-free JSON serialization for `--json` output.
//!
//! deft already declares `serde` for the manifest/lockfile data model, but
//! pulling in `serde_json` just to emit a handful of flat CI-facing payloads
//! would needlessly grow the dependency footprint (see
//! docs/guides/architecture.md). This covers exactly the closed set of
//! shapes `deft build --json` and `deft doctor --json` need: objects,
//! arrays, strings, numbers, bools, and null.

use std::fmt::Write as _;

/// A JSON value restricted to what deft's `--json` payloads need.
#[derive(Debug, Clone)]
pub enum Json {
    Null,
    Bool(bool),
    Number(i64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn str(s: impl Into<String>) -> Json {
        Json::String(s.into())
    }

    /// Serialize to a compact, single-line JSON string.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Number(n) => {
                let _ = write!(out, "{n}");
            }
            Json::String(s) => {
                out.push('"');
                escape_into(s, out);
                out.push('"');
            }
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(fields) => {
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    escape_into(key, out);
                    out.push_str("\":");
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Escape a string's contents for embedding inside a JSON string literal.
fn escape_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_scalars() {
        assert_eq!(Json::Null.render(), "null");
        assert_eq!(Json::Bool(true).render(), "true");
        assert_eq!(Json::Bool(false).render(), "false");
        assert_eq!(Json::Number(-42).render(), "-42");
        assert_eq!(Json::str("hi").render(), "\"hi\"");
    }

    #[test]
    fn escapes_special_characters() {
        let rendered = Json::str("line1\nline2\t\"quoted\"\\").render();
        assert_eq!(rendered, "\"line1\\nline2\\t\\\"quoted\\\"\\\\\"");
    }

    #[test]
    fn escapes_control_characters_as_unicode_escapes() {
        let rendered = Json::str("\u{1}bell").render();
        assert_eq!(rendered, "\"\\u0001bell\"");
    }

    #[test]
    fn renders_arrays_and_objects_flat() {
        let value = Json::Object(vec![
            ("name".to_string(), Json::str("widgets")),
            ("count".to_string(), Json::Number(3)),
            (
                "tags".to_string(),
                Json::Array(vec![Json::str("a"), Json::str("b")]),
            ),
            ("fix".to_string(), Json::Null),
        ]);
        assert_eq!(
            value.render(),
            "{\"name\":\"widgets\",\"count\":3,\"tags\":[\"a\",\"b\"],\"fix\":null}"
        );
    }

    #[test]
    fn empty_array_and_object_render_compactly() {
        assert_eq!(Json::Array(Vec::new()).render(), "[]");
        assert_eq!(Json::Object(Vec::new()).render(), "{}");
    }
}
