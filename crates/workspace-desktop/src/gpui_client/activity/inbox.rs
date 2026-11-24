//! Показывает личные разговоры и каналы с активными непрочитанными из снимка Core.

use gpui::{App, Div, Window, div, prelude::*, px};
use uuid::Uuid;
use workspace_core::{
    bootstrap::ChatListStatus,
    messenger::{
        activity::{InboxGroup, InboxRow, project_inbox},
        catalog::Chat,
    },
    storage::HistoryScope,
};

use crate::gpui_client::{
    contracts::ActivityAddress,
    preferences::Language,
    ui::{BadgeTone, Button, Icon, PANEL_RADIUS, Palette, TextRole, activate, label, unread_badge},
};

/// Действие пользователя на странице входящих с уже подготовленным адресом темы.
pub(in crate::gpui_client) enum InboxAction {
    Open(HistoryScope), // Открыть историю личного разговора или темы.
    Retry,              // Повторить загрузку каталога.
}

/// Строит страницу из проверенного снимка выбранного входа, включая частичный сбой.
pub(in crate::gpui_client) fn render(
    address: &ActivityAddress,
    chats: &[Chat],
    catalog_status: Option<ChatListStatus>,
    palette: Palette,
    language: Language,
    action: impl Fn(InboxAction, &mut Window, &mut App) + Clone + 'static,
) -> impl IntoElement {
    let user = address.session.user_uuid();
    let inbox = project_inbox(chats, user);
    let no_groups = inbox.personal.is_empty() && inbox.channels.is_empty();
    let status = match catalog_status {
        Some(ChatListStatus::Failed) => {
            let retry = action.clone();
            Some(
                super::status(palette, language.tr("inbox.loadError"), true)
                    .debug_selector(|| "inbox-error".into())
                    .child(
                        Button::new("inbox-retry", language.tr("common.retry"))
                            .render(palette, None, move |window, app| {
                                retry(InboxAction::Retry, window, app)
                            })
                            .debug_selector(|| "inbox-retry".into()),
                    ),
            )
        }
        Some(ChatListStatus::Ready) if no_groups => Some(
            super::status(palette, language.tr("inbox.noUnread"), false)
                .debug_selector(|| "inbox-empty".into()),
        ),
        _ if no_groups => Some(
            super::status(
                palette,
                language.tr("desktop.activity.inbox.loading"),
                false,
            )
            .debug_selector(|| "inbox-loading".into()),
        ),
        _ => None,
    };

    div()
        .id("inbox-content")
        .debug_selector(|| "inbox-content".into())
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .gap_3()
        .p_3()
        .overflow_y_scroll()
        .children(status)
        .when(!inbox.personal.is_empty(), |body| {
            body.child(
                section(
                    language.tr("inbox.dm"),
                    &inbox.personal,
                    palette,
                    language,
                    user,
                    &action,
                )
                .debug_selector(|| "inbox-personal".into()),
            )
        })
        .when(!inbox.channels.is_empty(), |body| {
            body.child(
                section(
                    language.tr("inbox.channels"),
                    &inbox.channels,
                    palette,
                    language,
                    user,
                    &action,
                )
                .debug_selector(|| "inbox-channels".into()),
            )
        })
}

/// Помещает карточки одного вида под общим заголовком.
fn section(
    title: &'static str,
    groups: &[InboxGroup<'_>],
    palette: Palette,
    language: Language,
    user: Option<Uuid>,
    action: &(impl Fn(InboxAction, &mut Window, &mut App) + Clone + 'static),
) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .min_w_0()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_1()
                .child(label(palette, TextRole::Caption, title).text_color(palette.text_muted))
                .child(div().flex_1().h(px(1.)).bg(palette.border)),
        )
        .children(
            groups
                .iter()
                .map(|group| card(group, palette, language, user, action)),
        )
}

/// Заголовок разговора показывает собственный счётчик, а строки содержат цели переходов.
fn card(
    group: &InboxGroup<'_>,
    palette: Palette,
    language: Language,
    user: Option<Uuid>,
    action: &(impl Fn(InboxAction, &mut Window, &mut App) + Clone + 'static),
) -> Div {
    let chat = group.chat;
    let name = language.chat_title(chat, user);
    let icon = if chat.direct {
        Icon::Profile
    } else {
        Icon::Channels
    };
    let badge = group
        .active_unread_count
        .and_then(|count| unread_badge(palette, count, BadgeTone::Accent));
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .gap_1()
        .p_2()
        .border_1()
        .border_color(palette.border)
        .rounded(PANEL_RADIUS)
        .bg(palette.surface)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .px_1()
                .child(icon_chip(icon, palette, 28.))
                .child(
                    label(palette, TextRole::Label, name.to_owned())
                        .flex_1()
                        .min_w_0()
                        .truncate(),
                )
                .children(badge),
        )
        .children(
            group
                .rows
                .iter()
                .map(|row| row_button(chat, name, row, icon, palette, language, action.clone())),
        )
}

/// Представляет тему или личный разговор как доступную для клавиатуры кнопку.
fn row_button(
    chat: &Chat,
    name: &str,
    row: &InboxRow<'_>,
    icon: Icon,
    palette: Palette,
    language: Language,
    action: impl Fn(InboxAction, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let scope = row.scope;
    let uid = scope.topic_id.unwrap_or(scope.stream_id).to_string();
    let selector = format!("inbox-row-{uid}");
    let title = row.topic.map_or_else(
        || name.to_owned(),
        |topic| {
            let topic = topic.name.trim();
            let topic = if topic.is_empty() {
                language.tr("inbox.allMessages")
            } else {
                topic
            };
            format!("{name} · {topic}")
        },
    );
    let time = row
        .updated_at
        .and_then(crate::platform::message_time::short_time);
    let button = div()
        .id(gpui::SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex()
        .min_w_0()
        .items_center()
        .gap_2()
        .p_2()
        .rounded(PANEL_RADIUS)
        .border_1()
        .border_color(palette.border)
        .bg(palette.card)
        .child(icon_chip(icon, palette, 32.))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(label(palette, TextRole::Body, title).truncate())
                .children(time.map(|time| label(palette, TextRole::Caption, time))),
        )
        .children(unread_badge(
            palette,
            row.active_unread_count,
            BadgeTone::Accent,
        ));
    if chat.direct || scope.topic_id.is_some() {
        activate(
            button.hover(|style| style.bg(palette.card_hover)),
            None,
            palette,
            move |window, app| action(InboxAction::Open(scope), window, app),
        )
    } else {
        button.opacity(0.65)
    }
}

/// Помещает значок разговора на нейтральную круглую подложку.
fn icon_chip(icon: Icon, palette: Palette, size: f32) -> Div {
    div()
        .size(px(size))
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .rounded_full()
        .bg(palette.background)
        .child(icon.element(px(17.), palette.text_secondary))
}
