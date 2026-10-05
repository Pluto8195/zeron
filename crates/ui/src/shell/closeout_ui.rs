//! The chat close-out confirmation dialog (see [`crate::chat_closeout`]).
//!
//! Opened from the sidebar chat menu's "Close out worktree…" and the
//! overview's expanded-tile detail. Fetches `PLAN_CHAT_CLOSEOUT`, then shows
//! one of: loading, live-chat notice, not-a-worktree notice, default-branch
//! notice, clean one-step confirm (`force: false`) or a two-step forced
//! confirm with warnings (`force: true`). Live and plan-failed states offer
//! "Check again".
//! Engine errors from `CLOSE_CHAT_WORKTREE` render verbatim inside the
//! still-open dialog, which then re-plans quietly so its buttons track the
//! engine's current view. On success the engine archives the chat;
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
    /// Present when the worktree is attached to a chat. Repo Map can also
    /// close an unmatched worktree, in which case the engine receives null.
    chat_id: Option<String>,
    cwd: String,
    title: String,
    /// Stale-reply guard ([`co::reply_is_current`]): replies for an earlier
    /// open of the dialog are dropped.
    request_id: u64,
    phase: CloseoutPhase,
    /// A `CLOSE_CHAT_WORKTREE` is in flight (ignore repeat clicks).
    submitting: bool,
    /// The user completed the first of two force-close clicks. Cleared any
    /// time the dialog is reopened or its plan is refreshed.
    force_armed: bool,
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
        let chat = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .cloned();
        let title = transcript::single_line(
            &chat
                .as_ref()
                .and_then(|c| c.title.clone())
                .unwrap_or_else(|| "New session".into()),
        );
        let cwd = chat.and_then(|c| c.cwd).unwrap_or_default();
        self.open_worktree_closeout(cwd, title, Some(chat_id), cx);
    }

    /// Open the protected close-out flow for a checkout selected outside a
    /// chat surface (for example Repo Map). `chat_id` is optional because a
    /// discovered worktree need not have a matched chat.
    pub(crate) fn open_worktree_closeout(
        &mut self,
        cwd: String,
        title: String,
        chat_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.closeout_seq += 1;
        let request_id = self.closeout_seq;
        let connected = self.state.read(cx).engine().is_some();
        let phase = if cwd.trim().is_empty() {
            CloseoutPhase::PlanFailed("This worktree has no folder to close out.".into())
        } else if !connected {
            CloseoutPhase::PlanFailed("Engine not connected".into())
        } else {
            CloseoutPhase::Loading
        };
        let loading = matches!(phase, CloseoutPhase::Loading);
        self.closeout_dialog = Some(CloseoutDialog {
            chat_id,
            cwd,
            title,
            request_id,
            phase,
            submitting: false,
            force_armed: false,
            error: None,
        });
        cx.notify();
        if loading {
            self.fetch_closeout_plan(cx);
        }
    }

    /// (Re)fetch the open dialog's plan. The current phase stays on screen
    /// until the reply lands, so callers that want a spinner set
    /// [`CloseoutPhase::Loading`] first. A failed re-plan after a refusal
    /// leaves the previous phase alone (the refusal message is already up).
    fn fetch_closeout_plan(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.closeout_dialog.as_ref() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let request_id = dialog.request_id;
        let chat_id = dialog.chat_id.clone();
        let cwd = dialog.cwd.clone();
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
                let Some(dialog) = shell.closeout_dialog.as_mut() else {
                    return;
                };
                match result {
                    Ok(plan) => {
                        dialog.phase = CloseoutPhase::Ready(plan);
                        dialog.force_armed = false;
                    }
                    Err(err) => {
                        tracing::warn!(chat = ?dialog.chat_id, error = %err, "PlanChatCloseout failed");
                        let quiet_replan =
                            matches!(dialog.phase, CloseoutPhase::Ready(_)) && dialog.error.is_some();
                        if !quiet_replan {
                            dialog.phase = CloseoutPhase::PlanFailed(err);
                        }
                        dialog.force_armed = false;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// "Check again": re-plan from scratch (after stopping a live chat, or
    /// when the plan call itself failed).
    fn recheck_chat_closeout(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.closeout_dialog.as_mut() else {
            return;
        };
        if dialog.submitting {
            return;
        }
        dialog.force_armed = false;
        if self.state.read(cx).engine().is_none() {
            dialog.phase = CloseoutPhase::PlanFailed("Engine not connected".into());
        } else {
            dialog.phase = CloseoutPhase::Loading;
            dialog.error = None;
            self.fetch_closeout_plan(cx);
        }
        cx.notify();
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
                        shell.sidebar_notice =
                            Some(co::success_notice(branch.as_deref(), &outcome).into());
                        if current {
                            shell.closeout_dialog = None;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(chat = ?chat_id, error = %err, "CloseChatWorktree failed");
                        if current {
                            if let Some(dialog) = shell.closeout_dialog.as_mut() {
                                dialog.submitting = false;
                                dialog.error = Some(err);
                            }
                            // The engine re-inspects on close, so a refusal
                            // means the plan we showed is stale (a file got
                            // dirtied, the chat went live…). Re-plan quietly
                            // so the buttons match what the engine now
                            // reports; the refusal message stays up.
                            shell.fetch_closeout_plan(cx);
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

    /// Clean close-out submits immediately. A force close first arms a
    /// deliberately explicit permanent-discard state; only a second click
    /// can issue the destructive RPC.
    fn proceed_chat_closeout(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.closeout_dialog.as_mut() else {
            return;
        };
        if dialog.submitting {
            return;
        }
        let verdict = match &dialog.phase {
            CloseoutPhase::Ready(plan) => co::verdict(plan),
            _ => return,
        };
        match co::proceed_action(verdict, dialog.force_armed) {
            co::CloseoutProceed::Blocked => {}
            co::CloseoutProceed::ArmForce => {
                dialog.force_armed = true;
                dialog.error = None;
                cx.notify();
            }
            co::CloseoutProceed::Submit { force } => self.submit_chat_closeout(force, cx),
        }
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
        let mono =
            |text: String, color: gpui::Hsla| small(text, color).font_family(code_family.clone());

        let verdict = match &dialog.phase {
            CloseoutPhase::Ready(plan) => Some(co::verdict(plan)),
            _ => None,
        };
        let title = match verdict {
            Some(
                CloseoutVerdict::Live
                | CloseoutVerdict::NotWorktree
                | CloseoutVerdict::OnDefaultBranch
                | CloseoutVerdict::DirtyInspectionFailed,
            ) => "Can\u{2019}t close out worktree",
            Some(CloseoutVerdict::NeedsForce) => "Force close out worktree?",
            _ => "Close out worktree?",
        };

        let mut body: Vec<AnyElement> = vec![
            small(
                format!("\u{201C}{}\u{201D}", dialog.title),
                theme.text_muted,
            )
            .into_any_element(),
        ];
        match &dialog.phase {
            CloseoutPhase::Loading => {
                body.push(
                    popover::dialog_body(&theme, "Checking the worktree\u{2026}")
                        .into_any_element(),
                );
            }
            CloseoutPhase::PlanFailed(err) => {
                body.push(
                    popover::dialog_body(&theme, "Couldn\u{2019}t inspect this worktree.")
                        .into_any_element(),
                );
                body.push(small(err.clone(), theme.danger).into_any_element());
            }
            CloseoutPhase::Ready(plan) => {
                let verdict = co::verdict(plan);
                let summary = co::summary_rows(plan);
                if !summary.is_empty() {
                    let rows = summary.into_iter().map(|(label, value)| {
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(64.0))
                                    .child(small(label.to_string(), theme.text_muted)),
                            )
                            .child(div().flex_1().min_w_0().child(mono(value, theme.text)))
                    });
                    body.push(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .children(rows)
                            .into_any_element(),
                    );
                }
                body.push(popover::dialog_body(&theme, co::body_copy(plan)).into_any_element());
                if matches!(
                    verdict,
                    CloseoutVerdict::NeedsForce | CloseoutVerdict::DirtyInspectionFailed
                ) {
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
                            warnings = warnings
                                .child(div().pl(px(10.0)).child(mono(line, theme.text_muted)));
                        }
                    }
                    if let Some(line) = co::dirty_inspection_label(plan) {
                        warnings = warnings.child(small(line, theme.danger));
                    }
                    if let Some(line) = co::unmerged_label(plan) {
                        warnings = warnings.child(small(line, theme.danger));
                    }
                    if let Some(line) = co::unverifiable_label(plan) {
                        warnings = warnings.child(small(line, theme.danger));
                    }
                    body.push(warnings.into_any_element());
                    if dialog.force_armed {
                        body.push(
                            small(
                                "Confirm permanent discard: this work cannot be recovered after the worktree is removed."
                                    .to_string(),
                                theme.danger,
                            )
                            .into_any_element(),
                        );
                    }
                }
            }
        }
        if let Some(err) = &dialog.error {
            body.push(small(err.clone(), theme.danger).into_any_element());
        }

        let submitting = dialog.submitting;
        let force_armed = dialog.force_armed;
        let proceed = verdict.filter(|v| v.can_proceed()).map(|verdict| {
            let force = verdict.force();
            let label = if submitting {
                "Closing out\u{2026}"
            } else if force && force_armed {
                "Permanently discard & close out"
            } else {
                verdict.button_label().unwrap_or("Close out")
            };
            popover::btn_danger(&theme, label)
                .id(if force {
                    "closeout-force-confirm"
                } else {
                    "closeout-confirm"
                })
                .when(submitting, |button| button.opacity(0.6))
                .on_click(cx.listener(|this, _, _, cx| this.proceed_chat_closeout(cx)))
        });
        let cancel_label = if verdict.is_some_and(|v| !v.can_proceed()) {
            "Close"
        } else {
            "Cancel"
        };
        // Worth re-asking only when the answer can change without reopening:
        // a live chat that's since been stopped, a transient dirty-inspection
        // failure, or a failed plan call.
        let recheck = (matches!(dialog.phase, CloseoutPhase::PlanFailed(_))
            || matches!(
                verdict,
                Some(CloseoutVerdict::Live | CloseoutVerdict::DirtyInspectionFailed)
            ))
        .then(|| {
            popover::btn_ghost(&theme, "Check again", "closeout-recheck")
                .id("closeout-recheck")
                .on_click(cx.listener(|this, _, _, cx| this.recheck_chat_closeout(cx)))
        });

        let card = popover::dialog_card(&theme)
            .w(px(440.0))
            .child(popover::dialog_title(&theme, title))
            .child(
                div()
                    .mt(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .children(body),
            )
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
                    .children(recheck)
                    .children(proceed),
            )
            .into_any_element();
        Some(popover::modal("chat-closeout-dialog", viewport, card))
    }
}
