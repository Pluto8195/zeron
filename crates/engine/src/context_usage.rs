//! Per-chat context-window usage percent — session-canvas parity (ticket
//! 002): tile sizing on the overview grid scales with how full a chat's
//! context window is (`session_canvas.html`'s `tileSize`, html:1040-1043).
//!
//! Two sources, tried in order:
//!
//! 1. **Live**: the chat's own [`zeron_doc::SessionDoc`] `contextUsage` meta
//!    stamp. Zeron's live run loop already tracks this for the composer's own
//!    context meter — `crates/harness/src/claude/normalize.rs`'s
//!    `Frame::Assistant` handling sums the harness's assistant-message
//!    `usage.{input_tokens,cache_read_input_tokens,cache_creation_input_tokens}`
//!    fields into an `AgentEvent::ContextUsage`, which `sessions.rs` writes
//!    into the doc via `SessionDoc::update_context_usage`
//!    (`crates/doc/src/schema.rs`). That write is part of the doc's persisted
//!    snapshot, not just in-memory run state, so a bare snapshot load +
//!    `LoroDoc::import` — the same lightweight peek
//!    `external_import.rs::repair_missing_timestamps` uses, deliberately
//!    bypassing `DocHost`'s warm-doc/LRU/room machinery — answers this for
//!    ANY chat that has run at least one turn through this engine, including
//!    across an engine restart (the snapshot survives it) and for a chat
//!    that isn't currently streaming.
//!
//! 2. **Transcript-tail fallback**: for a chat with no live doc stamp yet
//!    (an externally-imported/adopted session whose doc never went through
//!    the live run loop — by far the common case for a bulk-imported
//!    history — or a freshly-restarted engine that hasn't run a turn yet
//!    this process), tail-read the chat's on-disk Claude Code transcript
//!    (`~/.claude/projects/<project>/<session_id>.jsonl`) for the most
//!    recent NON-sidechain assistant message's `usage` block. Mirrors
//!    `session_canvas_server.py`'s own `context_pct_from_usage`
//!    (server.py:270-278) exactly, including its cache-by-`(path, mtime)`
//!    strategy (`_transcript_analysis_cache`) so an unchanged transcript is
//!    never re-read. Unlike `external_import.rs::parse_transcript`, this
//!    never parses the file front-to-back — it reads backward from EOF in
//!    growing chunks until it finds one qualifying line or gives up.
//!
//! Both paths compute the SAME fixed-percentage formula as the reference
//! tool: `min((input+cache_read+cache_creation)/200_000, 1.0)` — a FIXED
//! 200k divisor regardless of the model's real advertised context window
//! (the reference documents this as a deliberate approximation, not a bug).
//! This deliberately does NOT reuse `zeron_proto::ContextUsage::fraction()`,
//! which divides by the model's REAL window when known (a more accurate
//! number, appropriate for the composer's own meter) — parity with the
//! reference overview wants the same fixed approximation on both the live
//! and fallback paths here, not a more-accurate number on one and the fixed
//! one on the other.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use loro::LoroDoc;
use serde_json::Value;
use zeron_doc::SessionDoc;
use zeron_proto::HarnessId;
use zeron_sync::DocsStore;

use crate::EngineError;
use crate::external_import::ExternalSessionImporter;
use crate::workspace_host::WorkspaceHost;

/// Fixed context-window divisor, matching `session_canvas_server.py`'s
/// `CONTEXT_WINDOW_TOKENS = 200_000` (server.py:58) — see the module doc
/// comment for why this is fixed rather than the model's real window.
pub const CONTEXT_WINDOW_TOKENS: u64 = 200_000;

/// `context_pct_from_usage` (server.py:270-278): `min(tokens/200_000, 1.0)`.
pub fn pct_from_tokens(tokens: u64) -> f32 {
    (tokens as f64 / CONTEXT_WINDOW_TOKENS as f64).min(1.0) as f32
}

/// Backward tail-read starting window and hard cap — see
/// [`tail_last_assistant_usage_tokens`]. Doubling from 64KiB up to 8MiB
/// covers any transcript whose most recent assistant turn didn't have
/// several megabytes of tool output wedged after its own `usage` line, which
/// in practice is effectively always (a `usage` block accompanies EVERY
/// assistant message, one per turn).
const TAIL_INITIAL_WINDOW: u64 = 64 * 1024;
const TAIL_MAX_WINDOW: u64 = 8 * 1024 * 1024;

struct TailEntry {
    mtime: SystemTime,
    tokens: Option<u64>,
}

struct Inner {
    /// `session_id` -> resolved on-disk transcript path, cached after the
    /// first successful resolution (a session's transcript file never moves
    /// once created) — avoids re-globbing `~/.claude/projects` on every
    /// overview poll tick for a chat with no live doc stamp yet.
    session_paths: Mutex<HashMap<String, PathBuf>>,
    /// `(path)` -> the last-seen most-recent-assistant-usage token sum and
    /// the mtime it was read at — mirrors `session_canvas_server.py`'s
    /// `_transcript_analysis_cache`: an unchanged file is never re-read.
    tail_cache: Mutex<HashMap<PathBuf, TailEntry>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Cheap to clone (an `Arc` around the shared caches, plus the store handle
/// every chat doc already shares) — same shape as `PrTicketCache`/
/// `ExternalSessionImporter`.
#[derive(Clone)]
pub struct ContextUsageProvider {
    store: Arc<DocsStore>,
    inner: Arc<Inner>,
}

impl ContextUsageProvider {
    pub fn new(store: Arc<DocsStore>) -> Self {
        Self {
            store,
            inner: Arc::new(Inner {
                session_paths: Mutex::new(HashMap::new()),
                tail_cache: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Best-effort context-window occupancy for `chat_id`, `0.0..=1.0`.
    /// `None` only when there's truly nothing to compute it from: no live
    /// doc stamp AND no resolvable on-disk transcript (e.g. a chat that has
    /// never run a single turn anywhere). Blocking (a doc snapshot load plus,
    /// on the fallback path, filesystem reads) — run off the async path
    /// (`tokio::task::spawn_blocking`), matching every other filesystem-
    /// touching method in this crate's RPC layer.
    pub fn context_pct_for_chat(
        &self,
        workspace: &WorkspaceHost,
        importer: &ExternalSessionImporter,
        chat_id: &str,
    ) -> Result<Option<f32>, EngineError> {
        if let Some(pct) = self.live_pct(chat_id)? {
            return Ok(Some(pct));
        }
        let path = self.transcript_path_for_chat(workspace, importer, chat_id)?;
        Ok(path.and_then(|path| self.tail_pct(&path)))
    }

    /// Resolve `chat_id`'s on-disk Claude Code transcript path, trying the
    /// external-import cursor first (`ExternalSessionImporter::transcript_path_for`
    /// — the only source for a chat that was bulk/one-off imported) then
    /// falling back to the chat's own recorded harness session id (see
    /// [`Self::resolve_from_harness_session`]) — the same two-source
    /// resolution [`Self::context_pct_for_chat`]'s fallback path already
    /// used inline. Factored out as its own public method so OTHER callers
    /// that need "where is this chat's transcript on disk" (e.g. subagent
    /// scanning's `SCAN_CHAT_SUBAGENTS` handler) don't have to re-derive the
    /// same two-source resolution and risk quietly covering fewer chats than
    /// context-usage lookups do — which is exactly what happened before this
    /// method existed: subagent scanning only ever tried the import-cursor
    /// source, so a chat Zeron itself launched (the common case for one
    /// actively spawning subagents) never resolved a transcript at all.
    pub fn transcript_path_for_chat(
        &self,
        workspace: &WorkspaceHost,
        importer: &ExternalSessionImporter,
        chat_id: &str,
    ) -> Result<Option<PathBuf>, EngineError> {
        Ok(match importer.transcript_path_for(chat_id)? {
            Some(path) => Some(path),
            None => self.resolve_from_harness_session(workspace, chat_id),
        })
    }

    /// Live source: see the module doc comment's point 1.
    fn live_pct(&self, chat_id: &str) -> Result<Option<f32>, EngineError> {
        let Some(bytes) = self.store.load_snapshot(chat_id)? else {
            return Ok(None);
        };
        let loro = LoroDoc::new();
        if loro.import(&bytes).is_err() {
            return Ok(None); // corrupt/partial snapshot — not fatal here
        }
        let doc = SessionDoc::from_doc(loro);
        Ok(doc
            .context_usage()
            .and_then(|usage| usage.tokens)
            .map(pct_from_tokens))
    }

    /// Fallback path's transcript resolution when the chat never went
    /// through the external-import flow (so `transcript_path_for` has no
    /// cursor to read) but DOES have a recorded harness session — e.g. a
    /// chat Zeron itself launched or resumed, whose live doc stamp just
    /// hasn't landed yet this process. The empty-string "do not resume"
    /// tombstone (`WorkspaceHost::chat_harness_session`'s own contract)
    /// resolves to no session, same as having none at all.
    fn resolve_from_harness_session(
        &self,
        workspace: &WorkspaceHost,
        chat_id: &str,
    ) -> Option<PathBuf> {
        let chat = workspace.chat(chat_id).ok()??;
        let session_id = chat.harness_session_id?;
        if session_id.is_empty() {
            return None;
        }
        match chat.config.map(|config| config.harness) {
            Some(HarnessId::Codex) => self.resolve_codex_transcript_path(&session_id),
            Some(HarnessId::ClaudeCode) => self.resolve_transcript_path(&session_id),
            // Legacy rows may predate persisted chat config. Preserve the
            // historical Claude lookup, then try Codex's date tree rather
            // than making those sessions permanently unclassifiable.
            _ => self
                .resolve_transcript_path(&session_id)
                .or_else(|| self.resolve_codex_transcript_path(&session_id)),
        }
    }

    fn resolve_transcript_path(&self, session_id: &str) -> Option<PathBuf> {
        self.resolve_transcript_path_under(&default_projects_root(), session_id)
    }

    /// Codex stores rollouts below a date tree such as
    /// `~/.codex/sessions/2026/10/01/rollout-...-<session-id>.jsonl`, unlike
    /// Claude's single project-directory level. Keep this separate from the
    /// Claude resolver so the latter's cheap, shallow lookup is unchanged.
    fn resolve_codex_transcript_path(&self, session_id: &str) -> Option<PathBuf> {
        self.resolve_codex_transcript_path_under(&default_codex_sessions_root(), session_id)
    }

    fn resolve_codex_transcript_path_under(
        &self,
        sessions_root: &Path,
        session_id: &str,
    ) -> Option<PathBuf> {
        if let Some(path) = lock(&self.inner.session_paths).get(session_id) {
            return Some(path.clone());
        }
        let suffix = format!("{session_id}.jsonl");
        let mut dirs = vec![(sessions_root.to_path_buf(), 0usize)];
        while let Some((dir, depth)) = dirs.pop() {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() && depth < 3 {
                    dirs.push((path, depth + 1));
                } else if file_type.is_file()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(&suffix))
                {
                    lock(&self.inner.session_paths).insert(session_id.to_string(), path.clone());
                    return Some(path);
                }
            }
        }
        None
    }

    /// Split out from [`Self::resolve_transcript_path`] so the resolution
    /// logic is unit-testable against a synthetic project tree rather than
    /// depending on `$HOME`. Globally-unique filename convention: the
    /// session id IS the file stem regardless of which project directory it
    /// lives under — the same assumption `external_import.rs::scan`'s own
    /// "already imported" de-dupe relies on — so this stops at the first
    /// match rather than needing to reproduce Claude Code's own cwd → project
    /// directory name sanitization scheme.
    fn resolve_transcript_path_under(
        &self,
        projects_root: &Path,
        session_id: &str,
    ) -> Option<PathBuf> {
        if let Some(path) = lock(&self.inner.session_paths).get(session_id) {
            return Some(path.clone());
        }
        let entries = std::fs::read_dir(projects_root).ok()?;
        for project_dir in entries.flatten() {
            let candidate = project_dir.path().join(format!("{session_id}.jsonl"));
            if candidate.is_file() {
                lock(&self.inner.session_paths).insert(session_id.to_string(), candidate.clone());
                return Some(candidate);
            }
        }
        None
    }

    /// Transcript-tail fallback with the `(path, mtime)` cache — see the
    /// module doc comment's point 2.
    fn tail_pct(&self, path: &Path) -> Option<f32> {
        let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
        {
            let cache = lock(&self.inner.tail_cache);
            if let Some(entry) = cache.get(path)
                && entry.mtime == mtime
            {
                return entry.tokens.map(pct_from_tokens);
            }
        }
        let tokens = tail_last_assistant_usage_tokens(path);
        lock(&self.inner.tail_cache).insert(path.to_path_buf(), TailEntry { mtime, tokens });
        tokens.map(pct_from_tokens)
    }
}

fn default_projects_root() -> PathBuf {
    crate::repos::home_dir().join(".claude").join("projects")
}

fn default_codex_sessions_root() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::repos::home_dir().join(".codex"))
        .join("sessions")
}

/// The most recent NON-sidechain `assistant` message's summed
/// `input_tokens + cache_read_input_tokens + cache_creation_input_tokens`,
/// read from the TAIL of the transcript at `path` — never a full front-to-
/// back parse (`external_import.rs::parse_transcript`'s job, and overkill
/// for "what's the current occupancy right now"). Doubles its read window
/// from [`TAIL_INITIAL_WINDOW`] until it finds one qualifying line or can no
/// longer grow (hit [`TAIL_MAX_WINDOW`] or the start of the file). `None`
/// when the file is unreadable, empty, or truly carries no qualifying line
/// anywhere in the scanned range — a best-effort miss, matching every other
/// scan in this crate (liveness, subagent discovery, PR/ticket lookup).
fn tail_last_assistant_usage_tokens(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    if file_len == 0 {
        return None;
    }
    let mut window = TAIL_INITIAL_WINDOW.min(file_len);
    loop {
        let start = file_len.saturating_sub(window);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = vec![0u8; (file_len - start) as usize];
        file.read_exact(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);
        let mut lines: Vec<&str> = text.split('\n').collect();
        if start > 0 {
            // The first slice is a partial line fragment (the window started
            // mid-file) — drop it rather than risk mis-parsing a truncated
            // JSON object as "no usage here".
            lines.remove(0);
        }
        for line in lines.iter().rev() {
            if let Some(tokens) = parse_assistant_usage_line(line) {
                return Some(tokens);
            }
        }
        if start == 0 {
            return None; // scanned the whole file, found nothing
        }
        let next_window = window.saturating_mul(2).min(TAIL_MAX_WINDOW).min(file_len);
        if next_window <= window {
            return None; // capped out (or file itself is the cap) with no hit
        }
        window = next_window;
    }
}

/// One transcript line -> its assistant-usage token sum, iff it's a
/// non-sidechain `assistant` line whose `message.usage` carries at least one
/// of the three fields — mirrors the live wire parser's own gate
/// (`crates/harness/src/claude/normalize.rs`'s `Frame::Assistant` handling,
/// the `AgentEvent::ContextUsage` emission) exactly, so the fallback and live
/// paths can never silently disagree on what counts as "usage present".
/// `isSidechain` lines are a subagent's own thread (or, in
/// `external_import.rs`'s terms, out of scope for this session's own
/// context) — excluded the same way `external_import.rs::parse_transcript`
/// skips them for message content.
fn parse_assistant_usage_line(line: &str) -> Option<u64> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let usage = value.get("message")?.get("usage")?;
    const FIELDS: [&str; 3] = [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ];
    let mut found = false;
    let mut sum = 0u64;
    for key in FIELDS {
        if let Some(v) = usage.get(key).and_then(Value::as_u64) {
            sum = sum.saturating_add(v);
            found = true;
        }
    }
    found.then_some(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn pct_from_tokens_is_fixed_200k_and_caps_at_one() {
        assert_eq!(pct_from_tokens(0), 0.0);
        assert_eq!(pct_from_tokens(100_000), 0.5);
        assert_eq!(pct_from_tokens(200_000), 1.0);
        assert_eq!(
            pct_from_tokens(400_000),
            1.0,
            "must cap at 1.0, never exceed it"
        );
    }

    #[test]
    fn parse_assistant_usage_line_sums_all_three_fields() {
        let line = r#"{"type":"assistant","message":{"usage":{"input_tokens":200,"cache_read_input_tokens":40000,"cache_creation_input_tokens":1800,"output_tokens":100}}}"#;
        assert_eq!(parse_assistant_usage_line(line), Some(200 + 40000 + 1800));
    }

    #[test]
    fn parse_assistant_usage_line_accepts_a_single_present_field() {
        let line = r#"{"type":"assistant","message":{"usage":{"input_tokens":42}}}"#;
        assert_eq!(parse_assistant_usage_line(line), Some(42));
    }

    #[test]
    fn parse_assistant_usage_line_rejects_non_assistant_and_missing_usage() {
        assert_eq!(
            parse_assistant_usage_line(r#"{"type":"user","message":{"content":"hi"}}"#),
            None
        );
        assert_eq!(
            parse_assistant_usage_line(r#"{"type":"assistant","message":{"content":[]}}"#),
            None
        );
        assert_eq!(parse_assistant_usage_line("not json at all"), None);
        assert_eq!(parse_assistant_usage_line(""), None);
    }

    #[test]
    fn parse_assistant_usage_line_excludes_sidechain_subagent_lines() {
        let line =
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":999}}}"#;
        assert_eq!(
            parse_assistant_usage_line(line),
            None,
            "a subagent's own thread must not count toward its parent's context"
        );
    }

    fn assistant_line(tokens: u64) -> String {
        format!(r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":{tokens}}}}}}}"#)
    }

    #[test]
    fn tail_read_finds_the_most_recent_assistant_usage_in_a_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let contents = [
            r#"{"type":"user","message":{"content":"hi"}}"#.to_string(),
            assistant_line(100),
            r#"{"type":"user","message":{"content":"more"}}"#.to_string(),
            assistant_line(55000),
        ]
        .join("\n")
            + "\n";
        write(&path, &contents);

        assert_eq!(tail_last_assistant_usage_tokens(&path), Some(55000));
    }

    #[test]
    fn tail_read_returns_none_when_no_line_carries_usage_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        write(
            &path,
            &[
                r#"{"type":"user","message":{"content":"hi"}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#,
            ]
            .join("\n"),
        );
        assert_eq!(tail_last_assistant_usage_tokens(&path), None);
    }

    #[test]
    fn tail_read_returns_none_for_an_empty_or_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.jsonl");
        write(&empty, "");
        assert_eq!(tail_last_assistant_usage_tokens(&empty), None);

        let missing = dir.path().join("missing.jsonl");
        assert_eq!(tail_last_assistant_usage_tokens(&missing), None);
    }

    /// The window-doubling walk must cross the initial 64KiB boundary
    /// correctly: pad the file with enough non-matching lines that the
    /// *first* read window (well under 64KiB total) contains no usage line,
    /// forcing at least one doubling before the real (early-file) usage line
    /// is found.
    #[test]
    fn tail_read_grows_its_window_past_the_initial_chunk_when_needed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut contents = assistant_line(12345);
        contents.push('\n');
        // Pad well past TAIL_INITIAL_WINDOW (64KiB) with filler lines that
        // never match, so the first read window misses the real line above.
        let filler = r#"{"type":"user","message":{"content":"padding line to grow the file"}}"#;
        while contents.len() < (TAIL_INITIAL_WINDOW as usize) * 2 {
            contents.push_str(filler);
            contents.push('\n');
        }
        write(&path, &contents);

        assert_eq!(tail_last_assistant_usage_tokens(&path), Some(12345));
    }

    #[test]
    fn resolve_transcript_path_under_finds_a_session_by_id_regardless_of_project_dir_name() {
        let provider = ContextUsageProvider::new(Arc::new(
            DocsStore::open(tempfile::tempdir().unwrap().path()).unwrap(),
        ));
        let root = tempfile::tempdir().unwrap();
        let project_dir = root.path().join("-some-unrelated-project-name");
        let transcript = project_dir.join("session-abc.jsonl");
        write(&transcript, "{}\n");

        let found = provider.resolve_transcript_path_under(root.path(), "session-abc");
        assert_eq!(found.as_deref(), Some(transcript.as_path()));

        // Cached: a second lookup must not require the file to still exist
        // under the SAME root (proves it read from `session_paths`, not disk,
        // on the second call).
        let missing_root = tempfile::tempdir().unwrap();
        let found_again =
            provider.resolve_transcript_path_under(missing_root.path(), "session-abc");
        assert_eq!(found_again.as_deref(), Some(transcript.as_path()));
    }

    #[test]
    fn resolve_transcript_path_under_returns_none_for_an_unknown_session() {
        let provider = ContextUsageProvider::new(Arc::new(
            DocsStore::open(tempfile::tempdir().unwrap().path()).unwrap(),
        ));
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("-some-project")).unwrap();
        assert!(
            provider
                .resolve_transcript_path_under(root.path(), "no-such-session")
                .is_none()
        );
    }

    #[test]
    fn resolve_codex_transcript_path_under_walks_the_date_tree() {
        let provider = ContextUsageProvider::new(Arc::new(
            DocsStore::open(tempfile::tempdir().unwrap().path()).unwrap(),
        ));
        let root = tempfile::tempdir().unwrap();
        let transcript = root
            .path()
            .join("2026/10/01")
            .join("rollout-2026-10-01T09-58-03-session-abc.jsonl");
        write(&transcript, "{}\n");

        let found = provider.resolve_codex_transcript_path_under(root.path(), "session-abc");
        assert_eq!(found.as_deref(), Some(transcript.as_path()));
    }

    #[test]
    fn tail_pct_caches_by_mtime_and_reflects_a_real_content_change() {
        let provider = ContextUsageProvider::new(Arc::new(
            DocsStore::open(tempfile::tempdir().unwrap().path()).unwrap(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        write(&path, &assistant_line(1000));

        assert_eq!(provider.tail_pct(&path), Some(pct_from_tokens(1000)));
        assert_eq!(lock(&provider.inner.tail_cache).len(), 1);

        // Same mtime (file untouched): a cache-poisoning direct overwrite of
        // the cached tokens must still be served back — proves the mtime
        // check, not a fresh read, decided this.
        lock(&provider.inner.tail_cache)
            .get_mut(&path)
            .unwrap()
            .tokens = Some(999_999_999);
        assert_eq!(
            provider.tail_pct(&path),
            Some(1.0),
            "must serve the cached value, not re-read"
        );

        // Bump mtime forward and change content: must re-read and get the
        // new value, not the stale cached one.
        write(&path, &assistant_line(2000));
        let future = SystemTime::now() + std::time::Duration::from_secs(5);
        let file = File::open(&path).unwrap();
        file.set_modified(future).ok();
        assert_eq!(provider.tail_pct(&path), Some(pct_from_tokens(2000)));
    }

    #[test]
    fn live_pct_is_none_when_the_chat_has_no_snapshot_at_all() {
        let store = Arc::new(DocsStore::open(tempfile::tempdir().unwrap().path()).unwrap());
        let provider = ContextUsageProvider::new(store);
        assert_eq!(provider.live_pct("no-such-chat").unwrap(), None);
    }

    #[test]
    fn live_pct_reads_the_doc_stamped_context_usage_from_a_bare_snapshot() {
        let store = Arc::new(DocsStore::open(tempfile::tempdir().unwrap().path()).unwrap());
        let chat_id = "chat-1";
        let doc = SessionDoc::init(chat_id).unwrap();
        doc.update_context_usage(Some(60_000), Some(1_000_000))
            .unwrap();
        let bytes = doc.export_snapshot().unwrap();
        store
            .save_snapshot_with_cursor(chat_id, &bytes, 0, crate::chat2_host::CHAT2_DOC_EPOCH)
            .unwrap();

        let provider = ContextUsageProvider::new(store);
        // Fixed 200k divisor, NOT the doc's own recorded 1_000_000 window —
        // the whole point of not reusing `ContextUsage::fraction()`.
        assert_eq!(
            provider.live_pct(chat_id).unwrap(),
            Some(pct_from_tokens(60_000))
        );
    }

    #[test]
    fn live_pct_is_none_when_the_doc_has_no_context_usage_stamp() {
        let store = Arc::new(DocsStore::open(tempfile::tempdir().unwrap().path()).unwrap());
        let chat_id = "chat-2";
        let doc = SessionDoc::init(chat_id).unwrap();
        let bytes = doc.export_snapshot().unwrap();
        store
            .save_snapshot_with_cursor(chat_id, &bytes, 0, crate::chat2_host::CHAT2_DOC_EPOCH)
            .unwrap();

        let provider = ContextUsageProvider::new(store);
        assert_eq!(provider.live_pct(chat_id).unwrap(), None);
    }
}
