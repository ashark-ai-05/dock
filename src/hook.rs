//! Claude Code (and Codex) hook stdin, turned into the optional fields a state report can carry.
//!
//! A hook command is invoked with JSON on stdin: `session_id`, `transcript_path`, `cwd`,
//! `hook_event_name`, `tool_name`, `tool_input`. Older wiring only passed `dock agent-state
//! working` on argv. Both have to keep working, and junk on stdin must never fail the hook —
//! a hook that errors interrupts the agent that fired it.

use serde::Deserialize;
use serde_json::Value;

/// What a Claude Code / Codex hook may put on stdin.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct HookPayload {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<Value>,
}

/// Parse hook JSON. Empty input and junk are both `None` — never an error.
pub fn parse_stdin(raw: &str) -> Option<HookPayload> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str::<HookPayload>(trimmed).ok()
}

/// A short sentence for the roster: tool name plus one identifying argument, when known.
pub fn activity_summary(payload: &HookPayload) -> Option<String> {
    let tool = payload
        .tool_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let detail = payload
        .tool_input
        .as_ref()
        .and_then(tool_detail)
        .map(|detail| truncate(&detail, 72));
    match (tool, detail, payload.hook_event_name.as_deref()) {
        (Some(tool), Some(detail), _) => Some(format!("{tool} {detail}")),
        (Some(tool), None, _) => Some(tool.to_owned()),
        (None, _, Some(event)) if !event.trim().is_empty() => Some(event.trim().to_owned()),
        _ => None,
    }
}

fn tool_detail(input: &Value) -> Option<String> {
    let object = input.as_object()?;
    for key in [
        "file_path",
        "path",
        "command",
        "pattern",
        "query",
        "url",
        "glob",
    ] {
        if let Some(value) = object.get(key).and_then(Value::as_str) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    value
        .chars()
        .take(max.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claude_pre_tool_use_payload_is_read_from_stdin() {
        let payload = parse_stdin(
            r#"{"session_id":"s1","transcript_path":"/tmp/t.jsonl","cwd":"/repo","hook_event_name":"PreToolUse","tool_name":"Read","tool_input":{"file_path":"src/runtime.rs"}}"#,
        )
        .expect("valid hook JSON");
        assert_eq!(payload.session_id.as_deref(), Some("s1"));
        assert_eq!(payload.tool_name.as_deref(), Some("Read"));
        assert_eq!(
            activity_summary(&payload).as_deref(),
            Some("Read src/runtime.rs")
        );
    }

    #[test]
    fn junk_on_stdin_is_ignored_rather_than_a_crash() {
        assert_eq!(parse_stdin(""), None);
        assert_eq!(parse_stdin("not json"), None);
        assert_eq!(parse_stdin("{"), None);
        assert_eq!(parse_stdin("[1,2,3]"), None);
    }

    #[test]
    fn argv_only_hooks_have_nothing_to_parse() {
        assert_eq!(parse_stdin("\n"), None);
    }
}
