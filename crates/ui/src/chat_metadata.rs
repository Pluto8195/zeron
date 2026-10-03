//! Compact, shared metadata shown immediately below a chat title.
//!
//! The model is deliberately presentation-only. Durable PR links continue to
//! live in the workspace document and arrive through `CHAT_LINK_STATUS`; this
//! module only gives the full-chat and overview-panel headers one rendering
//! contract.

use gpui::{
    ClipboardItem, InteractiveElement as _, IntoElement as _, ParentElement as _, Render,
    SharedString, Styled as _, div, prelude::*, px,
};
use zeron_engine::pr_ticket_cache::ChatLinkStatus;
use zeron_proto::{Chat, ChatLinkSource, Space};

use crate::theme::Theme;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeaderPr {
    pub url: String,
    pub number: u64,
    pub title: Option<String>,
    pub source: Option<ChatLinkSource>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeaderTicket {
    pub identifier: String,
    pub title: Option<String>,
    pub url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeaderLinks {
    pub prs: Vec<HeaderPr>,
    pub ticket: Option<HeaderTicket>,
}

impl HeaderLinks {
    pub(crate) fn from_status(status: &ChatLinkStatus) -> Self {
        let prs = if status.pr_links.is_empty() {
            status
                .pr
                .as_ref()
                .map(|pr| HeaderPr {
                    url: pr.url.clone().unwrap_or_default(),
                    number: pr.number,
                    title: pr.title.clone(),
                    source: status.pr_source,
                })
                .into_iter()
                .collect()
        } else {
            status
                .pr_links
                .iter()
                .map(|link| HeaderPr {
                    url: link.url.clone(),
                    number: link
                        .detail
                        .as_ref()
                        .map_or_else(|| pr_number_from_url(&link.url), |pr| pr.number),
                    title: link.detail.as_ref().and_then(|pr| pr.title.clone()),
                    source: link.source,
                })
                .collect()
        };
        Self {
            prs,
            ticket: status.ticket.as_ref().map(|ticket| HeaderTicket {
                identifier: ticket.identifier.clone(),
                title: ticket.title.clone(),
                url: ticket.url.clone(),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MetadataAction {
    OpenUrl(String),
    Copy(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MetadataItem {
    pub key: String,
    pub label: String,
    pub tooltip: String,
    action: Option<MetadataAction>,
}

fn item(
    key: impl Into<String>,
    label: impl Into<String>,
    tooltip: impl Into<String>,
) -> MetadataItem {
    MetadataItem {
        key: key.into(),
        label: label.into(),
        tooltip: tooltip.into(),
        action: None,
    }
}

fn source_label(source: Option<ChatLinkSource>) -> &'static str {
    match source {
        Some(ChatLinkSource::Manual) => "linked manually",
        Some(ChatLinkSource::CreatedInChat) => "created in this chat",
        Some(ChatLinkSource::Mentioned) => "mentioned in this chat",
        None => "inferred from the branch",
    }
}

fn pr_number_from_url(url: &str) -> u64 {
    url.rsplit_once("/pull/")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        .and_then(|number| number.parse().ok())
        .unwrap_or(0)
}

fn compact_id(value: &str) -> String {
    const KEEP: usize = 10;
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(KEEP).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

/// Stable, compact item list shared by both header surfaces. Optional values
/// are omitted, except model: a missing explicit model is shown honestly as
/// the harness default instead of guessed.
pub(crate) fn metadata_items(
    chat: &Chat,
    space: Option<&Space>,
    links: Option<&HeaderLinks>,
    category: Option<&str>,
    origin: Option<&str>,
) -> Vec<MetadataItem> {
    let mut items = Vec::new();
    if let Some(links) = links {
        for (index, pr) in links.prs.iter().enumerate() {
            let label = if pr.number > 0 {
                format!("PR #{}", pr.number)
            } else {
                "Pull request".into()
            };
            let mut tooltip = pr.title.clone().unwrap_or_else(|| pr.url.clone());
            if !tooltip.is_empty() {
                tooltip.push_str(" · ");
            }
            tooltip.push_str(source_label(pr.source));
            let mut value = item(format!("pr-{index}"), label, tooltip);
            if !pr.url.is_empty() {
                value.action = Some(MetadataAction::OpenUrl(pr.url.clone()));
            }
            items.push(value);
        }
        if let Some(ticket) = &links.ticket {
            let mut value = item(
                "ticket",
                format!("Ticket {}", ticket.identifier),
                ticket
                    .title
                    .clone()
                    .unwrap_or_else(|| ticket.identifier.clone()),
            );
            value.action = ticket.url.clone().map(MetadataAction::OpenUrl);
            items.push(value);
        }
    }

    let (model, model_tooltip) = chat
        .config
        .as_ref()
        .and_then(|config| config.model.as_deref())
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(|model| (model.to_string(), format!("Model: {model}")))
        .unwrap_or_else(|| {
            (
                "default".into(),
                "Model: harness default (not explicitly set)".into(),
            )
        });
    items.push(item("model", format!("Model {model}"), model_tooltip));

    if let Some(branch) = chat
        .source_context
        .as_ref()
        .map(|context| context.branch.as_str())
        .or(chat.branch.as_deref())
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
    {
        items.push(item(
            "branch",
            format!("Branch {branch}"),
            format!("Branch: {branch}"),
        ));
    }
    if let Some(space) = space {
        let prefix = if space.git_detected {
            "Repo"
        } else {
            "Workspace"
        };
        let cwd = chat.cwd.as_deref().unwrap_or(&space.path);
        items.push(item(
            "workspace",
            format!("{prefix} {}", space.display_name()),
            cwd,
        ));
    } else if let Some(cwd) = chat.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
        let name = cwd
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(cwd);
        items.push(item("workspace", format!("Workspace {name}"), cwd));
    }
    if let Some(category) = category.map(str::trim).filter(|value| !value.is_empty()) {
        items.push(item(
            "category",
            format!("Category {category}"),
            format!("Category: {category}"),
        ));
    }
    if let Some(origin) = origin.map(str::trim).filter(|value| !value.is_empty()) {
        items.push(item(
            "origin",
            format!("Origin {origin}"),
            format!("Origin: {origin}"),
        ));
    }

    let mut chat_id = item(
        "chat-id",
        format!("Chat {}", compact_id(&chat.id)),
        format!("Chat ID: {} · click to copy", chat.id),
    );
    chat_id.action = Some(MetadataAction::Copy(chat.id.clone()));
    items.push(chat_id);
    if let Some(session) = chat
        .harness_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let mut session_id = item(
            "session-id",
            format!("Session {}", compact_id(session)),
            format!("Harness session ID: {session} · click to copy"),
        );
        session_id.action = Some(MetadataAction::Copy(session.to_string()));
        items.push(session_id);
    }
    items
}

struct MetadataTooltip(SharedString);

impl Render for MetadataTooltip {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let theme = Theme::of(cx);
        div()
            .max_w(px(320.0))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.text)
            .child(self.0.clone())
    }
}

/// Wrapping metadata strip. Individual values truncate and expose their full
/// text in a tooltip; PR/ticket chips open their URL and ID chips copy the
/// exact unshortened value.
pub(crate) fn metadata_strip(
    prefix: &str,
    items: Vec<MetadataItem>,
    theme: &Theme,
) -> gpui::AnyElement {
    let prefix = prefix.to_string();
    div()
        .id(SharedString::from(format!("{prefix}-metadata")))
        .debug_selector(|| "chat-header-metadata".into())
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(4.0))
        .children(items.into_iter().map(|item| {
            let tooltip: SharedString = item.tooltip.into();
            let action = item.action;
            let clickable = action.is_some();
            div()
                .id(SharedString::from(format!(
                    "{prefix}-metadata-{}",
                    item.key
                )))
                .max_w(px(180.0))
                .h(px(20.0))
                .px(px(6.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .border_1()
                .border_color(theme.border.opacity(0.8))
                .bg(theme.element_hover.opacity(0.55))
                .text_size(crate::typography::ui_rems(9.5))
                .text_color(theme.text_muted)
                .when(clickable, |chip| {
                    chip.cursor_pointer()
                        .hover(|chip| chip.bg(theme.element_hover).text_color(theme.text))
                })
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(item.label)),
                )
                .tooltip(move |_, cx| cx.new(|_| MetadataTooltip(tooltip.clone())).into())
                .when_some(action, |chip, action| {
                    chip.on_click(move |_, _, cx| match &action {
                        MetadataAction::OpenUrl(url) => cx.open_url(url),
                        MetadataAction::Copy(value) => {
                            cx.write_to_clipboard(ClipboardItem::new_string(value.clone()))
                        }
                    })
                })
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat() -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": "chat-1234567890abcdef",
            "deviceId": "device",
            "title": "Metadata",
            "archived": false,
            "cwd": "/work/zeron",
            "branch": "feature/header",
            "checkoutId": null,
            "config": null,
            "lastMessagePreview": null,
            "lastMessageAt": null,
            "createdAt": "2026-01-01T00:00:00Z",
            "harnessSessionId": "session-abcdefghijklmnop"
        }))
        .unwrap()
    }

    #[test]
    fn emits_every_pr_and_honest_model_fallback() {
        let links = HeaderLinks {
            prs: vec![
                HeaderPr {
                    url: "https://github.com/acme/app/pull/7".into(),
                    number: 7,
                    title: Some("First".into()),
                    source: Some(ChatLinkSource::Manual),
                },
                HeaderPr {
                    url: "https://github.com/acme/app/pull/8".into(),
                    number: 8,
                    title: Some("Second".into()),
                    source: Some(ChatLinkSource::Mentioned),
                },
            ],
            ticket: Some(HeaderTicket {
                identifier: "ENG-42".into(),
                title: Some("Header metadata".into()),
                url: None,
            }),
        };
        let items = metadata_items(
            &chat(),
            None,
            Some(&links),
            Some("coding"),
            Some("imported"),
        );
        let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
        assert!(labels.contains(&"PR #7"));
        assert!(labels.contains(&"PR #8"));
        assert!(labels.contains(&"Ticket ENG-42"));
        assert!(labels.contains(&"Model default"));
        assert!(labels.contains(&"Category coding"));
        assert!(labels.contains(&"Origin imported"));
    }

    #[test]
    fn ids_are_compact_but_copy_the_complete_value() {
        let items = metadata_items(&chat(), None, None, None, None);
        let chat_id = items.iter().find(|item| item.key == "chat-id").unwrap();
        assert_eq!(chat_id.label, "Chat chat-12345…");
        assert_eq!(
            chat_id.action,
            Some(MetadataAction::Copy("chat-1234567890abcdef".into()))
        );
        let session = items.iter().find(|item| item.key == "session-id").unwrap();
        assert_eq!(
            session.action,
            Some(MetadataAction::Copy("session-abcdefghijklmnop".into()))
        );
    }

    #[test]
    fn engine_status_adapter_keeps_all_prs_and_legacy_fallback() {
        let status: ChatLinkStatus = serde_json::from_value(serde_json::json!({
            "pr": null,
            "prLinks": [
                {"url": "https://github.com/acme/app/pull/7", "source": "manual", "detail": null},
                {"url": "https://github.com/acme/app/pull/8", "source": "mentioned", "detail": null}
            ],
            "ticket": null,
            "isWorktree": false,
            "diffStat": null,
            "prSource": null,
            "ticketSource": null
        }))
        .unwrap();
        let links = HeaderLinks::from_status(&status);
        assert_eq!(
            links.prs.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            [7, 8]
        );

        let legacy: ChatLinkStatus = serde_json::from_value(serde_json::json!({
            "pr": {"number": 9, "url": "https://github.com/acme/app/pull/9", "state": "open", "isDraft": false, "reviewDecision": null, "reviewers": [], "checks": null, "title": "Legacy", "branch": null, "mergeable": "", "hasReviewerRequested": true},
            "ticket": null,
            "isWorktree": false,
            "diffStat": null,
            "prSource": "created_in_chat",
            "ticketSource": null
        }))
        .unwrap();
        let links = HeaderLinks::from_status(&legacy);
        assert_eq!(links.prs.len(), 1);
        assert_eq!(links.prs[0].number, 9);
    }
}
