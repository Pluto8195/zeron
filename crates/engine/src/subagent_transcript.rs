//! `READ_SUBAGENT_TRANSCRIPT`: full user/assistant turn history for a
//! subagent's own on-disk transcript (`agent-{agentId}.jsonl`) — web parity
//! with `session_canvas_server.py`'s `build_transcript_turns`
//! (server.py:345-413), the side-panel chat view's data source there.
//!
//! Deliberately a DIFFERENT output shape from `external_import.rs`'s
//! `parse_transcript`: that function folds a transcript into
//! `zeron_doc::SessionMessageEntry`/`MessagePart` — the shape a chat DOC
//! wants (coalesced deltas, tool calls/results merged into one part,
//! sanitized). A subagent transcript panel isn't a doc and never becomes
//! one; it wants the same flat, ungrouped list of raw turns the reference
//! tool renders, each with short previews rather than full tool
//! inputs/outputs. Rather than write a THIRD from-scratch JSON-line parser
//! for that, this reuses `external_import.rs`'s tolerant raw-line/raw-block
//! deserialization shapes (`RawLine`/`RawMessage`/`RawBlock`/`parse_blocks`,
//! widened to `pub(crate)` for this) and builds its own simple turn list on
//! top, matching `build_transcript_turns`'s two-pass algorithm exactly:
//!
//! 1. First pass over every line: collect `tool_result` block content by
//!    `tool_use_id` — a tool's result lands on a LATER line than its
//!    `tool_use`, in a `user`-role message, so it has to be indexed before
//!    turns are built, not folded in as lines are walked forward.
//! 2. Second pass: one turn per `user`/`assistant` line that has real text
//!    or at least one tool call (a text-free tool-only turn still renders as
//!    a row of tool chips); tool calls render as `{name, inputPreview,
//!    resultPreview}`, resolving each result from the first pass's index.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::Serialize;
use serde_json::Value;

use crate::EngineError;
use crate::external_import::{
    RawLine, TITLEGEN_PROMPT_PREFIX, imported_tool_result_output, parse_blocks,
};

/// `TOOL_INPUT_PREVIEW_CHARS`/`TOOL_RESULT_PREVIEW_CHARS`
/// (server.py:341-342).
const TOOL_INPUT_PREVIEW_CHARS: usize = 90;
const TOOL_RESULT_PREVIEW_CHARS: usize = 400;

/// Hard cap on turns returned per read, independent of the reference (which
/// has no cap — this engine's RPC reply has to cross an IPC boundary and
/// render in a UI panel, so an enormous subagent transcript is capped to its
/// most recent turns rather than shipped whole). Keeps the newest activity,
/// matching what a "what's this subagent doing" panel actually wants.
const MAX_TURNS: usize = 500;

/// One tool call rendered inside a turn.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentToolCall {
    pub name: String,
    pub input_preview: String,
    #[serde(default)]
    pub result_preview: Option<String>,
}

/// One `user`/`assistant` line rendered as a turn.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentTurn {
    /// `"user"` or `"assistant"` — the transcript line's own `type`, passed
    /// through verbatim (never re-derived) so this can never drift from what
    /// `RawLine::kind` actually said.
    pub role: String,
    pub text: String,
    /// The transcript line's own raw `timestamp` string, passed through
    /// unparsed — this panel only ever displays it, never sorts/diffs by it,
    /// so there is no reason to parse it into an epoch value the way
    /// `external_import.rs::parse_ts` does for doc-entry `created_at`.
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub tools: Vec<SubagentToolCall>,
}

/// `READ_SUBAGENT_TRANSCRIPT`'s reply shape: `{turns, model}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentTranscript {
    pub turns: Vec<SubagentTurn>,
    #[serde(default)]
    pub model: Option<String>,
}

/// `message_text` (server.py:119-126): a message's displayable text, from
/// either a bare string `content` or the `text` blocks of an array
/// `content` — joined with no separator, exactly like the reference's
/// `"".join(...)`. Works on ANY string-or-block-array value, so it also
/// doubles as a `tool_result` block's own nested `content` reader (pass 1
/// below) — the reference's `message_text` is called on both shapes as-is.
fn message_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(Value::as_object)
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .map(|block| block.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>()
            .concat(),
        _ => String::new(),
    }
}

/// Text carried by a Codex `response_item.message`. Codex uses
/// `input_text` for user/developer messages and `output_text` for assistant
/// messages; accepting `text` as well keeps this tolerant of older rollout
/// files without making developer/system messages displayable (their role is
/// filtered by the caller).
fn codex_message_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(Value::as_object)
            .filter(|block| {
                matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("input_text" | "output_text" | "text")
                )
            })
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Codex persists host-provided context as user-role `response_item.message`
/// rows. Those are not a human turn and would swamp a subagent peek with
/// plugin/environment boilerplate. Developer and system roles are rejected
/// separately; these are the known user-role host envelopes.
fn is_codex_synthetic_user_text(text: &str) -> bool {
    matches!(
        text.trim_start(),
        text if text.starts_with("<recommended_plugins>")
            || text.starts_with("<environment_context>")
            || text.starts_with("<skills_instructions>")
            || text.starts_with("<permissions instructions>")
            || text.starts_with("<apps_instructions>")
            || text.starts_with("<plugins_instructions>")
    )
}

fn codex_tool_input(payload: &serde_json::Map<String, Value>) -> Value {
    let Some(raw) = payload.get("arguments").or_else(|| payload.get("input")) else {
        return serde_json::json!({});
    };
    match raw {
        Value::String(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone()))
        }
        other => other.clone(),
    }
}

fn preview_value(value: &Value, limit: usize) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    truncate(&text, limit)
}

/// `truncate` (server.py:129-131): collapse all whitespace runs to single
/// spaces, then hard-truncate to `limit` codepoints with a trailing `…`.
/// `str.split()`/`len()` in Python operate on codepoints, matching Rust
/// `char` iteration here (not grapheme clusters) — same unit on both sides.
fn truncate(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    let head: String = collapsed.chars().take(limit.saturating_sub(1)).collect();
    format!("{}\u{2026}", head.trim_end())
}

/// Build the full turn list for one subagent transcript file. `Ok(default)`
/// (empty turns, no model) for a missing/unreadable file — an unknown
/// `agentId` is not an error, matching `SCAN_CHAT_SUBAGENTS`'s own
/// empty-not-error stance for an unresolvable chat.
fn build_transcript(path: &Path) -> Result<SubagentTranscript, EngineError> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok(SubagentTranscript::default());
    };
    let lines: Vec<&str> = content
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    // Pass 1 (server.py:362-373): tool_result content by tool_use_id. A
    // result can be an empty/whitespace-only string — stored as such (like
    // the reference's raw dict assignment) and treated as "no preview" only
    // when actually resolved below, not filtered out here.
    let mut results_by_id: HashMap<String, String> = HashMap::new();
    let mut codex_results_by_id: HashMap<String, String> = HashMap::new();
    for line in &lines {
        if let Ok(raw) = serde_json::from_str::<RawLine>(line)
            && let Some(message) = raw.message
        {
            for block in parse_blocks(&message.content) {
                if block.kind == "tool_result" && !block.tool_use_id.is_empty() {
                    results_by_id.insert(
                        block.tool_use_id,
                        imported_tool_result_output(&block.content).unwrap_or_default(),
                    );
                }
            }
        }

        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = value.get("payload").and_then(Value::as_object) else {
            continue;
        };
        if !matches!(
            payload.get("type").and_then(Value::as_str),
            Some("function_call_output" | "custom_tool_call_output")
        ) {
            continue;
        }
        let Some(call_id) = payload
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let output = payload
            .get("output")
            .map(codex_message_text)
            .unwrap_or_default();
        codex_results_by_id.insert(call_id.to_string(), output);
    }

    // Pass 2 (server.py:377-413): one turn per user/assistant line.
    let mut turns = Vec::new();
    let mut model: Option<String> = None;
    for line in &lines {
        let value = serde_json::from_str::<Value>(line).ok();
        if let Some(value) = value.as_ref() {
            if matches!(
                value.get("type").and_then(Value::as_str),
                Some("turn_context" | "session_meta")
            ) && let Some(found) = value
                .get("payload")
                .and_then(|payload| payload.get("model"))
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
            {
                model = Some(found.to_string());
            }

            if value.get("type").and_then(Value::as_str) == Some("response_item")
                && let Some(payload) = value.get("payload").and_then(Value::as_object)
            {
                let timestamp = value
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match payload.get("type").and_then(Value::as_str) {
                    Some("message") => {
                        let role = match payload.get("role").and_then(Value::as_str) {
                            Some("user") => "user",
                            Some("assistant") => "assistant",
                            // Host instructions are persisted as developer or
                            // system messages. They are context, not turns.
                            _ => continue,
                        };
                        let text = payload
                            .get("content")
                            .map(codex_message_text)
                            .unwrap_or_default();
                        if text.is_empty()
                            || (role == "user"
                                && (text.starts_with(TITLEGEN_PROMPT_PREFIX)
                                    || is_codex_synthetic_user_text(&text)))
                        {
                            continue;
                        }
                        turns.push(SubagentTurn {
                            role: role.to_string(),
                            text,
                            timestamp,
                            tools: Vec::new(),
                        });
                    }
                    Some("function_call" | "custom_tool_call") => {
                        let call_id = payload.get("call_id").and_then(Value::as_str).unwrap_or("");
                        let name = payload
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("Tool");
                        let input = codex_tool_input(payload);
                        let result_preview = codex_results_by_id
                            .get(call_id)
                            .filter(|text| !text.is_empty())
                            .map(|text| truncate(text, TOOL_RESULT_PREVIEW_CHARS));
                        turns.push(SubagentTurn {
                            role: "assistant".to_string(),
                            text: String::new(),
                            timestamp,
                            tools: vec![SubagentToolCall {
                                name: name.to_string(),
                                input_preview: preview_value(&input, TOOL_INPUT_PREVIEW_CHARS),
                                result_preview,
                            }],
                        });
                    }
                    _ => {}
                }
                continue;
            }
        }

        let Ok(raw) = serde_json::from_str::<RawLine>(line) else {
            continue;
        };
        let role = match raw.kind.as_deref() {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        let Some(message) = raw.message else { continue };
        if let Some(m) = message.model.filter(|m| !m.is_empty()) {
            model = Some(m); // last one wins, matching the reference's unconditional overwrite
        }
        let text = message_text(&message.content);
        if role == "user" && (text.is_empty() || text.starts_with(TITLEGEN_PROMPT_PREFIX)) {
            continue;
        }

        let mut tools = Vec::new();
        for block in parse_blocks(&message.content) {
            if block.kind != "tool_use" {
                continue;
            }
            let input = if block.input.is_null() {
                serde_json::json!({})
            } else {
                block.input
            };
            let result_preview = results_by_id
                .get(&block.id)
                .filter(|text| !text.is_empty())
                .map(|text| truncate(text, TOOL_RESULT_PREVIEW_CHARS));
            tools.push(SubagentToolCall {
                name: block.name,
                input_preview: truncate(&input.to_string(), TOOL_INPUT_PREVIEW_CHARS),
                result_preview,
            });
        }

        if text.is_empty() && tools.is_empty() {
            continue;
        }
        turns.push(SubagentTurn {
            role: role.to_string(),
            text,
            timestamp: raw.timestamp,
            tools,
        });
    }

    if turns.len() > MAX_TURNS {
        let drop = turns.len() - MAX_TURNS;
        turns.drain(0..drop);
    }

    Ok(SubagentTranscript { turns, model })
}

/// `(path, mtime)`-keyed cache over [`build_transcript`] — same strategy as
/// `context_usage.rs`'s `tail_cache`/`session_canvas_server.py`'s
/// `_transcript_analysis_cache`: an unchanged subagent transcript is never
/// re-parsed. Cheap to clone (an `Arc`ed inner map), same shape as
/// `ContextUsageProvider`.
#[derive(Clone, Default)]
pub struct TranscriptCache {
    inner: Arc<Mutex<HashMap<PathBuf, (SystemTime, Arc<SubagentTranscript>)>>>,
}

impl TranscriptCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached (or freshly parsed) transcript at `path`. A missing file —
    /// no cursor at all yet (`agentId` not found), or cleaned up since —
    /// returns an empty transcript, not an error, and is NOT cached (nothing
    /// to key a mtime off), so a later successful write is picked up on the
    /// very next call.
    pub fn get_or_build(&self, path: &Path) -> Result<Arc<SubagentTranscript>, EngineError> {
        let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified()) else {
            return Ok(Arc::new(SubagentTranscript::default()));
        };
        {
            let cache = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((cached_mtime, transcript)) = cache.get(path)
                && *cached_mtime == mtime
            {
                return Ok(transcript.clone());
            }
        }
        let built = Arc::new(build_transcript(path)?);
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.to_path_buf(), (mtime, built.clone()));
        Ok(built)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// A realistic four-line subagent transcript: a user turn, an assistant
    /// turn with a tool_use, the tool's result echoed back on a LATER user
    /// line (pass-1's reason to exist), and a final assistant text-only
    /// reply that also stamps the model.
    fn synthetic_transcript() -> String {
        [
            r#"{"type":"user","message":{"role":"user","content":"please read the file"}}"#,
            r#"{"type":"assistant","message":{"model":"claude-x","content":[{"type":"text","text":"On it."},{"type":"tool_use","id":"tu1","name":"Read","input":{"path":"/a/b.txt"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":"file contents here"}]}}"#,
            r#"{"type":"assistant","message":{"model":"claude-x","content":[{"type":"text","text":"Done reading it."}]}}"#,
        ]
        .join("\n")
    }

    #[test]
    fn builds_turns_across_lines_pairing_tool_use_with_its_later_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-sub1.jsonl");
        write(&path, &synthetic_transcript());

        let out = build_transcript(&path).expect("build");
        assert_eq!(out.model.as_deref(), Some("claude-x"));

        // The bare tool_result line contributes no turn of its own (it has
        // no user-authored text and folds into pass 1's index instead).
        assert_eq!(out.turns.len(), 3);

        assert_eq!(out.turns[0].role, "user");
        assert_eq!(out.turns[0].text, "please read the file");
        assert!(out.turns[0].tools.is_empty());

        assert_eq!(out.turns[1].role, "assistant");
        assert_eq!(out.turns[1].text, "On it.");
        assert_eq!(out.turns[1].tools.len(), 1);
        let tool = &out.turns[1].tools[0];
        assert_eq!(tool.name, "Read");
        assert_eq!(tool.input_preview, r#"{"path":"/a/b.txt"}"#);
        assert_eq!(tool.result_preview.as_deref(), Some("file contents here"));

        assert_eq!(out.turns[2].role, "assistant");
        assert_eq!(out.turns[2].text, "Done reading it.");
        assert!(out.turns[2].tools.is_empty());
    }

    #[test]
    fn builds_codex_response_items_without_host_instruction_noise() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-child.jsonl");
        write(
            &path,
            &[
                r#"{"timestamp":"2026-10-01T12:00:00Z","type":"turn_context","payload":{"model":"gpt-codex-test"}}"#,
                r#"{"timestamp":"2026-10-01T12:00:01Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"internal instructions"}]}}"#,
                r#"{"timestamp":"2026-10-01T12:00:02Z","type":"response_item","payload":{"type":"message","role":"system","content":[{"type":"input_text","text":"system instructions"}]}}"#,
                r#"{"timestamp":"2026-10-01T12:00:03Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<recommended_plugins>host data</recommended_plugins>"},{"type":"input_text","text":"<environment_context>cwd</environment_context>"}]}}"#,
                r#"{"timestamp":"2026-10-01T12:00:04Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Inspect the rollout code"}]}}"#,
                r#"{"timestamp":"2026-10-01T12:00:05Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"I found the relevant file."}]}}"#,
                r#"{"timestamp":"2026-10-01T12:00:06Z","type":"response_item","payload":{"type":"function_call","name":"read_file","arguments":"{\"path\":\"/tmp/a\"}","call_id":"call-1"}}"#,
                r#"{"timestamp":"2026-10-01T12:00:07Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"file contents"}}"#,
            ]
            .join("\n"),
        );

        let out = build_transcript(&path).expect("build");
        assert_eq!(out.model.as_deref(), Some("gpt-codex-test"));
        assert_eq!(out.turns.len(), 3);
        assert_eq!(out.turns[0].role, "user");
        assert_eq!(out.turns[0].text, "Inspect the rollout code");
        assert_eq!(out.turns[1].role, "assistant");
        assert_eq!(out.turns[1].text, "I found the relevant file.");
        assert_eq!(
            out.turns[2].timestamp.as_deref(),
            Some("2026-10-01T12:00:06Z")
        );
        assert_eq!(out.turns[2].tools.len(), 1);
        assert_eq!(out.turns[2].tools[0].name, "read_file");
        assert_eq!(out.turns[2].tools[0].input_preview, r#"{"path":"/tmp/a"}"#);
        assert_eq!(
            out.turns[2].tools[0].result_preview.as_deref(),
            Some("file contents")
        );
    }

    #[test]
    fn builds_codex_custom_tool_previews_used_by_exec_rollouts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-custom-tool.jsonl");
        write(
            &path,
            &[
                r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"run the focused test","call_id":"call-exec"}}"#,
                r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-exec","output":[{"type":"input_text","text":"Script completed\n"},{"type":"input_text","text":"1 passed"}]}}"#,
            ]
            .join("\n"),
        );

        let out = build_transcript(&path).expect("build");
        assert_eq!(out.turns.len(), 1);
        let tool = &out.turns[0].tools[0];
        assert_eq!(tool.name, "exec");
        assert_eq!(tool.input_preview, "run the focused test");
        assert_eq!(
            tool.result_preview.as_deref(),
            Some("Script completed 1 passed")
        );
    }

    #[test]
    fn a_missing_transcript_file_yields_an_empty_transcript_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-never-written.jsonl");
        let out = build_transcript(&path).expect("build");
        assert!(out.turns.is_empty());
        assert!(out.model.is_none());
    }

    #[test]
    fn titlegen_prompt_user_turns_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-titlegen.jsonl");
        write(
            &path,
            r#"{"type":"user","message":{"content":"Reply with ONLY a concise title for this chat"}}"#,
        );
        let out = build_transcript(&path).expect("build");
        assert!(out.turns.is_empty());
    }

    #[test]
    fn a_tool_only_turn_with_no_text_still_renders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-tools-only.jsonl");
        write(
            &path,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu9","name":"Bash","input":{}}]}}"#,
        );
        let out = build_transcript(&path).expect("build");
        assert_eq!(out.turns.len(), 1);
        assert_eq!(out.turns[0].text, "");
        assert_eq!(out.turns[0].tools.len(), 1);
        // `input: {}` falls back through the `|| {}` equivalent — still `{}`.
        assert_eq!(out.turns[0].tools[0].input_preview, "{}");
        assert!(out.turns[0].tools[0].result_preview.is_none());
    }

    #[test]
    fn previews_are_truncated_to_the_documented_char_limits() {
        let long_arg = "x".repeat(500);
        let long_result = "y".repeat(1000);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-long.jsonl");
        write(
            &path,
            &[
                format!(
                    r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"tu1","name":"Write","input":{{"text":"{long_arg}"}}}}]}}}}"#
                ),
                format!(
                    r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"tu1","content":"{long_result}"}}]}}}}"#
                ),
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"wrap-up"}]}}"#.to_string(),
            ]
            .join("\n"),
        );
        let out = build_transcript(&path).expect("build");
        let tool = &out.turns[0].tools[0];
        assert_eq!(tool.input_preview.chars().count(), TOOL_INPUT_PREVIEW_CHARS);
        assert!(tool.input_preview.ends_with('\u{2026}'));
        let result = tool.result_preview.as_ref().unwrap();
        assert_eq!(result.chars().count(), TOOL_RESULT_PREVIEW_CHARS);
        assert!(result.ends_with('\u{2026}'));
    }

    #[test]
    fn caps_turns_at_the_configured_maximum_keeping_the_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-huge.jsonl");
        let mut contents = String::new();
        for i in 0..(MAX_TURNS + 20) {
            contents.push_str(&format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"turn {i}"}}]}}}}"#
            ));
            contents.push('\n');
        }
        write(&path, &contents);
        let out = build_transcript(&path).expect("build");
        assert_eq!(out.turns.len(), MAX_TURNS);
        // Kept the tail, not the head.
        assert_eq!(out.turns[0].text, format!("turn {}", 20));
        assert_eq!(
            out.turns.last().unwrap().text,
            format!("turn {}", MAX_TURNS + 19)
        );
    }

    #[test]
    fn cache_reuses_the_parsed_result_until_mtime_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-cached.jsonl");
        write(&path, &synthetic_transcript());

        let cache = TranscriptCache::new();
        let first = cache.get_or_build(&path).expect("first build");
        assert_eq!(first.turns.len(), 3);
        assert_eq!(cache.inner.lock().unwrap().len(), 1);

        // Poison the cache directly (same technique as
        // `context_usage.rs`'s own cache test) to prove a same-mtime lookup
        // serves the cached value rather than re-reading the file.
        cache.inner.lock().unwrap().get_mut(&path).unwrap().1 =
            Arc::new(SubagentTranscript::default());
        let second = cache.get_or_build(&path).expect("second build");
        assert!(
            second.turns.is_empty(),
            "must serve the (poisoned) cached value, not re-read"
        );

        // Bump mtime forward and change content: must re-read.
        write(
            &path,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"new content"}]}}"#,
        );
        let future = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(future)
            .unwrap();
        let third = cache.get_or_build(&path).expect("third build");
        assert_eq!(third.turns.len(), 1);
        assert_eq!(third.turns[0].text, "new content");
    }

    #[test]
    fn get_or_build_returns_empty_not_error_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-nope.jsonl");
        let cache = TranscriptCache::new();
        let out = cache.get_or_build(&path).expect("get_or_build");
        assert!(out.turns.is_empty());
        assert!(out.model.is_none());
    }

    /// Wire-contract pin (same style as `subagent_scan.rs`'s own
    /// `subagent_summary_json_field_names_match_the_overview_ui_contract`
    /// and `pr_ticket_cache.rs`'s equivalent): `READ_SUBAGENT_TRANSCRIPT`'s
    /// raw JSON reply shape, pinned field-name-by-field-name so a future
    /// `#[serde(rename_all)]` refactor can't silently break the UI panel
    /// reading it.
    #[test]
    fn subagent_transcript_json_field_names_match_the_ui_contract() {
        let transcript = SubagentTranscript {
            turns: vec![SubagentTurn {
                role: "assistant".into(),
                text: "hello".into(),
                timestamp: Some("2026-01-01T00:00:00.000Z".into()),
                tools: vec![SubagentToolCall {
                    name: "Read".into(),
                    input_preview: "{\"path\":\"/a\"}".into(),
                    result_preview: Some("ok".into()),
                }],
            }],
            model: Some("claude-x".into()),
        };
        let value = serde_json::to_value(&transcript).unwrap();
        assert_eq!(value["model"], serde_json::json!("claude-x"));
        let turn = &value["turns"][0];
        assert_eq!(turn["role"], serde_json::json!("assistant"));
        assert_eq!(turn["text"], serde_json::json!("hello"));
        assert_eq!(
            turn["timestamp"],
            serde_json::json!("2026-01-01T00:00:00.000Z")
        );
        let tool = &turn["tools"][0];
        assert_eq!(tool["name"], serde_json::json!("Read"));
        assert_eq!(tool["inputPreview"], serde_json::json!("{\"path\":\"/a\"}"));
        assert_eq!(tool["resultPreview"], serde_json::json!("ok"));
    }
}
