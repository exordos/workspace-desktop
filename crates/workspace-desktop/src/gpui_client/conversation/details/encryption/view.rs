//! Рисует сведения о текущем ключе, список локальных ключей и историю запросов.

use gpui::{ClipboardItem, Context, Div, ElementId, SharedString, Stateful, div, prelude::*, px};
use uuid::Uuid;
use workspace_core::{
    directory::ProfilePresence,
    messenger::{
        details::SidebarDetails,
        encryption::{HeldChatKey, key_exchange::KeyRequestOutcome},
    },
};

use super::super::{Conversation, EncryptedScreen, Panel, action};
use super::{Change, GrantState, KeyControls, KeyManagement, PendingKeyGrant, RequestedKey};
use crate::gpui_client::{
    preferences::Language,
    ui::{
        Avatar, AvatarPresence, AvatarSize, Button, ButtonAppearance, Icon, PANEL_RADIUS, Palette,
        TextRole, activate, ellipsized_label, label, popup::tooltip,
    },
};

/// Три раздела истории из загруженных сообщений.
#[derive(Clone, Copy)]
enum RequestSection {
    Incoming, // Новые запросы и ответы, которые можно повторить.
    Granted,  // Подтверждённые выдачи.
    Rejected, // Подтверждённые отказы.
}

impl RequestSection {
    /// Возвращает список в том же порядке, что и на экране.
    const ALL: [Self; 3] = [Self::Incoming, Self::Granted, Self::Rejected];

    /// Сопоставляет строку с разделом по её текущему результату.
    fn includes(self, row: &PendingKeyGrant) -> bool {
        match self {
            Self::Incoming => !matches!(row.state, GrantState::Completed(_)),
            Self::Granted => row.state == GrantState::Completed(KeyRequestOutcome::Granted),
            Self::Rejected => row.state == GrantState::Completed(KeyRequestOutcome::Rejected),
        }
    }

    /// Выбирает локализованную подпись для состояния списка.
    fn title(self) -> &'static str {
        match self {
            Self::Incoming => "e2ee.incomingTitle",
            Self::Granted => "e2ee.grantedTitle",
            Self::Rejected => "e2ee.rejectedTitle",
        }
    }

    /// Даёт устойчивый идентификатор строки раскрытия раздела.
    fn selector(self) -> &'static str {
        match self {
            Self::Incoming => "conversation-key-requests-incoming",
            Self::Granted => "conversation-key-requests-granted",
            Self::Rejected => "conversation-key-requests-rejected",
        }
    }
}

/// Снимок ключей, палитра и владелец действий для одного рендера.
struct KeyView<'a> {
    keys: &'a KeyControls, // Подтверждённые сведения и незавершённое действие.
    panel: &'a Panel,      // Снимок участников и уже загруженные аватары.
    palette: Palette,      // Цвета текущего окна.
    language: Language,    // Язык открытого аккаунта.
    cx: &'a Context<'a, Conversation>, // Доступ к экрану, который исполняет действия.
}

impl KeyManagement {
    /// Показывает текущий ключ и открытые запросы в основных сведениях.
    pub(in super::super) fn render_info(
        &self,
        panel: &Panel,
        palette: Palette,
        language: Language,
        cx: &Context<Conversation>,
    ) -> Div {
        let Self::Ready(keys) = self else {
            return self.feedback(palette, language, cx);
        };
        let view = KeyView {
            keys,
            panel,
            palette,
            language,
            cx,
        };
        let incoming = view.requests(RequestSection::Incoming);
        section(palette)
            .child(view.heading("e2ee.keyTitle"))
            .child(view.summary())
            .children(view.error())
            .when(!incoming.is_empty(), |body| {
                body.child(
                    section(palette)
                        .child(view.count_heading("e2ee.requestsTitle", incoming.len()))
                        .child(view.history_hint())
                        .child(view.request_list(incoming)),
                )
            })
    }

    /// Показывает текущее поколение, сохранённые ключи и свёрнутые разделы запросов.
    pub(in super::super) fn render_keys(
        &self,
        panel: &Panel,
        palette: Palette,
        language: Language,
        cx: &Context<Conversation>,
    ) -> Div {
        let Self::Ready(keys) = self else {
            return self.feedback(palette, language, cx);
        };
        let view = KeyView {
            keys,
            panel,
            palette,
            language,
            cx,
        };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(view.summary())
            .child(view.local_keys())
            .children(view.error())
            .when(!keys.bulk.is_empty(), |body| {
                body.child(view.history_hint()).children(
                    RequestSection::ALL
                        .into_iter()
                        .filter_map(|kind| view.history(kind)),
                )
            })
    }

    /// Показывает метаданные одного локального ключа и действия над ним.
    pub(in super::super) fn render_local_key(
        &self,
        id: Uuid,
        panel: &Panel,
        palette: Palette,
        language: Language,
        cx: &Context<Conversation>,
    ) -> Div {
        let Self::Ready(keys) = self else {
            return self.feedback(palette, language, cx);
        };
        let view = KeyView {
            keys,
            panel,
            palette,
            language,
            cx,
        };
        let Some(key) = keys.data.keys.iter().find(|key| key.key_id == id) else {
            return div().px_2().child(view.text("e2ee.localKeyUnavailable"));
        };
        let opener = keys.remove_focus.clone();
        let mut remove = Button::new(
            (ElementId::from(id), "remove-chat-key"),
            language.tr("e2ee.deleteLocalKey"),
        );
        remove.appearance = ButtonAppearance::Ghost;
        remove.disabled = keys.operation.is_some();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .px_2()
            .debug_selector(|| "conversation-local-key-details".into())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(view.text("e2ee.localKeyUuid"))
                    .child(
                        label(palette, TextRole::Body, id.to_string())
                            .rounded(PANEL_RADIUS)
                            .bg(palette.card)
                            .px_2()
                            .py_1(),
                    )
                    .child(view.key_kind(id)),
            )
            .child(
                view.date_field(
                    "e2ee.localKeyAnnouncedLabel",
                    view.announced_date(id).unwrap_or_else(|| {
                        language.tr("e2ee.localKeyAnnouncementUnknown").to_owned()
                    }),
                ),
            )
            .child(view.date_field("e2ee.localKeyAddedLabel", view.added_date(key)))
            .child(view.request_key(id, true))
            .child(
                remove
                    .render(
                        palette,
                        Some(&keys.remove_focus),
                        action(cx, move |conversation, window, cx| {
                            conversation.confirm_key_change(
                                Change::Remove(id),
                                opener.clone(),
                                window,
                                cx,
                            );
                        }),
                    )
                    .debug_selector(move || format!("conversation-remove-key-{id}")),
            )
            .children(view.error())
    }

    /// Показывает загрузку ключей или возможность повторить неудачное чтение.
    fn feedback(&self, palette: Palette, language: Language, cx: &Context<Conversation>) -> Div {
        let mut body = div().flex().flex_col().items_start().gap_2().px_2();
        if matches!(self, Self::Failed) {
            body = body
                .child(label(
                    palette,
                    TextRole::Caption,
                    language.tr("e2ee.localKeysLoadFailed"),
                ))
                .child(
                    Button::new("retry-sidebar-keys", language.tr("e2ee.retry")).render(
                        palette,
                        None,
                        action(cx, |conversation, _, cx| {
                            conversation.load_key_management(cx)
                        }),
                    ),
                );
        } else {
            body = body.child(label(
                palette,
                TextRole::Caption,
                language.tr(if matches!(self, Self::Loading(_)) {
                    "e2ee.loading"
                } else {
                    "e2ee.cryptoUnavailable"
                }),
            ));
        }
        body
    }
}

impl KeyView<'_> {
    /// Рисует идентификатор текущего поколения, копирование и ротацию.
    fn summary(&self) -> Div {
        let palette = self.palette;
        let id = self.keys.data.current_key_id;
        let copy_title = self.language.tr("e2ee.copyKey");
        let mut copy = Button::new("copy-sidebar-key-id", "");
        copy.appearance = ButtonAppearance::Ghost;
        copy.icon = Some(Icon::Copy);
        let opener = self.keys.rotate_focus.clone();
        let mut rotate = Button::new(
            "conversation-rotate-key",
            self.language.tr("e2ee.rotateKey"),
        );
        rotate.appearance = ButtonAppearance::Primary;
        rotate.disabled = self.keys.operation.is_some();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_2()
            .debug_selector(|| "conversation-key-summary".into())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(self.text("e2ee.keyUuid"))
                    .child(
                        copy.render(palette, None, move |_, app| {
                            app.write_to_clipboard(ClipboardItem::new_string(id.to_string()));
                        })
                        .tooltip(move |_, app| tooltip(palette, copy_title.into(), app)),
                    ),
            )
            .child(
                label(palette, TextRole::Body, id.to_string())
                    .rounded(PANEL_RADIUS)
                    .bg(palette.card)
                    .px_2()
                    .py_1(),
            )
            .children(
                self.announced_date(id)
                    .map(|date| label(palette, TextRole::Caption, date)),
            )
            .when(self.keys.data.can_rotate, |body| {
                body.child(
                    rotate
                        .render(
                            palette,
                            Some(&self.keys.rotate_focus),
                            action(self.cx, move |conversation, window, cx| {
                                conversation.confirm_key_change(
                                    Change::Rotate(id),
                                    opener.clone(),
                                    window,
                                    cx,
                                );
                            }),
                        )
                        .debug_selector(|| "conversation-rotate-key".into()),
                )
            })
    }

    /// Рисует недостающее поколение и раскрываемый список сохранённых ключей.
    fn local_keys(&self) -> Div {
        let id = self.keys.data.current_key_id;
        let missing = !self.keys.data.keys.iter().any(|key| key.key_id == id);
        let expanded = self.keys.expanded;
        let count = self.keys.data.keys.len().to_string();
        section(self.palette)
            .when(missing, |body| {
                body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .px_2()
                        .child(self.text("e2ee.currentKeyMissing"))
                        .child(self.request_key(id, false)),
                )
            })
            .child(accordion(
                "conversation-local-keys",
                self.language
                    .tr_with("e2ee.localKeysTitle", &[("count", &count)]),
                expanded,
                self.palette,
                action(self.cx, |conversation, _, cx| {
                    if let Some(KeyManagement::Ready(keys)) =
                        conversation.details.as_mut().map(|panel| &mut panel.keys)
                    {
                        keys.expanded = !keys.expanded;
                        cx.notify();
                    }
                }),
            ))
            .when(expanded, |body| {
                body.when(self.keys.data.keys.is_empty(), |body| {
                    body.child(self.text("e2ee.noLocalKeys").px_2())
                })
                .children(
                    self.keys
                        .data
                        .keys
                        .iter()
                        .filter_map(|key| self.local_key_row(key)),
                )
            })
    }

    /// Открывает сведения о выбранном поколении и передаёт фокус открывшей строки.
    fn local_key_row(&self, key: &HeldChatKey) -> Option<Stateful<Div>> {
        let palette = self.palette;
        let id = key.key_id;
        let focus = self.keys.key_focus.get(&id)?.clone();
        let open = focus.clone();
        let uuid = id.simple().to_string();
        let short = format!("{}…{}", &uuid[..8], &uuid[28..]);
        let date = self.added_date(key);
        Some(activate(
            div()
                .id((ElementId::from(id), "sidebar-local-key"))
                .flex()
                .items_center()
                .justify_between()
                .px_2()
                .py_2()
                .rounded(PANEL_RADIUS)
                .hover(|style| style.bg(palette.card_hover))
                .debug_selector(move || format!("conversation-local-key-{id}"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(label(palette, TextRole::Caption, short))
                                .child(self.key_kind(id)),
                        )
                        .child(label(
                            palette,
                            TextRole::Caption,
                            self.language
                                .tr_with("e2ee.localKeyAdded", &[("date", &date)]),
                        )),
                )
                .child(Icon::ChevronRight.element(px(16.), palette.icon)),
            Some(&focus),
            palette,
            action(self.cx, move |conversation, window, cx| {
                conversation.open_encrypted_screen(
                    EncryptedScreen::LocalKey(id),
                    open.clone(),
                    window,
                    cx,
                );
            }),
        ))
    }

    /// Даёт запросить отсутствующий или повторно запросить сохранённый ключ.
    fn request_key(&self, id: Uuid, local: bool) -> Div {
        let (selector, title, waiting) = if local {
            (
                "request-saved-chat-key",
                "e2ee.requestLocalKey",
                "e2ee.localKeyRequested",
            )
        } else {
            (
                "request-current-chat-key",
                "e2ee.requestCurrentKey",
                "e2ee.currentKeyRequested",
            )
        };
        let request = self.keys.requests.get(&id);
        let pending = matches!(request, Some(RequestedKey::Waiting(_)));
        let mut button = Button::new(selector, self.language.tr(title));
        button.disabled = pending || self.keys.operation.is_some();
        button.appearance = ButtonAppearance::Ghost;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                button
                    .render(
                        self.palette,
                        None,
                        action(self.cx, move |conversation, _, cx| {
                            conversation.request_sidebar_key(id, cx)
                        }),
                    )
                    .border_color(self.palette.border)
                    .debug_selector(move || selector.into()),
            )
            .when(pending, |body| body.child(self.text(waiting)))
            .when(request == Some(&RequestedKey::Rejected), |body| {
                body.child(self.text("e2ee.keyRejectTitle"))
            })
    }

    /// Показывает раздел истории с непустым списком.
    fn history(&self, kind: RequestSection) -> Option<Div> {
        let rows = self.requests(kind);
        if rows.is_empty() {
            return None;
        }
        let index = kind as usize;
        let expanded = self.keys.request_sections[index];
        Some(
            div()
                .flex()
                .flex_col()
                .child(accordion(
                    kind.selector(),
                    self.language
                        .tr_with(kind.title(), &[("count", &rows.len().to_string())]),
                    expanded,
                    self.palette,
                    action(self.cx, move |conversation, _, cx| {
                        if let Some(KeyManagement::Ready(keys)) =
                            conversation.details.as_mut().map(|panel| &mut panel.keys)
                        {
                            keys.request_sections[index] = !keys.request_sections[index];
                            cx.notify();
                        }
                    }),
                ))
                .when(expanded, |section| section.child(self.request_list(rows))),
        )
    }

    /// Берёт запросы одного раздела в порядке загруженной истории.
    fn requests(&self, kind: RequestSection) -> Vec<&PendingKeyGrant> {
        self.keys
            .bulk
            .iter()
            .filter(|row| kind.includes(row))
            .collect()
    }

    /// Рисует пакетное действие, если есть доступные запросы, и список устройств.
    fn request_list(&self, rows: Vec<&PendingKeyGrant>) -> Div {
        let all = rows
            .iter()
            .any(|row| row.can_respond(KeyRequestOutcome::Granted));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .when(all, |body| {
                let mut grant =
                    Button::new("bulk-grant-chat-keys", self.language.tr("e2ee.grantAll"));
                grant.appearance = ButtonAppearance::Ghost;
                grant.disabled = self.keys.operation.is_some();
                body.child(
                    div().flex().justify_end().px_2().child(
                        grant
                            .render(
                                self.palette,
                                None,
                                action(self.cx, |conversation, _, cx| {
                                    conversation.respond_to_sidebar_key_requests(
                                        None,
                                        KeyRequestOutcome::Granted,
                                        cx,
                                    );
                                }),
                            )
                            .debug_selector(|| "bulk-grant-chat-keys".into()),
                    ),
                )
            })
            .children(rows.into_iter().map(|row| self.request_row(row)))
    }

    /// Показывает подтверждённый результат либо доступные действия для устройства.
    fn request_row(&self, row: &PendingKeyGrant) -> Div {
        let palette = self.palette;
        let id = row.hint.request_id;
        let (name, avatar) = self.request_author(row.author_id);
        let selector = match row.state {
            GrantState::Completed(KeyRequestOutcome::Granted) => "granted",
            GrantState::Completed(KeyRequestOutcome::Rejected) => "rejected",
            _ => "pending",
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .min_h(px(56.))
            .debug_selector(move || format!("bulk-key-result-{id}"))
            .child(avatar.render(palette))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .flex_col()
                    .debug_selector(move || format!("bulk-key-{selector}-{id}"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(Icon::BusinessCenter.element(px(16.), palette.icon))
                            .child(div().min_w_0().flex_1().child(ellipsized_label(
                                palette,
                                TextRole::Body,
                                name.clone(),
                            ))),
                    )
                    .child(ellipsized_label(
                        palette,
                        TextRole::Caption,
                        row.hint.device_name.clone(),
                    ))
                    .when(row.state == GrantState::Sending, |body| {
                        body.child(self.text("message.sending"))
                    })
                    .when(matches!(row.state, GrantState::Failed(_)), |body| {
                        body.child(
                            self.text(
                                "desktop.conversation.encryption.actionCouldNotBeConfirmedTryAgain",
                            )
                            .text_color(palette.danger),
                        )
                    }),
            )
            .children(
                [
                    (
                        KeyRequestOutcome::Granted,
                        Icon::Check,
                        "e2ee.grantRequest",
                        "grant",
                    ),
                    (
                        KeyRequestOutcome::Rejected,
                        Icon::Close,
                        "e2ee.rejectRequest",
                        "reject",
                    ),
                ]
                .into_iter()
                .filter(|(outcome, ..)| row.can_respond(*outcome))
                .map(|(outcome, icon, key, button_id)| {
                    let description = self.language.tr_with(key, &[("name", &name)]);
                    let mut button = Button::new((ElementId::from(id), button_id), "");
                    button.appearance = ButtonAppearance::Ghost;
                    button.icon = Some(icon);
                    button.disabled = self.keys.operation.is_some();
                    button
                        .render(
                            palette,
                            None,
                            action(self.cx, move |conversation, _, cx| {
                                conversation.respond_to_sidebar_key_requests(Some(id), outcome, cx);
                            }),
                        )
                        .tooltip(move |_, app| tooltip(palette, description.clone().into(), app))
                }),
            )
    }

    /// Использует уже загруженные сведения о подтверждённом авторе запроса.
    fn request_author(&self, id: Uuid) -> (String, Avatar) {
        let profile = match self.panel.snapshot.as_deref() {
            Some(SidebarDetails::Channel(channel)) => channel
                .members
                .iter()
                .find(|member| member.user_id == id)
                .and_then(|member| member.profile.as_ref()),
            _ => None,
        };
        let name = profile
            .map(|profile| profile.display_name.clone())
            .unwrap_or_else(|| id.to_string());
        let initials = profile
            .and_then(|profile| profile.display_name.chars().next())
            .map(|letter| letter.to_uppercase().collect::<String>())
            .unwrap_or_default();
        let presence = match profile.and_then(|profile| profile.presence) {
            Some(ProfilePresence::Active) => AvatarPresence::Online,
            Some(ProfilePresence::Idle) => AvatarPresence::Away,
            Some(ProfilePresence::Offline | ProfilePresence::DoNotDisturb) => {
                AvatarPresence::Offline
            }
            None => AvatarPresence::Hidden,
        };
        let avatar = Avatar {
            size: AvatarSize::Small,
            image: self.panel.avatars.image(id, self.cx),
            presence,
            ..Avatar::from_initials(initials)
        };
        (name, avatar)
    }

    /// Рисует верхнюю подпись раздела.
    fn heading(&self, key: &'static str) -> Div {
        label(self.palette, TextRole::Heading, self.language.tr(key)).px_2()
    }

    /// Рисует верхнюю подпись с числом запросов.
    fn count_heading(&self, key: &'static str, count: usize) -> Div {
        label(
            self.palette,
            TextRole::Heading,
            self.language.tr_with(key, &[("count", &count.to_string())]),
        )
        .px_2()
    }

    /// Напоминает, что список составлен по сообщениям в локальной истории.
    fn history_hint(&self) -> Div {
        self.text("e2ee.loadedHistoryHint").px_2()
    }

    /// Рисует подпись рядом с элементами управления.
    fn text(&self, key: &'static str) -> Div {
        label(self.palette, TextRole::Caption, self.language.tr(key))
    }

    /// Показывает проблему после неподтверждённой операции.
    fn error(&self) -> Option<Div> {
        self.keys.error.then(|| {
            self.text("desktop.conversation.encryption.actionCouldNotBeConfirmedTryAgain")
                .text_color(self.palette.danger)
                .px_2()
        })
    }

    /// Отличает актуальный ключ от сохранённых исторических поколений.
    fn key_kind(&self, id: Uuid) -> Div {
        if id == self.keys.data.current_key_id {
            self.text("e2ee.localKeyCurrent")
                .text_color(self.palette.accent)
        } else {
            self.text("e2ee.localKeyHistorical")
        }
    }

    /// Форматирует дату сохранения или показывает, что запись не содержит даты.
    fn added_date(&self, key: &HeldChatKey) -> String {
        key.added_at_unix_ms
            .map(crate::platform::chat_keys::key_date)
            .unwrap_or_else(|| self.language.tr("e2ee.localKeyDateUnknown").into())
    }

    /// Форматирует дату объявления, если сообщение уже есть в истории.
    fn announced_date(&self, id: Uuid) -> Option<String> {
        self.keys
            .data
            .announced_at
            .get(&id)
            .copied()
            .map(crate::platform::chat_keys::key_date)
    }

    /// Рисует название даты над её значением.
    fn date_field(&self, key: &'static str, value: impl Into<SharedString>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(self.text(key))
            .child(label(self.palette, TextRole::Body, value))
    }
}

/// Отделяет соседние группы сведений общей границей.
fn section(palette: Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .border_t_1()
        .border_color(palette.border)
        .pt_3()
}

/// Рисует растягиваемую кнопку свёрнутого раздела с направлением раскрытия.
fn accordion(
    id: &'static str,
    title: String,
    expanded: bool,
    palette: Palette,
    on_activate: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    let chevron = if expanded {
        Icon::ChevronUp
    } else {
        Icon::ChevronDown
    };
    activate(
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_between()
            .min_h(px(32.))
            .gap_2()
            .px_2()
            .rounded(PANEL_RADIUS)
            .hover(|style| style.bg(palette.card_hover))
            .child(label(palette, TextRole::Label, title))
            .child(chevron.element(px(20.), palette.icon))
            .debug_selector(move || id.into()),
        None,
        palette,
        on_activate,
    )
}
