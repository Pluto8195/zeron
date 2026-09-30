//! Category/origin classification, ported from `agent-mode-tools/
//! session_canvas_server.py`'s `classify_heuristic`/`classify_origin` —
//! verifies the Rust port's heuristic buckets and origin detection (via the
//! peon-hook signal and `entrypoint`) against real on-disk transcript shapes.

use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

async fn import_transcript(lines: &[&str]) -> zeron_engine::external_import::ImportedSession {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));
    let transcript_path = dir.path().join("external-session.jsonl");
    std::fs::write(&transcript_path, lines.join("\n") + "\n").expect("write transcript");
    let result = core
        .external_import
        .import("imported-chat", "session-id", &transcript_path)
        .expect("import");
    core.shutdown().await;
    result
}

fn user_line(uuid: &str, text: &str, entrypoint: Option<&str>) -> String {
    let ep = entrypoint
        .map(|e| format!(r#","entrypoint":"{e}""#))
        .unwrap_or_default();
    format!(
        r#"{{"parentUuid":null,"isSidechain":false,"type":"user","message":{{"role":"user","content":"{text}"}},"uuid":"{uuid}","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/work/project","sessionId":"sess-1"{ep}}}"#
    )
}

fn assistant_line(uuid: &str, tool_calls: &[(&str, &str)]) -> String {
    let mut content = vec![r#"{"type":"text","text":"ok"}"#.to_string()];
    for (i, (name, input)) in tool_calls.iter().enumerate() {
        content.push(format!(
            r#"{{"type":"tool_use","id":"tool-{i}","name":"{name}","input":{input}}}"#
        ));
    }
    format!(
        r#"{{"parentUuid":null,"isSidechain":false,"type":"assistant","message":{{"model":"claude-x","content":[{content}]}},"uuid":"{uuid}","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/work/project","sessionId":"sess-1"}}"#,
        content = content.join(",")
    )
}

const PEON_HOOK_LINE: &str = r#"{"type":"system","subtype":"stop_hook_summary","hookInfos":[{"command":"python3 ~/bin/peon_hook.py --agent claude --state done","durationMs":100}],"cwd":"/work/project","sessionId":"sess-1"}"#;

#[tokio::test]
async fn short_low_tool_transcript_is_quick_question() {
    let lines = vec![
        user_line("u1", "what does this function do", None),
        assistant_line("a1", &[]),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let result = import_transcript(&lines).await;
    assert_eq!(result.category, "quick_question");
}

#[tokio::test]
async fn three_or_more_edits_is_implementing() {
    let lines = vec![
        user_line("u1", "add a feature", None),
        assistant_line(
            "a1",
            &[
                ("Edit", r#"{"file_path":"a.rs"}"#),
                ("Edit", r#"{"file_path":"b.rs"}"#),
                ("Edit", r#"{"file_path":"c.rs"}"#),
            ],
        ),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let result = import_transcript(&lines).await;
    assert_eq!(result.category, "implementing");
}

#[tokio::test]
async fn reads_with_no_edits_is_research() {
    let lines = vec![
        user_line("u1", "how does auth work here", None),
        assistant_line(
            "a1",
            &[
                ("Read", r#"{"file_path":"a.rs"}"#),
                ("Grep", r#"{"pattern":"auth"}"#),
                ("Glob", r#"{"pattern":"**/*.rs"}"#),
            ],
        ),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let result = import_transcript(&lines).await;
    assert_eq!(result.category, "research");
}

#[tokio::test]
async fn a_review_skill_is_pr_review_even_with_reads() {
    let lines = vec![
        user_line("u1", "review this PR", None),
        assistant_line(
            "a1",
            &[
                ("Read", r#"{"file_path":"a.rs"}"#),
                ("Read", r#"{"file_path":"b.rs"}"#),
                ("Skill", r#"{"skill":"code-review"}"#),
            ],
        ),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let result = import_transcript(&lines).await;
    assert_eq!(result.category, "pr_review");
}

fn reads(n: usize) -> Vec<(&'static str, &'static str)> {
    vec![("Read", r#"{"file_path":"a.rs"}"#); n]
}

#[tokio::test]
async fn sentry_skill_is_debug_not_research() {
    let lines = vec![
        user_line("u1", "look at the latest issues", None),
        assistant_line("a1", &[("Skill", r#"{"skill":"sentry-api"}"#), ("Read", r#"{"file_path":"a.rs"}"#), ("Read", r#"{"file_path":"b.rs"}"#)]),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    assert_eq!(import_transcript(&lines).await.category, "debug");
}

#[tokio::test]
async fn a_bug_hunt_opener_is_debug_even_when_it_ends_in_edits() {
    let mut edits = reads(1);
    edits.extend([("Edit", r#"{"file_path":"a.rs"}"#); 3]);
    let lines = vec![
        user_line("u1", "checkout is broken in prod, find the root cause", None),
        assistant_line("a1", &edits),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    assert_eq!(import_transcript(&lines).await.category, "debug");
}

#[tokio::test]
async fn failure_words_need_context_and_a_review_skill_still_wins() {
    // "error" alone is feature work, not a firefight.
    let feature = vec![
        user_line("u1", "add error handling to the importer", None),
        assistant_line("a1", &[("Edit", r#"{"file_path":"a.rs"}"#); 3]),
    ];
    let feature: Vec<&str> = feature.iter().map(String::as_str).collect();
    assert_eq!(import_transcript(&feature).await.category, "implementing");

    let review = vec![
        user_line("u1", "review this PR for bugs", None),
        assistant_line("a1", &[("Read", r#"{"file_path":"a.rs"}"#), ("Read", r#"{"file_path":"b.rs"}"#), ("Skill", r#"{"skill":"code-review"}"#)]),
    ];
    let review: Vec<&str> = review.iter().map(String::as_str).collect();
    assert_eq!(import_transcript(&review).await.category, "pr_review");
}

#[tokio::test]
async fn a_short_debug_question_stays_quick_question() {
    let lines = vec![user_line("u1", "why is this crashing", None), assistant_line("a1", &[])];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    assert_eq!(import_transcript(&lines).await.category, "quick_question");
}

#[tokio::test]
async fn bash_only_activity_falls_to_other() {
    let lines = vec![
        user_line("u1", "run the build a few times", None),
        assistant_line(
            "a1",
            &[
                ("Bash", r#"{"command":"make"}"#),
                ("Bash", r#"{"command":"make test"}"#),
                ("Bash", r#"{"command":"make lint"}"#),
            ],
        ),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let result = import_transcript(&lines).await;
    assert_eq!(result.category, "other");
}

#[tokio::test]
async fn peon_hook_signal_means_agent_mode_origin_regardless_of_entrypoint() {
    let lines = vec![
        user_line("u1", "hello", Some("sdk-cli")),
        assistant_line("a1", &[]),
        PEON_HOOK_LINE.to_string(),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let result = import_transcript(&lines).await;
    assert_eq!(result.origin, "agent_mode");
}

#[tokio::test]
async fn sdk_cli_entrypoint_without_peon_hook_is_sdk_driven() {
    let lines = vec![
        user_line("u1", "hello", Some("sdk-cli")),
        assistant_line("a1", &[]),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let result = import_transcript(&lines).await;
    assert_eq!(result.origin, "sdk_driven");
}

#[tokio::test]
async fn claude_desktop_entrypoint_maps_correctly() {
    let lines = vec![
        user_line("u1", "hello", Some("claude-desktop")),
        assistant_line("a1", &[]),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let result = import_transcript(&lines).await;
    assert_eq!(result.origin, "claude_desktop");
}

#[tokio::test]
async fn bare_cli_entrypoint_maps_correctly() {
    let lines = vec![
        user_line("u1", "hello", Some("cli")),
        assistant_line("a1", &[]),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let result = import_transcript(&lines).await;
    assert_eq!(result.origin, "bare_cli");
}

#[tokio::test]
async fn no_entrypoint_and_no_hook_is_unknown_origin() {
    let lines = vec![
        user_line("u1", "hello", None),
        assistant_line("a1", &[]),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let result = import_transcript(&lines).await;
    assert_eq!(result.origin, "unknown");
}

#[tokio::test]
async fn classification_for_matches_what_import_returned() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));
    let transcript_path = dir.path().join("external-session.jsonl");
    let lines = vec![
        user_line("u1", "add a feature", None),
        assistant_line(
            "a1",
            &[
                ("Edit", r#"{"file_path":"a.rs"}"#),
                ("Edit", r#"{"file_path":"b.rs"}"#),
                ("Edit", r#"{"file_path":"c.rs"}"#),
            ],
        ),
        PEON_HOOK_LINE.to_string(),
    ];
    std::fs::write(&transcript_path, lines.join("\n") + "\n").expect("write transcript");

    let imported = core
        .external_import
        .import("imported-chat", "session-id", &transcript_path)
        .expect("import");
    assert_eq!(imported.category, "implementing");
    assert_eq!(imported.origin, "agent_mode");

    let (category, origin) = core
        .external_import
        .classification_for("imported-chat")
        .expect("classification_for")
        .expect("classification present");
    assert_eq!(category, "implementing");
    assert_eq!(origin, "agent_mode");

    // A chat that never went through import has no classification.
    assert!(
        core.external_import
            .classification_for("never-imported")
            .expect("classification_for")
            .is_none()
    );

    core.shutdown().await;
}

#[tokio::test]
async fn tool_usage_for_returns_the_real_tally_and_loaded_skills() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));
    let transcript_path = dir.path().join("external-session.jsonl");
    let lines = vec![
        user_line("u1", "add a feature", None),
        assistant_line(
            "a1",
            &[
                ("Edit", r#"{"file_path":"a.rs"}"#),
                ("Edit", r#"{"file_path":"b.rs"}"#),
                ("Read", r#"{"file_path":"c.rs"}"#),
                ("Skill", r#"{"skill":"code-review"}"#),
            ],
        ),
    ];
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    std::fs::write(&transcript_path, lines.join("\n") + "\n").expect("write transcript");

    core.external_import
        .import("imported-chat", "session-id", &transcript_path)
        .expect("import");

    let (tool_counts, skills_loaded) = core
        .external_import
        .tool_usage_for("imported-chat")
        .expect("tool_usage_for")
        .expect("tool usage present");
    assert_eq!(tool_counts.get("Edit"), Some(&2));
    assert_eq!(tool_counts.get("Read"), Some(&1));
    assert_eq!(skills_loaded, vec!["code-review".to_string()]);

    // A chat that never went through import has no tool usage.
    assert!(
        core.external_import
            .tool_usage_for("never-imported")
            .expect("tool_usage_for")
            .is_none()
    );

    core.shutdown().await;
}
