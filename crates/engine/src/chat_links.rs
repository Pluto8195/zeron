//! Pure text-mining helpers for durable PR/ticket links (ticket 0xx) — shared
//! by the live per-event tap (`sessions.rs`'s `drive_run`), the transcript
//! backfill repair (`external_import.rs::repair_missing_links`), and the
//! ticket-from-PR stamp (`rpc.rs`'s `CHAT_LINK_STATUS` handler).
//!
//! Hand-rolled, no `regex` crate — same precedent as
//! `pr_ticket_cache::extract_ticket_id`, which this module reuses for the
//! ticket side rather than re-deriving the same pattern.
//!
//! Live-tap AND backfill caveat: `AgentEvent::ToolResult.output` is populated
//! for ACP-driven harnesses (cursor, devin, grok, hermes, pi, antigravity)
//! but deliberately `None` for claude/codex (see the field's own doc comment
//! in `crates/proto/src/agent.rs`) — those two adapters never surface a
//! tool's raw output on the live wire, and `external_import.rs::
//! parse_transcript` (the ONLY backfill source, Claude Code transcripts)
//! doesn't capture tool-result text either (its `RawBlock` has no field for
//! it). So for a Claude Code chat — live or backfilled — a `gh pr create`
//! result's PR URL only becomes visible once the agent narrates it back in
//! its own text reply, which lands as `mentioned` (via `note_message`'s
//! mining / the `Text` branch of `mine_links_from_entries`) rather than
//! `created_in_chat`. The `ToolResult`/`Tool.output` paths here still exist
//! for the harnesses that DO populate it live, and stay correct if a future
//! transcript source ever does too.

use crate::workspace_host::{ChatLinkKind, WorkspaceHost};
use zeron_proto::{ChatLinkSource, ToolCall};

/// A `github.com/<org>/<repo>/pull/<n>` URL appearing anywhere in `text` —
/// matches the design's own vocabulary ("a `github.com/<org>/<repo>/pull/<n>`
/// URL appearing in a tool result..."). The scheme is included in the
/// returned string when the source text had one immediately before the host
/// (`https://github.com/...`); a bare `github.com/...` mention is returned
/// exactly as it appeared, unprefixed. `None` when no PR URL appears at all.
pub(crate) fn extract_pr_url(text: &str) -> Option<String> {
    const HOST: &str = "github.com/";
    let mut search_from = 0usize;
    while let Some(rel) = text[search_from..].find(HOST) {
        let host_start = search_from + rel;
        let rest = &text[host_start + HOST.len()..];
        if let Some(len) = pull_path_len(rest) {
            let scheme_len = ["https://", "http://"]
                .iter()
                .find(|s| text[..host_start].ends_with(**s))
                .map_or(0, |s| s.len());
            let start = host_start - scheme_len;
            let end = host_start + HOST.len() + len;
            return Some(text[start..end].to_string());
        }
        search_from = host_start + HOST.len();
    }
    None
}

/// `<owner>/<repo>/pull/<digits>` immediately at the start of `rest` — the
/// byte length consumed, or `None` if `rest` doesn't continue that shape.
fn pull_path_len(rest: &str) -> Option<usize> {
    let is_seg = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    let owner_end = rest.find('/').filter(|&i| i > 0 && rest[..i].chars().all(is_seg))?;
    let after_owner = &rest[owner_end + 1..];
    let repo_end = after_owner
        .find('/')
        .filter(|&i| i > 0 && after_owner[..i].chars().all(is_seg))?;
    let after_repo = &after_owner[repo_end + 1..];
    let digits = after_repo.strip_prefix("pull/")?;
    let digit_len = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    (digit_len > 0).then_some(owner_end + 1 + repo_end + 1 + "pull/".len() + digit_len)
}

/// Heuristic PR-creation detector: substring match on `gh pr create` in a
/// tool invocation's command/input text — a heuristic per the design, not a
/// full shell-argv parse (matches e.g. `gh pr create --title …` and a
/// quoted/piped variant just as well; a comment or string that happens to
/// contain the same words is an accepted false-positive-on-purpose, same
/// spirit as `is_bot_login`'s substring checks in `pr_ticket_cache.rs`).
pub(crate) fn looks_like_pr_create(command_or_input: &str) -> bool {
    command_or_input.contains("gh pr create")
}

/// The text a [`ToolCall`] invocation is judged by — the shell command for
/// `Exec`, or the tool name plus its raw JSON input for anything MCP/unknown
/// shaped (cursor and others route `gh` through an MCP-wrapped exec tool
/// rather than a native `Exec` call).
pub(crate) fn tool_call_invocation_text(call: &ToolCall) -> String {
    match call {
        ToolCall::Exec { command } => command.clone(),
        ToolCall::Unknown { name, input } | ToolCall::Mcp { tool: name, input, .. } => {
            let mut text = name.clone();
            if let Some(input) = input {
                text.push(' ');
                text.push_str(&input.to_string());
            }
            text
        }
        _ => String::new(),
    }
}

/// Ticket-id mention, delegating to the existing branch-name extractor's
/// `[A-Za-z]{2,6}-\d+` pattern — the same regex, just run over free text
/// instead of a branch name.
pub(crate) fn extract_ticket_mention(text: &str) -> Option<String> {
    crate::pr_ticket_cache::extract_ticket_id(text)
}

/// Stamp a `mentioned` PR link from free text (a message or a tool result),
/// most-recent-mention-wins (write-time precedence: `mentioned` never
/// overwrites `manual`/`created_in_chat`, but DOES overwrite an older
/// `mentioned` value — same source, later write, per
/// `ChatLinkSource::can_overwrite`). Best-effort: a missing chat row or a
/// precedence loss both no-op silently, matching every other workspace write
/// this engine makes off the hot path.
pub(crate) fn mine_pr_mention(workspace: &WorkspaceHost, chat_id: &str, text: &str) {
    if let Some(url) = extract_pr_url(text)
        && let Err(err) =
            workspace.set_chat_link(chat_id, ChatLinkKind::Pr, Some(&url), ChatLinkSource::Mentioned)
    {
        tracing::warn!(chat = %chat_id, error = %err, "PR mention link write failed");
    }
}

/// Sibling to [`mine_pr_mention`] for ticket ids.
pub(crate) fn mine_ticket_mention(workspace: &WorkspaceHost, chat_id: &str, text: &str) {
    if let Some(id) = extract_ticket_mention(text)
        && let Err(err) = workspace.set_chat_link(
            chat_id,
            ChatLinkKind::Ticket,
            Some(&id),
            ChatLinkSource::Mentioned,
        )
    {
        tracing::warn!(chat = %chat_id, error = %err, "ticket mention link write failed");
    }
}

/// Stamp a `created_in_chat` PR link — the engine watched THIS chat run
/// `gh pr create` and mint the PR. Best-effort, same contract as
/// [`mine_pr_mention`].
pub(crate) fn mine_pr_created(workspace: &WorkspaceHost, chat_id: &str, url: &str) {
    if let Err(err) = workspace.set_chat_link(
        chat_id,
        ChatLinkKind::Pr,
        Some(url),
        ChatLinkSource::CreatedInChat,
    ) {
        tracing::warn!(chat = %chat_id, error = %err, "PR created-in-chat link write failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_pr_url_finds_a_scheme_qualified_mention() {
        let text = "Created pull request: https://github.com/acme/widgets/pull/42\nDone.";
        assert_eq!(
            extract_pr_url(text),
            Some("https://github.com/acme/widgets/pull/42".to_string())
        );
    }

    #[test]
    fn extract_pr_url_finds_a_bare_host_mention_unprefixed() {
        assert_eq!(
            extract_pr_url("see github.com/acme/widgets/pull/7 for details"),
            Some("github.com/acme/widgets/pull/7".to_string())
        );
    }

    #[test]
    fn extract_pr_url_ignores_non_pull_github_urls() {
        assert_eq!(extract_pr_url("https://github.com/acme/widgets/issues/7"), None);
        assert_eq!(extract_pr_url("https://github.com/acme/widgets"), None);
        assert_eq!(extract_pr_url("no url here"), None);
    }

    #[test]
    fn extract_pr_url_stops_at_trailing_punctuation_and_path() {
        // A trailing `/files` (the "Files changed" tab) must not get folded
        // into the digits, and a `.` following the number must not either.
        assert_eq!(
            extract_pr_url("https://github.com/acme/widgets/pull/42/files"),
            Some("https://github.com/acme/widgets/pull/42".to_string())
        );
        assert_eq!(
            extract_pr_url("(https://github.com/acme/widgets/pull/42)."),
            Some("https://github.com/acme/widgets/pull/42".to_string())
        );
    }

    #[test]
    fn looks_like_pr_create_matches_the_gh_invocation() {
        assert!(looks_like_pr_create("gh pr create --title 'Fix thing' --body '…'"));
        assert!(!looks_like_pr_create("gh pr view 42"));
        assert!(!looks_like_pr_create("gh pr merge 42"));
    }

    #[test]
    fn tool_call_invocation_text_covers_exec_and_mcp_shapes() {
        assert_eq!(
            tool_call_invocation_text(&ToolCall::Exec {
                command: "gh pr create --title x".into()
            }),
            "gh pr create --title x"
        );
        let mcp = ToolCall::Mcp {
            server: "github".into(),
            tool: "gh".into(),
            input: Some(serde_json::json!({"args": ["pr", "create"]})),
        };
        assert!(tool_call_invocation_text(&mcp).contains("pr"));
        assert_eq!(tool_call_invocation_text(&ToolCall::Glob { pattern: "*.rs".into() }), "");
    }

    #[test]
    fn extract_ticket_mention_delegates_to_the_shared_regex() {
        assert_eq!(extract_ticket_mention("see ENG-2715 for context"), Some("ENG-2715".into()));
        assert_eq!(extract_ticket_mention("nothing here"), None);
    }
}
