//! Import-time title resolution: `ExternalSessionImporter::import` used to
//! title a new chat only when the transcript happened to carry an `ai-title`
//! record (`None` otherwise, rendered "New session" by every UI surface —
//! see `crates/engine/src/external_import.rs`'s `repair_missing_titles` doc
//! comment for the full inventory of where that fallback lives). This file
//! covers the priority chain that replaced that single-source behavior,
//! ported from `agent-mode-tools/session_canvas_server.py`:
//!   1. agent-mode.sh's own task-name registry (`~/.config/agent-mode/
//!      session-ids/*.session-id`), keyed by session id — beats everything.
//!   2. the transcript's own `ai-title` record.
//!   3. the first real (non-synthetic) user message, truncated.
//!
//! Same `$HOME`-redirection convention as `external_import_bulk_session_canvas.rs`
//! (the registry lives under `$HOME`) — kept as one test function so a
//! sibling test in this crate can never observe a mid-flight `$HOME` swap
//! (`std::env::set_var` is process-global).

use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

fn transcript_with_lines(lines: Vec<serde_json::Value>) -> String {
    lines
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn user_line(session_id: &str, uuid: &str, cwd: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "parentUuid": null, "isSidechain": false, "type": "user",
        "message": {"role": "user", "content": text},
        "uuid": uuid, "timestamp": "2026-01-01T00:00:01.000Z",
        "cwd": cwd, "sessionId": session_id,
    })
}

fn assistant_ack_line(session_id: &str, uuid: &str, parent: &str, cwd: &str) -> serde_json::Value {
    serde_json::json!({
        "parentUuid": parent, "isSidechain": false, "type": "assistant",
        "message": {"model": "claude-x", "content": [{"type": "text", "text": "ack"}]},
        "uuid": uuid, "timestamp": "2026-01-01T00:00:02.000Z",
        "cwd": cwd, "sessionId": session_id,
    })
}

fn ai_title_line(session_id: &str, title: &str) -> serde_json::Value {
    serde_json::json!({"type": "ai-title", "aiTitle": title, "sessionId": session_id})
}

const TITLEGEN_MESSAGE: &str =
    "Reply with ONLY a concise title (under 8 words) for this coding-agent chat, no punctuation.";
const CLASSIFY_MESSAGE: &str =
    "Classify this coding-agent chat into exactly one task category from the list below.";

#[tokio::test]
async fn priority_chain_registry_beats_ai_title_beats_first_message() {
    let fake_home = tempfile::tempdir().expect("fake home tempdir");
    let previous_home = std::env::var_os("HOME");
    // SAFETY: test-only, scoped to this one function, restored at the end —
    // same justification as `external_import_bulk_session_canvas.rs`.
    unsafe {
        std::env::set_var("HOME", fake_home.path());
    }

    let registry_dir = fake_home
        .path()
        .join(".config")
        .join("agent-mode")
        .join("session-ids");
    std::fs::create_dir_all(&registry_dir).expect("registry dir");
    std::fs::write(
        registry_dir.join("review-growthbook-wrapper.session-id"),
        "sess-registry\n",
    )
    .expect("write registry entry");

    let data_dir = tempfile::tempdir().expect("data dir");
    let core = assemble(EngineProfile::development(
        data_dir.path(),
        "dev-org",
        "dev-user",
    ));
    let transcripts_dir = tempfile::tempdir().expect("transcripts dir");

    // (1) A session with BOTH a registry entry and an ai-title: the registry
    // name wins outright.
    {
        let path = transcripts_dir.path().join("registry.jsonl");
        std::fs::write(
            &path,
            transcript_with_lines(vec![
                user_line(
                    "sess-registry",
                    "u1",
                    "/work/one",
                    "totally unrelated pasted ticket text",
                ),
                assistant_ack_line("sess-registry", "a1", "u1", "/work/one"),
                ai_title_line("sess-registry", "An AI-generated title nobody asked for"),
            ]),
        )
        .expect("write transcript");
        let result = core
            .external_import
            .import("chat-registry", "sess-registry", &path)
            .expect("import");
        assert_eq!(result.title.as_deref(), Some("growthbook-wrapper"));
        let chat = core
            .workspace
            .chat("chat-registry")
            .expect("read chat")
            .expect("exists");
        assert_eq!(chat.title.as_deref(), Some("growthbook-wrapper"));
    }

    // (2) No registry entry, but a real `ai-title` record: the ai-title wins
    // over the first message.
    {
        let path = transcripts_dir.path().join("ai-title.jsonl");
        std::fs::write(
            &path,
            transcript_with_lines(vec![
                user_line(
                    "sess-ai-title",
                    "u1",
                    "/work/two",
                    "add a health check endpoint",
                ),
                assistant_ack_line("sess-ai-title", "a1", "u1", "/work/two"),
                ai_title_line("sess-ai-title", "Add health check endpoint"),
            ]),
        )
        .expect("write transcript");
        let result = core
            .external_import
            .import("chat-ai-title", "sess-ai-title", &path)
            .expect("import");
        assert_eq!(result.title.as_deref(), Some("Add health check endpoint"));
    }

    // (3) Neither a registry entry nor an ai-title: falls back to the first
    // real user message, truncated.
    {
        let path = transcripts_dir.path().join("first-message.jsonl");
        std::fs::write(
            &path,
            transcript_with_lines(vec![
                user_line(
                    "sess-first-msg",
                    "u1",
                    "/work/three",
                    "fix the flaky retry test",
                ),
                assistant_ack_line("sess-first-msg", "a1", "u1", "/work/three"),
            ]),
        )
        .expect("write transcript");
        let result = core
            .external_import
            .import("chat-first-msg", "sess-first-msg", &path)
            .expect("import");
        assert_eq!(result.title.as_deref(), Some("fix the flaky retry test"));
    }

    // (4) A first message wrapped in `<pasted_content id="...">`: the wrapper
    // tag is stripped and the readable text underneath becomes the title.
    {
        let path = transcripts_dir.path().join("pasted-content.jsonl");
        std::fs::write(
            &path,
            transcript_with_lines(vec![
                user_line(
                    "sess-pasted",
                    "u1",
                    "/work/four",
                    "\n\n<pasted_content id=\"f73f\">\n\nYou're picking up implementation work on a fork of Zeron",
                ),
                assistant_ack_line("sess-pasted", "a1", "u1", "/work/four"),
            ]),
        )
        .expect("write transcript");
        let result = core
            .external_import
            .import("chat-pasted", "sess-pasted", &path)
            .expect("import");
        let title = result.title.expect("title resolved");
        assert!(
            !title.contains("pasted_content"),
            "wrapper tag must be stripped: {title}"
        );
        assert!(
            title.starts_with("You're picking up implementation work"),
            "got: {title}"
        );
    }

    // (5) A synthetic CLASSIFY prompt as the first `user` turn must be
    // skipped in favor of the next, real user message.
    {
        let path = transcripts_dir.path().join("synthetic-first.jsonl");
        std::fs::write(
            &path,
            transcript_with_lines(vec![
                user_line("sess-synthetic", "u1", "/work/five", CLASSIFY_MESSAGE),
                assistant_ack_line("sess-synthetic", "a1", "u1", "/work/five"),
                user_line("sess-synthetic", "u2", "/work/five", TITLEGEN_MESSAGE),
                assistant_ack_line("sess-synthetic", "a2", "u2", "/work/five"),
                user_line(
                    "sess-synthetic",
                    "u3",
                    "/work/five",
                    "the actual real first message",
                ),
                assistant_ack_line("sess-synthetic", "a3", "u3", "/work/five"),
            ]),
        )
        .expect("write transcript");
        let result = core
            .external_import
            .import("chat-synthetic", "sess-synthetic", &path)
            .expect("import");
        assert_eq!(
            result.title.as_deref(),
            Some("the actual real first message")
        );
    }

    // (6) A long first message is truncated to one line, ~60 chars, with an
    // ellipsis — never a raw multi-hundred-char dump as a chat title.
    {
        let path = transcripts_dir.path().join("long-message.jsonl");
        let long_text = "this first message goes on for a very long time well past the sixty character mark that titles are truncated at\nand a second line that must never appear in the title";
        std::fs::write(
            &path,
            transcript_with_lines(vec![
                user_line("sess-long", "u1", "/work/six", long_text),
                assistant_ack_line("sess-long", "a1", "u1", "/work/six"),
            ]),
        )
        .expect("write transcript");
        let result = core
            .external_import
            .import("chat-long", "sess-long", &path)
            .expect("import");
        let title = result.title.expect("title resolved");
        assert!(
            title.chars().count() <= 61,
            "title too long: {title:?} ({} chars)",
            title.chars().count()
        );
        assert!(title.ends_with('\u{2026}'));
        assert!(
            !title.contains('\n'),
            "title must be single-line: {title:?}"
        );
    }

    core.shutdown().await;

    // SAFETY: restoring whatever HOME was before this test touched it.
    unsafe {
        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
