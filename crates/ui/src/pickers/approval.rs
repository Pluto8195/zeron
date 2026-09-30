//! The composer's per-chat approval chip: `Ask` / `Auto`.
//!
//! Persisted on the chat row as `ChatConfig.autoApprove` (default `false` =
//! Ask) and resolved into `RunRequest.auto_approve` on every send. Ask means
//! the harness surfaces permission prompts in the UI; Auto means the agent
//! never asks (full access), so it renders in the theme's warning accent.
//! A click flips it: on an existing chat that has a config it writes
//! `Mutate setChatAutoApprove` (patches just that field); on a config-less
//! chat it writes a full `setChatConfig` the way the harness/model pickers
//! do; on the new-chat canvas it's a draft carried onto `createChat`.
//!
//! Pure logic only — the chip view lives in `Pickers`.

use zeron_proto::ChatConfig;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ApprovalMode {
    /// The agent requests approval for risky actions (the default).
    #[default]
    Ask,
    /// The agent never asks — full access.
    Auto,
}

impl ApprovalMode {
    pub fn from_auto_approve(auto_approve: bool) -> Self {
        if auto_approve { Self::Auto } else { Self::Ask }
    }

    pub fn auto_approve(self) -> bool {
        self == Self::Auto
    }

    /// Two states: a click always flips.
    pub fn next(self) -> Self {
        match self {
            Self::Ask => Self::Auto,
            Self::Auto => Self::Ask,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask",
            Self::Auto => "Auto",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Ask => "Ask: the agent requests approval for risky actions",
            Self::Auto => "Auto-approve: the agent never asks \u{2014} full access",
        }
    }
}

/// The approval flag the next send uses (and the chip shows).
///
/// - New-chat canvas (`selected_chat: None`): the draft pick.
/// - Selected chat with a config row: the row's persisted value.
/// - Selected chat whose row hasn't synced a config yet: the value carried
///   over from the new-chat draft that minted THIS chat (so the moment
///   between send and the row landing doesn't flash back to Ask), else Ask.
pub fn effective_auto_approve(
    selected_chat: Option<&str>,
    row_config: Option<&ChatConfig>,
    carried: Option<(&str, bool)>,
    draft: bool,
) -> bool {
    let Some(chat_id) = selected_chat else {
        return draft;
    };
    if let Some(config) = row_config {
        return config.auto_approve;
    }
    carried
        .filter(|(carried_id, _)| *carried_id == chat_id)
        .is_some_and(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::{HarnessId, SandboxLevel};

    fn config(auto_approve: bool) -> ChatConfig {
        ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve,
        }
    }

    #[test]
    fn mode_defaults_to_ask_and_flips() {
        assert_eq!(ApprovalMode::default(), ApprovalMode::Ask);
        assert_eq!(ApprovalMode::Ask.next(), ApprovalMode::Auto);
        assert_eq!(ApprovalMode::Auto.next(), ApprovalMode::Ask);
        assert!(ApprovalMode::from_auto_approve(true).auto_approve());
        assert!(!ApprovalMode::from_auto_approve(false).auto_approve());
        assert_eq!(ApprovalMode::Ask.label(), "Ask");
        assert_eq!(ApprovalMode::Auto.label(), "Auto");
        assert!(ApprovalMode::Auto.tooltip().starts_with("Auto-approve:"));
    }

    #[test]
    fn effective_flag_prefers_the_row_then_the_carried_draft() {
        // New-chat canvas: the draft.
        assert!(effective_auto_approve(None, None, None, true));
        assert!(!effective_auto_approve(None, None, None, false));
        // A configured row wins over anything carried.
        let yolo = config(true);
        let ask = config(false);
        assert!(effective_auto_approve(Some("c"), Some(&yolo), None, false));
        assert!(!effective_auto_approve(
            Some("c"),
            Some(&ask),
            Some(("c", true)),
            true
        ));
        // Config-less row: only a draft carried onto THIS chat applies.
        assert!(effective_auto_approve(
            Some("c"),
            None,
            Some(("c", true)),
            false
        ));
        assert!(!effective_auto_approve(
            Some("c"),
            None,
            Some(("other", true)),
            false
        ));
        assert!(!effective_auto_approve(Some("c"), None, None, true));
    }
}
