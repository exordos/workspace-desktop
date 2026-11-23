//! Показывает фильтры, строки каналов и состояния нижнего списка.
//! Запросы и переходы по строкам остаются у панели.

use gpui::{Context, Div, SharedString, Stateful, Window, div, prelude::*};
use workspace_core::messenger::catalog::{Chat, NewChatCatalog};

use crate::gpui_client::{
    preferences::Language,
    ui::{
        Avatar, Button, ChatRow, Icon, PANEL_RADIUS, Palette, TextRole, filter_button, label,
        popup::tooltip,
    },
};

use super::{Load, NewChats, action, people::initials};

/// Фильтры нижнего каталога в порядке, принятом для Desktop.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Filter {
    Subscribed,   // Активные общие каналы, где у аккаунта есть членство.
    Unsubscribed, // Каналы, доступные для обнаружения без членства.
    All,          // Активные каналы и лички.
    Archive,      // Архивные каналы и лички.
}

impl Filter {
    pub(super) const ALL: [Self; 4] = [
        Self::Subscribed,
        Self::Unsubscribed,
        Self::All,
        Self::Archive,
    ];

    /// Превращает внутренний ключ в подпись для нужного языка.
    fn title(self, language: Language) -> &'static str {
        match self {
            Self::Subscribed => language.text("С подпиской", "Subscribed"),
            Self::Unsubscribed => language.text("Без подписки", "Unsubscribed"),
            Self::All => language.text("Все", "All"),
            Self::Archive => language.text("Архив", "Archive"),
        }
    }

    /// Возвращает стабильный ключ кнопки фильтра.
    fn id(self) -> &'static str {
        match self {
            Self::Subscribed => "new-chats-filter-subscribed",
            Self::Unsubscribed => "new-chats-filter-unsubscribed",
            Self::All => "new-chats-filter-all",
            Self::Archive => "new-chats-filter-archive",
        }
    }

    /// Берёт часть подтверждённого каталога, связанную с выбранным фильтром.
    pub(super) fn rows(self, catalog: &NewChatCatalog) -> &[Chat] {
        match self {
            Self::Subscribed | Self::All => &catalog.active,
            Self::Archive => &catalog.archived,
            Self::Unsubscribed => &[],
        }
    }

    /// Разделяет активные лички и каналы для вкладки «С подпиской».
    pub(super) fn includes(self, chat: &Chat) -> bool {
        self != Self::Subscribed || !chat.direct
    }
}

impl NewChats {
    /// Рисует один ряд фильтров в том же порядке при любой ширине.
    pub(super) fn catalog_filters(
        &self,
        palette: Palette,
        language: Language,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        self.filter_group.render(
            palette,
            window,
            Filter::ALL.into_iter().enumerate().map(|(index, filter)| {
                filter_button(
                    filter.id(),
                    filter.title(language),
                    self.filter == filter,
                    palette,
                    Some(self.filter_group.focus(index)),
                    action(cx, move |view, _, cx| {
                        view.filter = filter;
                        cx.notify();
                    }),
                )
                .debug_selector(move || filter.id().into())
            }),
        )
    }

    /// Показывает состояние запроса или доступные строки текущего фильтра.
    pub(super) fn catalog_list(
        &self,
        palette: Palette,
        language: Language,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let list = div()
            .id(SharedString::from(format!(
                "new-chats-list-{}",
                self.filter.id()
            )))
            .debug_selector(|| "new-chats-channel-list".into())
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        let status = |text| label(palette, TextRole::Caption, text).py_2();
        if self.filter == Filter::Unsubscribed {
            return list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .p_3()
                    .rounded(PANEL_RADIUS)
                    .bg(palette.card)
                    .child(label(
                        palette,
                        TextRole::Label,
                        language.text("В работе", "In progress"),
                    ))
                    .child(label(
                        palette,
                        TextRole::Caption,
                        language.text(
                            "Скоро здесь появятся каналы, на которые вы ещё не подписаны",
                            "Channels you haven’t subscribed to will appear here soon",
                        ),
                    ))
                    .debug_selector(|| "new-chats-unsubscribed-unavailable".into()),
            );
        }
        let Load::Ready(catalog) = &self.catalog else {
            return if self.catalog.failed() {
                list.child(status(
                    language.text("Не удалось загрузить чаты", "Could not load chats"),
                ))
                .child(
                    Button::new(
                        "new-chats-retry-catalog",
                        language.text("Повторить", "Retry"),
                    )
                    .render(
                        palette,
                        None,
                        action(cx, |view, _, cx| view.load_catalog(cx)),
                    )
                    .debug_selector(|| "new-chats-retry-catalog".into()),
                )
            } else {
                list.child(status(language.text("Загрузка…", "Loading…")))
            };
        };
        let term = self.search.read(cx).text().trim().to_lowercase();
        let mut rows = self
            .filter
            .rows(catalog)
            .iter()
            .filter(|chat| {
                self.filter.includes(chat)
                    && [
                        Some(chat.name.as_str()),
                        chat.metadata.direct_peer_username.as_deref(),
                        chat.metadata.direct_peer_email.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|value| value.to_lowercase().contains(&term))
            })
            .peekable();
        if rows.peek().is_none() {
            return list.child(status(
                language.text("Ничего не найдено", "No results found"),
            ));
        }
        list.when(
            self.filter == Filter::Archive && self.restore_failed,
            |list| {
                list.child(
                    status(
                        language.text("Не удалось восстановить чат", "Could not restore the chat"),
                    )
                    .text_color(palette.danger),
                )
            },
        )
        .children(rows.map(|chat| self.catalog_row(chat, palette, language, cx)))
    }

    /// Переиспользует карточку каталога и добавляет к архиву отдельное восстановление.
    fn catalog_row(
        &self,
        chat: &Chat,
        palette: Palette,
        language: Language,
        cx: &Context<Self>,
    ) -> Div {
        let id = chat.id;
        let archive = self.filter == Filter::Archive;
        let selector = format!("new-chats-stream-{id}");
        let details = self
            .session
            .as_ref()
            .map(|session| super::super::stream_details(chat, session))
            .unwrap_or_default();
        let row = ChatRow {
            id: SharedString::from(selector.clone()).into(),
            avatar: Avatar::from_initials(initials(&chat.name)),
            title: chat.name.clone().into(),
            preview: details.preview,
            time: details.time,
            unread: details.unread,
            mention: false,
            selected: false,
            disabled: archive && !chat.direct && chat.topics.is_empty(),
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .child(
                div().min_w_0().flex_1().child(
                    row.render(
                        palette,
                        None,
                        action(cx, move |view, _, cx| view.navigate_row(id, cx)),
                    )
                    .debug_selector(move || selector.clone()),
                ),
            )
            .when(archive, |row| {
                let selector = format!("new-chats-restore-{id}");
                let mut restore = Button::new(SharedString::from(selector.clone()), "");
                restore.icon = Some(Icon::Reply);
                restore.disabled = self.restoring.is_some();
                row.child(
                    restore
                        .render(
                            palette,
                            None,
                            action(cx, move |view, _, cx| view.restore(id, cx)),
                        )
                        .flex_none()
                        .tooltip(move |_, app| {
                            tooltip(
                                palette,
                                language.text("Восстановить", "Restore").into(),
                                app,
                            )
                        })
                        .debug_selector(move || selector.clone()),
                )
            })
    }
}
