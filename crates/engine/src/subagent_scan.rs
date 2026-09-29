//! Subagent detection for an external Claude Code session (ticket 002 phase
//! 3): metadata only, not import. A session that spawned subagents (via the
//! Agent/Task tool) gets a sibling directory next to its own transcript file:
//!
//!   {project_dir}/{session_id}.jsonl          <- the session's own transcript
//!   {project_dir}/{session_id}/subagents/
//!       agent-{agent_id}.meta.json            <- spawn metadata (small)
//!       agent-{agent_id}.jsonl                <- the subagent's own transcript
//!
//! Verified against real on-disk data on this machine (this very session's
//! own subagents directory, among others) — not assumed from the ticket's
//! citation alone. A real `meta.json`:
//!   {"agentType":"fork","isFork":true,"description":"...","toolUseId":"...",
//!    "spawnDepth":1,"requestShape":"background","requestNonInteractive":false,
//!    "model":"inherit"}
//! Notably: **no status field**. There is no reliable on-disk signal for
//! "still running" vs "finished" — do not fabricate one. Callers that want a
//! liveness signal should compose this with `crate::liveness::check_liveness`
//! against the subagent's own `.jsonl` (recency/process-match), not invent a
//! meta.json field that doesn't exist.
//!
//! Deliberately metadata-only for this pass: this does NOT parse the
//! subagent's own transcript content, and does NOT create a chat/import it.
//! `meta.json`'s `description` is already a usable one-line preview without
//! needing `external_import.rs`'s `parse_transcript` (which is private to
//! that module, and reasonably so — it returns full `SessionMessageEntry`
//! content, more than this scope needs). If a later pass wants to actually
//! import a subagent as its own chat, `parse_transcript` would need to be
//! exposed the same way `decode_tool_use` was earlier this session — flagging
//! that here rather than duplicating it preemptively for a need this pass
//! doesn't have.
//!
//! Whether an imported subagent should become its own child `Chat` (reusing
//! `Chat::parent_chat_id`) is explicitly NOT decided here. That field is
//! documented (`crates/proto/src/entities.rs`) as "the chat whose agent
//! created this one (via the Zeron MCP server)" — Zeron's own live
//! in-app orchestration links, established by a running MCP tool call, not
//! by scanning history after the fact. Reusing it for an external session's
//! after-the-fact-discovered subagents would conflate two different
//! provenances (a live Zeron-orchestrated spawn vs. a fact discovered by
//! scanning someone else's already-finished directory tree) under one field
//! with no way to tell them apart later. This pass does not create any
//! `Chat` rows for subagents at all, so the question doesn't need answering
//! yet — flagged for whoever builds actual subagent import.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::EngineError;

/// `SUBAGENT_IDLE_DONE_SECONDS` (`session_canvas_server.py`:62) — the same
/// fixed liveness window the web reference uses for its subagent tiles: a
/// subagent's own transcript file hasn't been touched this recently, treat
/// it as finished. There is still no explicit "done" signal anywhere on
/// disk (see this module's own doc comment above) — this is a heuristic,
/// not a fact, exactly as it is for the web.
const SUBAGENT_IDLE_DONE_SECONDS: u64 = 15;

/// Web-parity liveness heuristic for one subagent — see
/// [`SUBAGENT_IDLE_DONE_SECONDS`]. Wire values are lowercase to match
/// `find_subagents`'s own `"running"`/`"done"` strings exactly (the UI reads
/// raw JSON, not just the Rust enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentStatus {
    Running,
    Done,
}

/// `running` iff `jsonl_path` exists and was modified within the last
/// [`SUBAGENT_IDLE_DONE_SECONDS`] — mirrors `find_subagents`'s
/// `os.path.exists(jsonl_path) and (time.time() - os.path.getmtime(jsonl_path)
/// < SUBAGENT_IDLE_DONE_SECONDS)` (server.py:429-431) exactly, including its
/// treatment of a missing file as `done` (never an error — a subagent whose
/// own transcript hasn't been created yet, or was cleaned up, isn't "still
/// running" by this signal). A future mtime (clock skew, or a filesystem
/// with second-granularity timestamps landing exactly on now) makes
/// `elapsed()` return `Err` — the reference's own `time.time() - mtime`
/// would be negative there, which is `< 15` too, so this counts it as
/// `running` rather than `done` to match.
fn subagent_status(jsonl_path: &Path) -> SubagentStatus {
    match std::fs::metadata(jsonl_path).and_then(|m| m.modified()) {
        Ok(mtime) => match mtime.elapsed() {
            Ok(elapsed) if elapsed.as_secs() < SUBAGENT_IDLE_DONE_SECONDS => SubagentStatus::Running,
            Ok(_) => SubagentStatus::Done,
            Err(_) => SubagentStatus::Running,
        },
        Err(_) => SubagentStatus::Done, // no transcript file yet/anymore
    }
}

/// One subagent spawned by an external session, as found on disk. Metadata
/// (plus a liveness heuristic) only — no transcript content; see
/// `READ_SUBAGENT_TRANSCRIPT` (`crate::subagent_transcript`) for that.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentSummary {
    pub agent_id: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    /// Absolute path to the subagent's own transcript, for a later pass that
    /// wants to actually read/import it.
    pub transcript_path: String,
    /// Web-parity liveness heuristic — see [`subagent_status`].
    pub status: SubagentStatus,
}

#[derive(Debug, Default, Deserialize)]
struct RawMeta {
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "agentType")]
    agent_type: Option<String>,
}

fn subagents_dir_for(transcript_path: &Path) -> Option<PathBuf> {
    let parent = transcript_path.parent()?;
    let session_id = transcript_path.file_stem()?.to_str()?;
    Some(parent.join(session_id).join("subagents"))
}

/// The on-disk path a subagent `agent_id`'s own transcript WOULD live at,
/// given its parent session's transcript path — derived server-side the same
/// way [`scan_subagents`] derives every `transcript_path` it returns, never
/// trusted from a client. Used by `READ_SUBAGENT_TRANSCRIPT`'s handler: the
/// client only ever sends `{chatId, agentId}`, not a path. Returns a path
/// unconditionally (existence is the transcript reader's problem, not this
/// derivation's — an unknown/not-yet-written agent id resolves to a path
/// that just doesn't exist yet, which `subagent_transcript::TranscriptCache`
/// treats as an empty transcript rather than an error).
pub(crate) fn subagent_transcript_path(parent_transcript_path: &Path, agent_id: &str) -> Option<PathBuf> {
    let dir = subagents_dir_for(parent_transcript_path)?;
    Some(dir.join(format!("agent-{agent_id}.jsonl")))
}

/// Subagents spawned by the session at `transcript_path` (its own
/// `{session_id}.jsonl`), if any. Empty (not an error) when the session
/// never spawned one, or the directory doesn't exist. Best-effort: an
/// unreadable/unparseable `meta.json` is skipped, not fatal to the scan —
/// matches `external_import.rs::parse_transcript`'s tolerant stance.
pub fn scan_subagents(transcript_path: &Path) -> Result<Vec<SubagentSummary>, EngineError> {
    let Some(dir) = subagents_dir_for(transcript_path) else {
        return Ok(Vec::new());
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new()); // no subagents directory — not an error
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(agent_id) = name
            .strip_prefix("agent-")
            .and_then(|rest| rest.strip_suffix(".meta.json"))
        else {
            continue; // not a meta.json (e.g. the sibling .jsonl) — skip
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<RawMeta>(&raw) else {
            continue;
        };
        let jsonl_path = dir.join(format!("agent-{agent_id}.jsonl"));
        let status = subagent_status(&jsonl_path);
        out.push(SubagentSummary {
            agent_id: agent_id.to_string(),
            description: meta.description,
            agent_type: meta.agent_type,
            transcript_path: jsonl_path.to_string_lossy().into_owned(),
            status,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn finds_subagents_for_a_session_that_spawned_them() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "sess-with-subagents";
        let transcript = dir.path().join(format!("{session_id}.jsonl"));
        write(&transcript, "{}\n");

        let subdir = dir.path().join(session_id).join("subagents");
        write(
            &subdir.join("agent-abc123.meta.json"),
            r#"{"agentType":"fork","isFork":true,"description":"Do the thing","toolUseId":"t1","spawnDepth":1,"requestShape":"background","requestNonInteractive":false,"model":"inherit"}"#,
        );
        write(&subdir.join("agent-abc123.jsonl"), "{}\n");

        let found = scan_subagents(&transcript).expect("scan");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].agent_id, "abc123");
        assert_eq!(found[0].description.as_deref(), Some("Do the thing"));
        assert_eq!(found[0].agent_type.as_deref(), Some("fork"));
        assert!(found[0].transcript_path.ends_with("agent-abc123.jsonl"));
    }

    #[test]
    fn a_session_with_no_subagents_returns_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("lonely-session.jsonl");
        write(&transcript, "{}\n");

        let found = scan_subagents(&transcript).expect("scan");
        assert!(found.is_empty());
    }

    #[test]
    fn a_malformed_meta_json_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "sess-with-junk";
        let transcript = dir.path().join(format!("{session_id}.jsonl"));
        write(&transcript, "{}\n");

        let subdir = dir.path().join(session_id).join("subagents");
        write(&subdir.join("agent-good.meta.json"), r#"{"description":"ok"}"#);
        write(&subdir.join("agent-bad.meta.json"), "not json at all");

        let found = scan_subagents(&transcript).expect("scan");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].agent_id, "good");
    }

    #[test]
    fn real_on_disk_subagents_for_this_conversations_own_session_parse_cleanly() {
        let real = Path::new(
            "/Users/mikey/.claude/projects/-Users-mikey-Projects-agent-mode-tools/29fd48ab-0258-4406-b725-26265045e05f.jsonl",
        );
        if !real.is_file() {
            return; // machine-specific; skip if not present rather than fail CI elsewhere
        }
        let found = scan_subagents(real).expect("scan real session");
        assert!(
            !found.is_empty(),
            "this conversation's own session is known to have spawned subagents"
        );
        assert!(found.iter().any(|s| s.description.is_some()));
    }

    #[test]
    fn status_is_running_for_a_freshly_written_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-fresh.jsonl");
        write(&path, "{}\n");
        assert_eq!(subagent_status(&path), SubagentStatus::Running);
    }

    #[test]
    fn status_is_done_once_the_transcript_has_gone_idle_past_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-idle.jsonl");
        write(&path, "{}\n");
        let stale = SystemTime::now() - std::time::Duration::from_secs(SUBAGENT_IDLE_DONE_SECONDS + 5);
        std::fs::File::open(&path).unwrap().set_modified(stale).unwrap();
        assert_eq!(subagent_status(&path), SubagentStatus::Done);
    }

    #[test]
    fn status_is_done_for_a_missing_transcript_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-never-written.jsonl");
        assert_eq!(subagent_status(&path), SubagentStatus::Done);
    }

    /// `scan_subagents` must actually stamp the heuristic onto each row it
    /// returns, not just compute it in isolation — a freshly-written sibling
    /// `.jsonl` (the common case: a subagent still running right now) should
    /// come back `running`.
    #[test]
    fn scan_subagents_stamps_status_from_the_sibling_jsonls_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "sess-with-a-live-subagent";
        let transcript = dir.path().join(format!("{session_id}.jsonl"));
        write(&transcript, "{}\n");

        let subdir = dir.path().join(session_id).join("subagents");
        write(&subdir.join("agent-live.meta.json"), r#"{"description":"still going"}"#);
        write(&subdir.join("agent-live.jsonl"), "{}\n");

        let found = scan_subagents(&transcript).expect("scan");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].status, SubagentStatus::Running);
    }

    /// Wire-contract pin (mirrors `pr_ticket_cache.rs`'s
    /// `chat_link_status_json_field_names_match_the_overview_ui_contract`):
    /// the overview UI (`crates/ui/src/overview.rs`, outside this crate)
    /// deserializes `ScanChatSubagents`'s raw JSON directly. This pins the
    /// exact wire keys/values — `agentId`/`transcriptPath`/`agentType`
    /// camelCase, and `status` as the lowercase string `"running"`/`"done"`
    /// (NOT the Rust enum's own `Running`/`Done` spelling) — so a future
    /// `#[serde(rename_all)]` refactor can't silently break the UI while
    /// still compiling clean on this side.
    #[test]
    fn subagent_summary_json_field_names_match_the_overview_ui_contract() {
        let running = SubagentSummary {
            agent_id: "abc123".into(),
            description: Some("Do the thing".into()),
            agent_type: Some("fork".into()),
            transcript_path: "/tmp/agent-abc123.jsonl".into(),
            status: SubagentStatus::Running,
        };
        let value = serde_json::to_value(&running).unwrap();
        assert_eq!(value["agentId"], serde_json::json!("abc123"));
        assert_eq!(value["agentType"], serde_json::json!("fork"));
        assert_eq!(value["transcriptPath"], serde_json::json!("/tmp/agent-abc123.jsonl"));
        assert_eq!(value["status"], serde_json::json!("running"));

        let done = SubagentSummary {
            status: SubagentStatus::Done,
            ..running
        };
        assert_eq!(serde_json::to_value(&done).unwrap()["status"], serde_json::json!("done"));
    }
}
