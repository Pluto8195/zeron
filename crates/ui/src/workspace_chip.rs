//! The new-chat "worktree" chip: `Auto` / `Worktree` / `Main checkout`.
//!
//! Only shown on the blank new-session canvas, before the first send. On that
//! send the composer runs a short pre-dispatch step (see `Composer::send`):
//! `Auto` asks the engine whether the message wants an isolated worktree
//! (`PLAN_CHAT_WORKSPACE`, Jev or a heuristic), `Worktree` skips straight to
//! `CREATE_CHAT_WORKTREE`, `Main checkout` makes no calls. After send the
//! session footer shows the outcome as a small badge.
//!
//! Everything here except the tooltip view is pure, so the decision logic is
//! unit-testable without a gpui context.

use gpui::{Context, IntoElement, Render, SharedString, Window, div, prelude::*, px};
use serde_json::Value;

use crate::theme::Theme;

/// `{message, cwd}` → `{needsWorktree, probability, source}` (IPC-only).
pub use zeron_rpc::methods::PLAN_CHAT_WORKSPACE;
/// `{chatId, repoPath, name}` → `{worktreePath, branch}`; on success the
/// engine has already stamped the chat's cwd (IPC-only).
pub use zeron_rpc::methods::CREATE_CHAT_WORKTREE;

/// Classifier budget (the engine caps Jev at ~3s; headroom for IPC).
pub const PLAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// `git worktree add` is ~1s; generous headroom for a cold repo.
pub const CREATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The draft pick. Always `Auto` for a fresh new-chat canvas — an explicit
/// pick is one-shot and never persisted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkspaceChoice {
    #[default]
    Auto,
    Worktree,
    MainCheckout,
}

impl WorkspaceChoice {
    /// Click cycles Auto → Worktree → Main checkout → Auto.
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Worktree,
            Self::Worktree => Self::MainCheckout,
            Self::MainCheckout => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Worktree => "Worktree",
            Self::MainCheckout => "Main checkout",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Auto => {
                "Auto: decide from your first message whether this chat gets its own \
                 worktree (click to cycle)"
            }
            Self::Worktree => {
                "Worktree: always create an isolated worktree for this chat (click to cycle)"
            }
            Self::MainCheckout => {
                "Main checkout: run in the project's own folder, no worktree (click to cycle)"
            }
        }
    }

    /// The first pre-dispatch call this choice needs, if any.
    pub fn first_step(self) -> PreSendStep {
        match self {
            Self::Auto => PreSendStep::Plan,
            Self::Worktree => PreSendStep::Create,
            Self::MainCheckout => PreSendStep::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreSendStep {
    Plan,
    Create,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanSource {
    Jev,
    Heuristic,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanReply {
    pub needs_worktree: bool,
    pub probability: Option<f64>,
    pub source: PlanSource,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeReply {
    pub worktree_path: String,
    pub branch: String,
}

/// Decode a `PLAN_CHAT_WORKSPACE` reply. `needsWorktree` is required; an
/// unknown `source` reads as heuristic (only `"jev"` earns the probability
/// line in the tooltip).
pub fn decode_plan(value: &Value) -> Result<PlanReply, String> {
    let needs_worktree = value
        .get("needsWorktree")
        .and_then(Value::as_bool)
        .ok_or_else(|| "plan reply missing needsWorktree".to_string())?;
    let probability = value.get("probability").and_then(Value::as_f64);
    let source = match value.get("source").and_then(Value::as_str) {
        Some("jev") => PlanSource::Jev,
        _ => PlanSource::Heuristic,
    };
    Ok(PlanReply {
        needs_worktree,
        probability,
        source,
    })
}

/// Decode a `CREATE_CHAT_WORKTREE` reply; both fields required and non-empty.
pub fn decode_worktree(value: &Value) -> Result<WorktreeReply, String> {
    let field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("worktree reply missing {key}"))
    };
    Ok(WorktreeReply {
        worktree_path: field("worktreePath")?,
        branch: field("branch")?,
    })
}

/// After PLAN: create a worktree only on a successful `needsWorktree: true`.
/// A PLAN failure (timeout, old engine without the method) quietly falls
/// back to the main checkout — Auto never blocks a send on the classifier.
pub fn should_create_after_plan(plan: &Result<PlanReply, String>) -> bool {
    matches!(plan, Ok(PlanReply { needs_worktree: true, .. }))
}

/// What the send does with a `CREATE_CHAT_WORKTREE` result.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateResolution {
    /// `Some` = run the dispatch in the new worktree (the engine already
    /// stamped it; carrying it onto the Run request keeps the two in sync).
    pub cwd: Option<String>,
    pub outcome: WorkspaceOutcome,
    /// Inline composer error for a failed create. The message is still
    /// dispatched in the base checkout — never eaten.
    pub failure: Option<String>,
}

pub fn resolve_create(
    result: Result<Value, String>,
    plan: Option<PlanReply>,
) -> CreateResolution {
    match result.and_then(|value| decode_worktree(&value)) {
        Ok(reply) => CreateResolution {
            cwd: Some(reply.worktree_path),
            outcome: WorkspaceOutcome::Worktree {
                branch: reply.branch,
                plan,
            },
            failure: None,
        },
        Err(err) => CreateResolution {
            cwd: None,
            failure: Some(format!(
                "Couldn't create a worktree ({err}) — sent in the main checkout instead."
            )),
            outcome: WorkspaceOutcome::Main {
                plan,
                note: Some(format!("Worktree creation failed: {err}")),
                warning: true,
            },
        },
    }
}

/// Outcome after a PLAN that did NOT lead to a create.
pub fn main_after_plan(plan: Result<PlanReply, String>) -> WorkspaceOutcome {
    match plan {
        Ok(plan) => WorkspaceOutcome::Main {
            plan: Some(plan),
            note: None,
            warning: false,
        },
        Err(err) => WorkspaceOutcome::Main {
            plan: None,
            note: Some(format!("Couldn't decide ({err}); used the main checkout")),
            warning: false,
        },
    }
}

/// Per-chat workspace decision, shown in the session footer after send.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceOutcome {
    Deciding,
    Creating,
    Worktree {
        branch: String,
        plan: Option<PlanReply>,
    },
    Main {
        plan: Option<PlanReply>,
        note: Option<String>,
        warning: bool,
    },
}

impl WorkspaceOutcome {
    pub fn label(&self) -> String {
        match self {
            Self::Deciding => "Deciding…".to_string(),
            Self::Creating => "Creating worktree…".to_string(),
            Self::Worktree { branch, .. } => format!("worktree: {branch}"),
            Self::Main { .. } => "main checkout".to_string(),
        }
    }

    pub fn is_warning(&self) -> bool {
        matches!(self, Self::Main { warning: true, .. })
    }

    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Deciding | Self::Creating)
    }

    pub fn tooltip(&self) -> String {
        let decided = |plan: &Option<PlanReply>, target: &str| match plan {
            Some(PlanReply {
                source: PlanSource::Jev,
                probability: Some(p),
                ..
            }) => format!("Jev: {p:.2} → {target}"),
            Some(PlanReply {
                source: PlanSource::Jev,
                probability: None,
                ..
            }) => format!("Jev → {target}"),
            Some(PlanReply {
                source: PlanSource::Heuristic,
                ..
            }) => format!("Heuristic → {target}"),
            None => format!("Chosen: {target}"),
        };
        match self {
            Self::Deciding => "Deciding whether this chat needs its own worktree…".to_string(),
            Self::Creating => "Creating a worktree for this chat…".to_string(),
            Self::Worktree { plan, .. } => decided(plan, "worktree"),
            Self::Main { plan, note, .. } => match note {
                Some(note) => note.clone(),
                None => decided(plan, "main checkout"),
            },
        }
    }
}

/// `[A-Za-z]{2,6}-\d+` bounded by non-alphanumerics, uppercased — a port of
/// the engine's `pr_ticket_cache::extract_ticket_id` (no `regex` dep).
pub fn extract_ticket_id(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() {
            let start = i;
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
                j += 1;
            }
            let letters = j - start;
            if (2..=6).contains(&letters) && j < bytes.len() && bytes[j] == b'-' {
                let mut k = j + 1;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                let digits = k - (j + 1);
                let left_ok = start == 0 || !is_word(bytes[start - 1]);
                let right_ok = k >= bytes.len() || !is_word(bytes[k]);
                if digits > 0 && left_ok && right_ok {
                    return Some(text[start..k].to_ascii_uppercase());
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

/// Kebab slug of the first `words` ASCII-alphanumeric words, capped at 40
/// chars (on a word boundary). Empty input → `"chat"`.
pub fn slug_first_words(text: &str, words: usize) -> String {
    let mut out = String::new();
    for word in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(words)
    {
        let word = word.to_ascii_lowercase();
        let extra = if out.is_empty() { word.len() } else { word.len() + 1 };
        if !out.is_empty() && out.len() + extra > 40 {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&word[..word.len().min(40)]);
    }
    if out.is_empty() {
        "chat".to_string()
    } else {
        out
    }
}

/// The `name` sent to `CREATE_CHAT_WORKTREE`: a ticket id when the message
/// names one, else a slug of its first ~5 words.
pub fn worktree_name(message: &str) -> String {
    extract_ticket_id(message).unwrap_or_else(|| slug_first_words(message, 5))
}

/// Plain text tooltip for the chip and the outcome badge.
pub(crate) struct WorkspaceChipTooltip(pub SharedString);

impl Render for WorkspaceChipTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .max_w(px(280.0))
            .text_size(px(11.0))
            .text_color(theme.text)
            .child(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn choice_cycles_and_defaults_to_auto() {
        assert_eq!(WorkspaceChoice::default(), WorkspaceChoice::Auto);
        let c = WorkspaceChoice::Auto;
        assert_eq!(c.next(), WorkspaceChoice::Worktree);
        assert_eq!(c.next().next(), WorkspaceChoice::MainCheckout);
        assert_eq!(c.next().next().next(), WorkspaceChoice::Auto);
        assert_eq!(WorkspaceChoice::Auto.first_step(), PreSendStep::Plan);
        assert_eq!(WorkspaceChoice::Worktree.first_step(), PreSendStep::Create);
        assert_eq!(WorkspaceChoice::MainCheckout.first_step(), PreSendStep::None);
    }

    #[test]
    fn ticket_id_extraction() {
        assert_eq!(extract_ticket_id("fix eng-123 now"), Some("ENG-123".into()));
        assert_eq!(extract_ticket_id("ZER-9: crash"), Some("ZER-9".into()));
        assert_eq!(extract_ticket_id("abcdefg-12"), None); // 7 letters
        assert_eq!(extract_ticket_id("x1ab-12"), None); // left not bounded
        assert_eq!(extract_ticket_id("ab-12c"), None); // right not bounded
        assert_eq!(extract_ticket_id("no ticket here"), None);
    }

    #[test]
    fn slug_and_name() {
        assert_eq!(
            slug_first_words("Fix the flaky login test in CI please", 5),
            "fix-the-flaky-login-test"
        );
        assert_eq!(slug_first_words("  !!! ", 5), "chat");
        assert_eq!(slug_first_words("Héllo wörld", 5), "h-llo-w-rld");
        assert!(slug_first_words(&"supercalifragilistic ".repeat(5), 5).len() <= 40);
        assert_eq!(worktree_name("Please do ENG-42 today"), "ENG-42");
        assert_eq!(worktree_name("Add dark mode toggle"), "add-dark-mode-toggle");
    }

    #[test]
    fn decode_plan_reply() {
        let p = decode_plan(&json!({"needsWorktree": true, "probability": 0.87, "source": "jev"}))
            .unwrap();
        assert_eq!(
            p,
            PlanReply {
                needs_worktree: true,
                probability: Some(0.87),
                source: PlanSource::Jev
            }
        );
        let h = decode_plan(&json!({"needsWorktree": false, "probability": null, "source": "heuristic"}))
            .unwrap();
        assert_eq!(h.source, PlanSource::Heuristic);
        assert_eq!(h.probability, None);
        assert!(decode_plan(&json!({"probability": 0.5})).is_err());
    }

    #[test]
    fn decode_worktree_reply() {
        let w = decode_worktree(&json!({"worktreePath": "/r/.wt/x", "branch": "zeron/x"})).unwrap();
        assert_eq!(w.worktree_path, "/r/.wt/x");
        assert_eq!(w.branch, "zeron/x");
        assert!(decode_worktree(&json!({"branch": "b"})).is_err());
        assert!(decode_worktree(&json!({"worktreePath": "", "branch": "b"})).is_err());
    }

    #[test]
    fn plan_gates_create() {
        let yes = Ok(PlanReply {
            needs_worktree: true,
            probability: Some(0.9),
            source: PlanSource::Jev,
        });
        let no = Ok(PlanReply {
            needs_worktree: false,
            probability: Some(0.1),
            source: PlanSource::Jev,
        });
        assert!(should_create_after_plan(&yes));
        assert!(!should_create_after_plan(&no));
        assert!(!should_create_after_plan(&Err("unknown method".into())));
        let quiet = main_after_plan(Err("timeout".into()));
        assert!(!quiet.is_warning());
        assert_eq!(quiet.label(), "main checkout");
        assert_eq!(main_after_plan(no).tooltip(), "Jev: 0.10 → main checkout");
    }

    #[test]
    fn create_failure_falls_back_to_main_with_warning() {
        let r = resolve_create(Err("not a git repo".into()), None);
        assert_eq!(r.cwd, None);
        assert!(r.failure.as_deref().unwrap().contains("not a git repo"));
        assert!(r.outcome.is_warning());
        assert_eq!(r.outcome.label(), "main checkout");
        // A malformed success is treated as a failure too.
        let bad = resolve_create(Ok(json!({})), None);
        assert!(bad.failure.is_some());
        assert_eq!(bad.cwd, None);
    }

    #[test]
    fn create_success_carries_cwd_and_branch() {
        let plan = PlanReply {
            needs_worktree: true,
            probability: Some(0.87),
            source: PlanSource::Jev,
        };
        let r = resolve_create(
            Ok(json!({"worktreePath": "/wt", "branch": "zeron/eng-1"})),
            Some(plan),
        );
        assert_eq!(r.cwd.as_deref(), Some("/wt"));
        assert_eq!(r.failure, None);
        assert_eq!(r.outcome.label(), "worktree: zeron/eng-1");
        assert_eq!(r.outcome.tooltip(), "Jev: 0.87 → worktree");
        let explicit = resolve_create(Ok(json!({"worktreePath": "/wt", "branch": "b"})), None);
        assert_eq!(explicit.outcome.tooltip(), "Chosen: worktree");
    }
}
