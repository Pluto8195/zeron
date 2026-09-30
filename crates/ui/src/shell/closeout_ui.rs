//! The chat close-out confirmation dialog (see [`crate::chat_closeout`]).
//!
//! Opened from the sidebar chat menu's "Close out worktree…" and the
//! overview's expanded-tile detail. Fetches `PLAN_CHAT_CLOSEOUT`, then shows
//! one of: loading, live-chat notice, not-a-worktree notice, default-branch
//! notice, clean confirm (`force: false`) or forced confirm with warnings
//! (`force: true`). Engine errors from `CLOSE_CHAT_WORKTREE` render verbatim
//! inside the still-open dialog. On success the engine archives the chat;
//! the `WATCH_CHATS` stream carries that into `AppState`, which the overview
//! observes, so the tile leaves the canvas with no extra refresh call.

use super::*;

use crate::chat_closeout::{self as co, CloseoutPlan, CloseoutVerdict};

pub(super) enum CloseoutPhase {
    Loading,
    /// The plan call itself failed (engine error, timeout, bad reply).
    PlanFailed(String),
    Ready(CloseoutPlan),
}

pub(super) struct CloseoutDialog {
    chat_id: String,
    cwd: String,
    title: String,
    /// Stale-reply guard ([`co::reply_is_current`]): replies for an earlier
    /// open of the dialog are dropped.
    request_id: u64,
    phase: CloseoutPhase,
    /// A `CLOSE_CHAT_WORKTREE` is in flight (ignore repeat clicks).
    submitting: bool,
    /// The close call's error, verbatim; the dialog stays open.
    error: Option<String>,
}

impl Shell {
    fn closeout_request_id(&self) -> Option<u64> {
        self.closeout_dialog.as_ref().map(|d| d.request_id)
    }

    /// Open the close-out dialog for `chat_id` and fetch its plan.
    pub(crate) fn open_chat_closeout(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.close_chat_menu(cx);
        let chat = self.state.read(cx).chats.iter().find(|c| c.id == chat_id).cloned();
        let title = transcript::single_line(
            &chat
                .as_ref()
                .and_then(|c| c.title.clone())
                .unwrap_or_else(|| "New session".into()),
        );
        let cwd = chat.and_then(|c| c.cwd).unwrap_or_default();
        self.closeout_seq += 1;
        let request_id = self.closeout_seq;
        let engine = self.state.read(cx).engine().cloned();
        let phase = if cwd.trim().is_empty() {
            CloseoutPhase::PlanFailed("This chat has no folder to close out.".into())
        } else if engine.is_none() {
            CloseoutPhase::PlanFailed("Engine not connected".into())
        } else {
            CloseoutPhase::Loading
        };
        let loading = matches!(phase, CloseoutPhase::Loading);
        self.closeout_dialog = Some(CloseoutDialog {
            chat_id: chat_id.clone(),
            cwd: cwd.clone(),
            title,
            request_id,
            phase,
            submitting: false,
            error: None,
        });
        cx.notify();
        let (true, Some(engine)) = (loading, engine) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                co::PLAN_CHAT_CLOSEOUT,
                serde_json::json!({ "chatId": chat_id, "cwd": cwd }),
                co::PLAN_TIMEOUT,
            )
            .await
            .and_then(|value| co::decode_plan(&value));
            this.update(cx, |shell, cx| {
                if !co::reply_is_current(shell.closeout_request_id(), request_id) {
                    return;
                }
                if let Some(dialog) = shell.closeout_dialog.as_mut() {
                    dialog.phase = match result {
                        Ok(plan) => CloseoutPhase::Ready(plan),
                        Err(err) => {
                            tracing::warn!(chat = %dialog.chat_id, error = %err, "PlanChatCloseout failed");
                            CloseoutPhase::PlanFailed(err)
                        }
                    };
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn close_chat_closeout(&mut self, cx: &mut Context<Self>) {
        if self.closeout_dialog.take().is_some() {
            cx.notify();
        }
    }

    /// The proceed button: `CLOSE_CHAT_WORKTREE` with `force` from the
    /// plan's verdict. Success closes the dialog; failure shows the engine's
    /// message inside it.
    fn submit_chat_closeout(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(dialog) = self.closeout_dialog.as_mut() else {
            return;
        };
        if dialog.submitting {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            dialog.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        dialog.submitting = true;
        dialog.error = None;
        let request_id = dialog.request_id;
        let chat_id = dialog.chat_id.clone();
        let cwd = dialog.cwd.clone();
        let branch = match &dialog.phase {
            CloseoutPhase::Ready(plan) => plan.branch.clone(),
            _ => None,
        };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                co::CLOSE_CHAT_WORKTREE,
                serde_json::json!({ "chatId": chat_id, "cwd": cwd, "force": force }),
                co::CLOSE_TIMEOUT,
            )
            .await
            .and_then(|value| co::decode_outcome(&value));
            this.update(cx, |shell, cx| {
                let current = co::reply_is_current(shell.closeout_request_id(), request_id);
                match result {
                    Ok(outcome) => {
                        // Reported even if the dialog was dismissed mid-call.
                        let what = branch.map_or_else(|| "worktree".to_string(), |b| format!("worktree ({b})"));
                        let note = if outcome.archived {
                            format!("Closed out {what}; chat archived")
                        } else {
                            format!("Closed out {what}")
                        };
                        shell.sidebar_notice = Some(note.into());
                        if current {
                            shell.closeout_dialog = None;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(chat = %chat_id, error = %err, "CloseChatWorktree failed");
                        if current {
                            if let Some(dialog) = shell.closeout_dialog.as_mut() {
                                dialog.submitting = false;
                                dialog.error = Some(err);
                            }
                        } else {
                            shell.sidebar_notice = Some(format!("Close out failed: {err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn render_closeout_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.closeout_dialog.as_ref()?;
        let theme = Theme::of(cx).for_popup();
        let code_family = crate::typography::code_effective_family_name(cx);
        let small = |text: String, color: gpui::Hsla| {
            div()
                .text_size(crate::typography::ui_rems(12.0))
                .line_height(px(17.0))
                .text_color(color)
                .child(SharedString::from(text))
        };
        let mono = |text: String, color: gpui::Hsla| {
            small(text, color).font_family(code_family.clone()).overflow_hidden().text_ellipsis()
        };

        let verdict = match &dialog.phase {
            CloseoutPhase::Ready(plan) => Some(co::verdict(plan)),
            _ => None,
        };
        let title = match verdict {
            Some(CloseoutVerdict::Live | CloseoutVerdict::NotWorktree | CloseoutVerdict::OnDefaultBranch) => {
                "Can\u{2019}t close out worktree"
            }
            Some(CloseoutVerdict::NeedsForce) => "Force close out worktree?",
            _ => "Close out worktree?",
        };

        let mut body: Vec<AnyElement> = vec![
            small(format!("\u{201C}{}\u{201D}", dialog.title), theme.text_muted).into_any_element(),
        ];
        match &dialog.phase {
            CloseoutPhase::Loading => {
                body.push(popover::dialog_body(&theme, "Checking the worktree\u{2026}").into_any_element());
            }
            CloseoutPhase::PlanFailed(err) => {
                body.push(
                    popover::dialog_body(&theme, "Couldn\u{2019}t inspect this chat\u{2019}s worktree.")
                        .into_any_element(),
                );
                body.push(small(err.clone(), theme.danger).into_any_element());
            }
            CloseoutPhase::Ready(plan) => {
                let verdict = co::verdict(plan);
                if plan.is_worktree && !plan.worktree_path.is_empty() {
                    body.push(mono(co::summary_line(plan), theme.text).into_any_element());
                }
                body.push(popover::dialog_body(&theme, co::body_copy(plan)).into_any_element());
                if verdict == CloseoutVerdict::NeedsForce {
                    let mut warnings = div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .p(px(10.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.danger.opacity(0.35))
                        .bg(theme.danger.opacity(0.08));
                    if plan.dirty {
                        warnings = warnings.child(small(co::dirty_count_label(plan), theme.danger));
                        for line in co::dirty_file_lines(plan) {
                            warnings = warnings.child(div().pl(px(10.0)).child(mono(line, theme.text_muted)));
                        }
                    }
                    if let Some(line) = co::unmerged_label(plan) {
                        warnings = warnings.child(small(line, theme.danger));
                    }
                    if let Some(line) = co::unverifiable_label(plan) {
                        warnings = warnings.child(small(line, theme.danger));
                    }
                    body.push(warnings.into_any_element());
                }
            }
        }
        if let Some(err) = &dialog.error {
            body.push(small(err.clone(), theme.danger).into_any_element());
        }

        let submitting = dialog.submitting;
        let proceed = verdict.filter(|v| v.can_proceed()).map(|verdict| {
            let force = verdict.force();
            let label = if submitting {
                "Closing out\u{2026}"
            } else {
                verdict.button_label().unwrap_or("Close out")
            };
            popover::btn_danger(&theme, label)
                .id(if force { "closeout-force-confirm" } else { "closeout-confirm" })
                .when(submitting, |button| button.opacity(0.6))
                .on_click(cx.listener(move |this, _, _, cx| this.submit_chat_closeout(force, cx)))
        });
        let cancel_label = if verdict.is_some_and(|v| !v.can_proceed()) { "Close" } else { "Cancel" };

        let card = popover::dialog_card(&theme)
            .w(px(440.0))
            .child(popover::dialog_title(&theme, title))
            .child(div().mt(px(8.0)).flex().flex_col().gap(px(8.0)).children(body))
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, cancel_label, "closeout-cancel")
                            .id("closeout-cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.close_chat_closeout(cx))),
                    )
                    .children(proceed),
            )
            .into_any_element();
        Some(popover::modal("chat-closeout-dialog", viewport, card))
    }
}
