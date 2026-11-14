//! SendPalette / API-send feature - ported from `src/lib/apiPresets.ts`,
//! `src/lib/apiTemplateRenderer.ts`, `src/lib/jsonPath.ts`, and the
//! `send_request` Tauri command in `src-tauri/src/lib.rs`. Runs
//! completely independent of whether the current workspace is local or
//! SSH - the request always goes straight from this process to the
//! external API, `AppConfig` (presets/tokens) being the only shared
//! state, and that's workspace-agnostic already.

use std::collections::HashMap;

/// One saved HTTP preset. `#[serde(default)]` on every field but `name`/
/// `url` mirrors the original Rust struct exactly, so an old
/// `~/.kotoshelf/config.json` (written by kotoshelf itself, or an older
/// preset saved before a field existed) still round-trips.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiPreset {
    pub name: String,
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body_template: String,
    #[serde(default)]
    pub prompt_template: String,
    #[serde(default)]
    pub response_json_path: Option<String>,
    #[serde(default)]
    pub response_target: String, // "newTab" | "afterSelection" | "statusOnly"
    #[serde(default)]
    pub session_id_path: Option<String>,
    #[serde(default)]
    pub session_updated_path: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_method() -> String {
    "POST".to_string()
}

fn default_timeout_secs() -> u64 {
    30
}

impl ApiPreset {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: "https://".to_string(),
            method: "POST".to_string(),
            headers: Vec::new(),
            body_template: String::new(),
            prompt_template: String::new(),
            response_json_path: None,
            response_target: "newTab".to_string(),
            session_id_path: None,
            session_updated_path: None,
            timeout_secs: default_timeout_secs(),
        }
    }
}

/// Values available to `{{placeholder}}` substitution - one built per
/// send, from the active tab's selection (or whole document, see
/// `app.rs`) and the app-wide config.
pub struct RenderContext<'a> {
    pub selection: &'a str,
    pub filename: &'a str,
    pub tokens: &'a HashMap<String, String>,
    /// `None` until a response has set one via a `SessionID: ...` line
    /// (see `extract_session_id`) - resolves to `""`/`null` rather than
    /// being left literal, unlike an unknown `{{tokens.X}}`.
    pub session_id: Option<&'a str>,
}

/// Expands every `{{placeholder}}` in `template` - a single pass, no
/// recursion (a resolved value is never re-scanned for further
/// placeholders). Unrecognized placeholders (including a `{{tokens.X}}`
/// for a token that doesn't exist) are left as literal `{{...}}` text,
/// exactly like the original - the visible typo is the point, it's
/// meant to signal a preset/token misconfiguration rather than
/// disappearing silently.
pub fn render_template(template: &str, ctx: &RenderContext) -> String {
    let re = regex::Regex::new(r"\{\{\s*([a-zA-Z0-9_.]+)\s*\}\}").unwrap();
    re.replace_all(template, |caps: &regex::Captures| {
        let key = &caps[1];
        resolve_placeholder(key, ctx).unwrap_or_else(|| caps[0].to_string())
    })
    .into_owned()
}

fn resolve_placeholder(key: &str, ctx: &RenderContext) -> Option<String> {
    match key {
        "selection" => Some(ctx.selection.to_string()),
        // JSON-escaped but with the wrapping quotes stripped, meant to
        // be dropped inside the template's own quotes: `"input":
        // "{{selectionJson}}"`.
        "selectionJson" => {
            let quoted = serde_json::to_string(ctx.selection).unwrap_or_default();
            Some(quoted[1..quoted.len() - 1].to_string())
        }
        "filename" => Some(ctx.filename.to_string()),
        "sessionId" => Some(ctx.session_id.unwrap_or("").to_string()),
        // Unquoted JSON literal (a quoted string, or the bare word
        // `null`), meant to be dropped in unquoted: `"field":
        // {{sessionIdJson}}`.
        "sessionIdJson" => Some(match ctx.session_id {
            Some(id) => serde_json::to_string(id).unwrap_or_else(|_| "null".to_string()),
            None => "null".to_string(),
        }),
        _ => {
            let name = key.strip_prefix("tokens.")?;
            ctx.tokens.get(name).cloned()
        }
    }
}

/// Matches a `SessionID: <value>` line anywhere in `text` (case-sensitive,
/// line-anchored) - how a preset's response feeds a session id back into
/// the *next* send via `{{sessionId}}`/`{{sessionIdJson}}`, without the
/// app tracking any session state itself.
pub fn extract_session_id(text: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?m)^SessionID:[ \t]*(.*)$").unwrap();
    let value = re.captures(text)?.get(1)?.as_str().trim();
    if value.is_empty() { None } else { Some(value.to_string()) }
}

/// Extracts the value at a dot-separated JSON path out of `raw_body`
/// (e.g. `choices.0.message.content` - array indices are just numeric
/// segments, no `[0]` bracket syntax). Returns `None` on a parse
/// failure, a missing key, a non-integer/out-of-range array index, or a
/// `null` result anywhere along the path - callers fall back to the raw
/// body in all of those cases. A non-string leaf is returned as its own
/// JSON text (`serde_json::to_string`), not coerced.
pub fn extract_json_path(raw_body: &str, path: &str) -> Option<String> {
    let root: serde_json::Value = serde_json::from_str(raw_body).ok()?;
    let mut node = root;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        node = match node {
            serde_json::Value::Array(mut arr) => {
                let idx: usize = segment.parse().ok()?;
                if idx >= arr.len() {
                    return None;
                }
                arr.swap_remove(idx)
            }
            serde_json::Value::Object(mut obj) => obj.remove(segment)?,
            _ => return None,
        };
        if node.is_null() {
            return None;
        }
    }
    if node.is_null() {
        return None;
    }
    match node {
        serde_json::Value::String(s) => Some(s),
        other => Some(other.to_string()),
    }
}

pub struct SendRequestInput {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    /// `None` skips `.body(...)` entirely (so a GET/DELETE with an empty
    /// body template sends no body at all, not `""`).
    pub body: Option<String>,
    pub timeout_secs: u64,
}

pub struct SendRequestOutput {
    pub status: u16,
    pub body: String,
}

/// Fires the already-template-resolved request. Blocking (`reqwest::blocking`)
/// - callers must run this off the UI thread (see `app.rs`'s
/// `send_with_preset`, which spawns it the same way SSH connects do)
/// since a slow/hanging API would otherwise freeze the whole window.
/// `timeout_secs` of `0` disables the timeout entirely, for APIs whose
/// responses can legitimately take longer than any fixed bound.
pub fn send_request(input: SendRequestInput) -> Result<SendRequestOutput, String> {
    if !input.url.starts_with("http://") && !input.url.starts_with("https://") {
        return Err("Invalid URL scheme: only http:// and https:// are allowed".to_string());
    }
    let mut client_builder = reqwest::blocking::Client::builder();
    client_builder = if input.timeout_secs == 0 {
        client_builder.timeout(None)
    } else {
        client_builder.timeout(std::time::Duration::from_secs(input.timeout_secs))
    };
    let client = client_builder.build().map_err(|e| format!("Failed to create HTTP client: {e}"))?;
    let method = reqwest::Method::from_bytes(input.method.as_bytes())
        .map_err(|_| format!("Invalid HTTP method: {}", input.method))?;
    let mut builder = client.request(method, &input.url);
    for (name, value) in &input.headers {
        let clean_name = name.trim_matches(|c: char| c.is_control());
        let clean_value = value.trim_matches(|c: char| c.is_control());
        if !clean_name.is_empty() {
            builder = builder.header(clean_name, clean_value);
        }
    }
    if let Some(body) = input.body {
        builder = builder.body(body);
    }
    let response = builder.send().map_err(|e| format!("Request failed: {e}"))?;
    let status = response.status().as_u16();
    let body = response.text().map_err(|e| format!("Failed to read response body: {e}"))?;
    Ok(SendRequestOutput { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(selection: &'a str, tokens: &'a HashMap<String, String>, session_id: Option<&'a str>) -> RenderContext<'a> {
        RenderContext { selection, filename: "note", tokens, session_id }
    }

    #[test]
    fn renders_selection() {
        let tokens = HashMap::new();
        assert_eq!(render_template("say {{selection}}!", &ctx("hi", &tokens, None)), "say hi!");
    }

    #[test]
    fn selection_json_strips_wrapping_quotes_but_escapes_content() {
        let tokens = HashMap::new();
        let out = render_template(r#"{"input": "{{selectionJson}}"}"#, &ctx("a \"quote\"\nnewline", &tokens, None));
        assert_eq!(out, r#"{"input": "a \"quote\"\nnewline"}"#);
    }

    #[test]
    fn unknown_token_is_left_literal() {
        let tokens = HashMap::new();
        assert_eq!(render_template("key={{tokens.missing}}", &ctx("", &tokens, None)), "key={{tokens.missing}}");
    }

    #[test]
    fn known_token_resolves() {
        let mut tokens = HashMap::new();
        tokens.insert("api_key".to_string(), "secret123".to_string());
        assert_eq!(render_template("Bearer {{tokens.api_key}}", &ctx("", &tokens, None)), "Bearer secret123");
    }

    #[test]
    fn session_id_resolves_to_empty_string_when_absent() {
        let tokens = HashMap::new();
        assert_eq!(render_template("id={{sessionId}}", &ctx("", &tokens, None)), "id=");
    }

    #[test]
    fn session_id_json_is_an_unquoted_null_or_quoted_string() {
        let tokens = HashMap::new();
        assert_eq!(render_template("s={{sessionIdJson}}", &ctx("", &tokens, None)), "s=null");
        assert_eq!(render_template("s={{sessionIdJson}}", &ctx("", &tokens, Some("abc"))), "s=\"abc\"");
    }

    #[test]
    fn extracts_session_id_line() {
        assert_eq!(extract_session_id("some text\nSessionID: abc123\nmore"), Some("abc123".to_string()));
        assert_eq!(extract_session_id("no session line here"), None);
        assert_eq!(extract_session_id("SessionID:   \n"), None); // empty after trim
    }

    #[test]
    fn json_path_extracts_nested_string() {
        let body = r#"{"choices": [{"message": {"content": "hello"}}]}"#;
        assert_eq!(extract_json_path(body, "choices.0.message.content"), Some("hello".to_string()));
    }

    #[test]
    fn json_path_returns_none_for_missing_key() {
        let body = r#"{"a": 1}"#;
        assert_eq!(extract_json_path(body, "b"), None);
    }

    #[test]
    fn json_path_returns_none_for_out_of_range_index() {
        let body = r#"{"items": [1, 2]}"#;
        assert_eq!(extract_json_path(body, "items.5"), None);
    }

    #[test]
    fn json_path_returns_none_for_invalid_json() {
        assert_eq!(extract_json_path("not json", "a"), None);
    }

    #[test]
    fn json_path_stringifies_non_string_leaves() {
        let body = r#"{"n": 42, "obj": {"x": 1}}"#;
        assert_eq!(extract_json_path(body, "n"), Some("42".to_string()));
        assert_eq!(extract_json_path(body, "obj"), Some(r#"{"x":1}"#.to_string()));
    }

    #[test]
    fn json_path_returns_none_when_a_step_resolves_to_null() {
        let body = r#"{"a": null}"#;
        assert_eq!(extract_json_path(body, "a.b"), None);
        assert_eq!(extract_json_path(body, "a"), None);
    }

    #[test]
    fn send_request_rejects_non_http_schemes() {
        let input = SendRequestInput {
            url: "file:///etc/passwd".to_string(),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            timeout_secs: 30,
        };
        assert!(send_request(input).is_err());
    }
}
