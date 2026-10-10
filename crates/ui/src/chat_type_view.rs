//! Filtered, type-specific chat inboxes. The row model and filter pipeline are
//! intentionally shared so later ticket/debug/implementation inboxes only add
//! a [`ChatViewKind`] specification instead of another one-off page.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use gpui::{
    Context, Entity, FocusHandle, MouseButton, Render, ScrollHandle, SharedString, Subscription,
    Window, div, prelude::*, px,
};
use zeron_engine::pr_ticket_cache::{ChatLinkStatus, PrStatus};
use zeron_proto::{Chat, ChatIndicator};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::shell::Shell;
use crate::state::{AppState, format_time_ago};
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatViewKind {
    PrReview,
}

impl ChatViewKind {
    fn category(self) -> &'static str {
        match self {
            Self::PrReview => "pr_review",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::PrReview => "PR review chats",
        }
    }

    fn empty_message(self) -> &'static str {
        match self {
            Self::PrReview => "No PR review chats match this view",
        }
    }
}

#[derive(Clone)]
struct TypeChatRow {
    chat: Chat,
    status: ChatIndicator,
    repo: String,
    pr: Option<PrSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PrSummary {
    number: u64,
    title: Option<String>,
    state: Option<String>,
    branch: Option<String>,
}

impl PrSummary {
    fn from_status(status: &ChatLinkStatus) -> Option<Self> {
        let detail = status
            .pr_links
            .first()
            .and_then(|link| link.detail.as_ref())
            .or(status.pr.as_ref());
        if let Some(pr) = detail {
            return Some(Self::from_pr(pr));
        }
        let link = status.pr_links.first()?;
        Some(Self {
            number: pr_number_from_url(&link.url),
            title: None,
            state: None,
            branch: None,
        })
    }

    fn from_pr(pr: &PrStatus) -> Self {
        Self {
            number: pr.number,
            title: pr.title.clone(),
            state: Some(if pr.is_draft {
                "draft".to_string()
            } else {
                pr.state.clone()
            }),
            branch: pr.branch.clone(),
        }
    }
}

fn pr_number_from_url(url: &str) -> u64 {
    url.rsplit_once("/pull/")
        .and_then(|(_, tail)| tail.split(['/', '?', '#']).next())
        .and_then(|number| number.parse().ok())
        .unwrap_or(0)
}

fn repo_label(chat: &Chat, state: &AppState) -> String {
    if let Some(space) = state.space_for_chat(chat) {
        return space.display_name().to_string();
    }
    chat.cwd
        .as_deref()
        .and_then(|cwd| cwd.trim_end_matches('/').rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("No repository")
        .to_string()
}

fn row_matches(row: &TypeChatRow, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let pr = row.pr.as_ref();
    let haystack = format!(
        "{} {} {} {} {} {} {}",
        row.chat.title.as_deref().unwrap_or("New session"),
        row.repo,
        row.chat.branch.as_deref().unwrap_or_default(),
        pr.map(|pr| pr.number.to_string()).unwrap_or_default(),
        pr.and_then(|pr| pr.title.as_deref()).unwrap_or_default(),
        pr.and_then(|pr| pr.state.as_deref()).unwrap_or_default(),
        row.chat.last_message_preview.as_deref().unwrap_or_default(),
    )
    .to_lowercase();
    query.split_whitespace().all(|word| haystack.contains(word))
}

fn chat_visible(
    kind: ChatViewKind,
    chat: &Chat,
    category: Option<&str>,
    show_archived: bool,
) -> bool {
    chat.parent_chat_id.is_none()
        && (show_archived || !chat.archived)
        && category == Some(kind.category())
}

fn sort_rows(rows: &mut [TypeChatRow]) {
    rows.sort_by(|a, b| {
        let a_updated = a.chat.last_message_at.unwrap_or(a.chat.created_at);
        let b_updated = b.chat.last_message_at.unwrap_or(b.chat.created_at);
        b_updated.cmp(&a_updated).then_with(|| {
            a.chat
                .title
                .as_deref()
                .unwrap_or_default()
                .cmp(b.chat.title.as_deref().unwrap_or_default())
        })
    });
}

fn status_label(status: ChatIndicator) -> &'static str {
    match status {
        ChatIndicator::Working => "Working",
        ChatIndicator::AwaitingInput => "Needs input",
        ChatIndicator::Errored => "Failed",
        ChatIndicator::Completed => "Done",
        ChatIndicator::Idle => "Idle",
    }
}

fn move_selection(active: usize, count: usize, direction: i32) -> usize {
    if count == 0 {
        return 0;
    }
    if direction < 0 {
        active.saturating_sub(1)
    } else {
        (active + 1).min(count - 1)
    }
}

pub(crate) struct ChatTypeView {
    state: Entity<AppState>,
    shell: gpui::WeakEntity<Shell>,
    kind: ChatViewKind,
    search: Entity<ComposerInput>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    show_archived: bool,
    active: usize,
    classifications: HashMap<String, String>,
    classification_pending: HashSet<String>,
    links: HashMap<String, ChatLinkStatus>,
    links_pending: HashSet<String>,
    _state_observation: Subscription,
    _search_events: Subscription,
}

impl ChatTypeView {
    pub(crate) fn new(
        state: Entity<AppState>,
        shell: gpui::WeakEntity<Shell>,
        kind: ChatViewKind,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            ComposerInput::with_context(
                "Search PR review chats…",
                crate::composer::PALETTE_SEARCH_CONTEXT,
                cx,
            )
            .with_single_line()
            .with_accessibility_role(gpui::Role::SearchInput)
        });
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                this.active = 0;
                this.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
                cx.notify();
            }
            ComposerInputEvent::Submitted => {
                let rows = this.rows(cx);
                if let Some(row) = rows.get(this.active) {
                    this.open_chat(row.chat.id.clone(), cx);
                }
            }
            _ => {}
        });
        let state_observation = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            shell,
            kind,
            search,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            show_archived: false,
            active: 0,
            classifications: HashMap::new(),
            classification_pending: HashSet::new(),
            links: HashMap::new(),
            links_pending: HashSet::new(),
            _state_observation: state_observation,
            _search_events: search_events,
        }
    }

    pub(crate) fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&gpui::Focusable::focus_handle(self.search.read(cx), cx), cx);
    }

    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub(crate) fn classification_changed(
        &mut self,
        chat_id: &str,
        value: &serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        if let Some(category) = value.get("category").and_then(|value| value.as_str()) {
            self.classifications
                .insert(chat_id.to_string(), category.to_string());
        } else {
            self.classifications.remove(chat_id);
        }
        self.active = 0;
        cx.notify();
    }

    fn ensure_classification(&mut self, chat_id: String, cx: &mut Context<Self>) {
        if self.classifications.contains_key(&chat_id)
            || self.classification_pending.contains(&chat_id)
        {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.classification_pending.insert(chat_id.clone());
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::CHAT_CLASSIFICATION,
                    serde_json::json!({ "chatId": chat_id }),
                )
                .await;
            this.update(cx, |this, cx| {
                this.classification_pending.remove(&chat_id);
                if let Ok(value) = result
                    && let Some(category) = value.get("category").and_then(|value| value.as_str())
                {
                    this.classifications
                        .insert(chat_id.clone(), category.to_string());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn ensure_links(&mut self, chat_id: String, cx: &mut Context<Self>) {
        if self.links.contains_key(&chat_id) || self.links_pending.contains(&chat_id) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.links_pending.insert(chat_id.clone());
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::CHAT_LINK_STATUS,
                    serde_json::json!({ "chatId": chat_id }),
                )
                .await;
            this.update(cx, |this, cx| {
                this.links_pending.remove(&chat_id);
                if let Ok(value) = result
                    && let Ok(status) = serde_json::from_value::<ChatLinkStatus>(value)
                {
                    this.links.insert(chat_id.clone(), status);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn rows(&mut self, cx: &mut Context<Self>) -> Vec<TypeChatRow> {
        let now = Utc::now();
        let chats: Vec<Chat> = self.state.read(cx).chats.clone();
        for chat in &chats {
            self.ensure_classification(chat.id.clone(), cx);
        }

        let query = self.search.read(cx).text().trim().to_lowercase();
        let mut rows = Vec::new();
        for chat in chats {
            if !chat_visible(
                self.kind,
                &chat,
                self.classifications.get(&chat.id).map(String::as_str),
                self.show_archived,
            ) {
                continue;
            }
            self.ensure_links(chat.id.clone(), cx);
            let state = self.state.read(cx);
            let row = TypeChatRow {
                status: state.display_status_for(&chat, now),
                repo: repo_label(&chat, state),
                pr: self.links.get(&chat.id).and_then(PrSummary::from_status),
                chat,
            };
            if row_matches(&row, &query) {
                rows.push(row);
            }
        }
        sort_rows(&mut rows);
        rows
    }

    fn open_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let _ = self
            .shell
            .update(cx, |shell, cx| shell.open_chat(chat_id, cx));
    }

    fn on_key_down(&mut self, event: &gpui::KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let count = self.rows(cx).len();
        match event.keystroke.key.as_str() {
            "down" if count > 0 => {
                self.active = move_selection(self.active, count, 1);
                self.scroll.scroll_to_item(self.active);
                cx.stop_propagation();
                cx.notify();
            }
            "up" if count > 0 => {
                self.active = move_selection(self.active, count, -1);
                self.scroll.scroll_to_item(self.active);
                cx.stop_propagation();
                cx.notify();
            }
            "enter" if count > 0 => {
                let rows = self.rows(cx);
                if let Some(row) = rows.get(self.active) {
                    self.open_chat(row.chat.id.clone(), cx);
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn render_row(
        &self,
        row: TypeChatRow,
        index: usize,
        theme: &Theme,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let active = index == self.active;
        let chat_id = row.chat.id.clone();
        let title: SharedString = row
            .chat
            .title
            .clone()
            .unwrap_or_else(|| "New session".to_string())
            .into();
        let (pr_number, pr_title, pr_state, branch) = row.pr.as_ref().map_or_else(
            || {
                (
                    "—".to_string(),
                    "No linked PR detail".to_string(),
                    "—".to_string(),
                    row.chat.branch.clone().unwrap_or_else(|| "—".to_string()),
                )
            },
            |pr| {
                (
                    (pr.number > 0)
                        .then(|| format!("#{}", pr.number))
                        .unwrap_or_else(|| "PR".to_string()),
                    pr.title.clone().unwrap_or_else(|| "Linked PR".to_string()),
                    pr.state.clone().unwrap_or_else(|| "linked".to_string()),
                    pr.branch
                        .clone()
                        .or_else(|| row.chat.branch.clone())
                        .unwrap_or_else(|| "—".to_string()),
                )
            },
        );
        let updated = row.chat.last_message_at.unwrap_or(row.chat.created_at);
        div()
            .id(("pr-review-chat-row", index))
            .min_h(px(52.0))
            .grid()
            .grid_cols(7)
            .items_center()
            .gap(px(12.0))
            .px(px(14.0))
            .py(px(8.0))
            .border_b_1()
            .border_color(theme.border.opacity(0.65))
            .cursor_pointer()
            .when(active, |element| element.bg(theme.element_active))
            .hover(|element| element.bg(theme.element_hover))
            .on_mouse_down(MouseButton::Left, move |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.active = index;
                this.open_chat(chat_id.clone(), cx);
            }))
            .child(
                div()
                    .col_span(2)
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .truncate()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(title),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(pr_title)),
                    ),
            )
            .child(div().truncate().child(SharedString::from(row.repo)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.accent)
                            .child(SharedString::from(pr_number)),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(crate::typography::ui_rems(10.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(pr_state)),
                    ),
            )
            .child(
                div()
                    .truncate()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(branch)),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(status_label(row.status))),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format_time_ago(updated, now))),
            )
            .into_any_element()
    }
}

impl Render for ChatTypeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let rows = self.rows(cx);
        self.active = self.active.min(rows.len().saturating_sub(1));
        let count = rows.len();
        let shortcut = crate::settings::badge_combo(crate::shell::OPEN_PR_REVIEW_CHATS_COMBO);

        div()
            .id("chat-type-view")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .child(
                div()
                    .flex_none()
                    .h(px(54.0))
                    .px(px(18.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(15.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(SharedString::from(self.kind.title())),
                            )
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(format!("{count} chats"))),
                            )
                            .child(popover::kbd_hint(&theme, &shortcut)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(div().w(px(280.0)).child(popover::search_input_frame(
                                &theme,
                                self.search.clone().into_any_element(),
                            )))
                            .child(
                                div()
                                    .id("chat-type-show-archived")
                                    .px(px(9.0))
                                    .py(px(5.0))
                                    .rounded(px(6.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .cursor_pointer()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(if self.show_archived {
                                        theme.text
                                    } else {
                                        theme.text_muted
                                    })
                                    .when(self.show_archived, |element| {
                                        element.bg(theme.element_active)
                                    })
                                    .hover(|element| element.bg(theme.element_hover))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_archived = !this.show_archived;
                                        this.active = 0;
                                        cx.notify();
                                    }))
                                    .child("Show archived"),
                            ),
                    ),
            )
            .child(
                div()
                    .h(px(34.0))
                    .flex_none()
                    .grid()
                    .grid_cols(7)
                    .items_center()
                    .gap(px(12.0))
                    .px(px(14.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.element_hover.opacity(0.45))
                    .text_size(crate::typography::ui_rems(10.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(div().col_span(2).child("CHAT / PR TITLE"))
                    .child("REPOSITORY")
                    .child("PR")
                    .child("BRANCH")
                    .child("STATUS")
                    .child("UPDATED"),
            )
            .child(
                div()
                    .id("chat-type-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .when(rows.is_empty(), |element| {
                        element.child(
                            div()
                                .h_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(theme.text_muted)
                                .child(SharedString::from(self.kind.empty_message())),
                        )
                    })
                    .children(
                        rows.into_iter()
                            .enumerate()
                            .map(|(index, row)| self.render_row(row, index, &theme, now, cx)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, title: &str, updated: i64, archived: bool) -> Chat {
        let at = DateTime::from_timestamp(updated, 0).unwrap();
        Chat {
            id: id.into(),
            device_id: "device".into(),
            title: Some(title.into()),
            archived,
            cwd: Some(format!("/work/{id}")),
            branch: Some(format!("branch-{id}")),
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: Some(at),
            created_at: at,
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
            parent_chat_id: None,
            linked_pr_url: None,
            linked_pr_source: None,
            linked_ticket_id: None,
            linked_ticket_source: None,
        }
    }

    fn row(id: &str, title: &str, updated: i64, number: u64) -> TypeChatRow {
        TypeChatRow {
            chat: chat(id, title, updated, false),
            status: ChatIndicator::Idle,
            repo: format!("repo-{id}"),
            pr: Some(PrSummary {
                number,
                title: Some(format!("Fix {title}")),
                state: Some("open".into()),
                branch: None,
            }),
        }
    }

    #[test]
    fn pr_review_kind_uses_authoritative_category_key() {
        assert_eq!(ChatViewKind::PrReview.category(), "pr_review");
    }

    #[test]
    fn filtering_uses_category_and_archived_toggle() {
        let active = chat("active", "Active", 1, false);
        let archived = chat("archived", "Archived", 1, true);
        assert!(chat_visible(
            ChatViewKind::PrReview,
            &active,
            Some("pr_review"),
            false
        ));
        assert!(!chat_visible(
            ChatViewKind::PrReview,
            &active,
            Some("debug"),
            false
        ));
        assert!(!chat_visible(
            ChatViewKind::PrReview,
            &archived,
            Some("pr_review"),
            false
        ));
        assert!(chat_visible(
            ChatViewKind::PrReview,
            &archived,
            Some("pr_review"),
            true
        ));
    }

    #[test]
    fn search_covers_chat_repo_pr_and_branch_fields() {
        let row = row("alpha", "Review cache race", 2, 418);
        assert!(row_matches(&row, "cache"));
        assert!(row_matches(&row, "repo-alpha 418"));
        assert!(row_matches(&row, "branch-alpha"));
        assert!(!row_matches(&row, "unrelated"));
    }

    #[test]
    fn inbox_order_is_most_recent_first_with_stable_title_tie_break() {
        let mut rows = vec![
            row("older", "Zulu", 1, 1),
            row("b", "Beta", 2, 2),
            row("a", "Alpha", 2, 3),
        ];
        sort_rows(&mut rows);
        assert_eq!(
            rows.iter()
                .map(|row| row.chat.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "older"]
        );
    }

    #[test]
    fn linked_pr_number_falls_back_to_url_while_detail_loads() {
        let status: ChatLinkStatus = serde_json::from_value(serde_json::json!({
            "pr": null,
            "prLinks": [{
                "url": "https://github.com/acme/app/pull/77",
                "source": "manual",
                "detail": null
            }],
            "ticket": null,
            "isWorktree": false,
            "diffStat": null,
            "prSource": null,
            "ticketSource": null
        }))
        .unwrap();
        assert_eq!(PrSummary::from_status(&status).unwrap().number, 77);
    }

    #[test]
    fn keyboard_selection_clamps_at_both_ends() {
        assert_eq!(move_selection(0, 3, -1), 0);
        assert_eq!(move_selection(0, 3, 1), 1);
        assert_eq!(move_selection(2, 3, 1), 2);
        assert_eq!(move_selection(4, 0, 1), 0);
    }
}
