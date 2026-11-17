//! Показывает каталог выбранного входа, хранит поиск и раскрытие каналов.
//! Строки передают оболочке адрес разговора; запросы остаются у родителя.

use std::collections::{HashMap, HashSet};

use gpui::{
    App, Context, Div, ElementId, Entity, EventEmitter, FocusHandle, KeyBinding, Render,
    ScrollHandle, SharedString, Window, div, prelude::*, px,
};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;
use workspace_core::{
    bootstrap::{ChatList, ChatListStatus},
    messenger::catalog::{Chat, Topic},
    storage::HistoryScope,
};

use super::{
    contracts::{AccountSession, ChatEntry, ChatRoute, ConversationAddress, SidebarAction},
    ui::{
        Avatar, AvatarPresence, BadgeTone, Button, ButtonAppearance, ChatRow, ChatRowPreview, Icon,
        Palette, TextRole, ThemeMode, label,
    },
};
use search::{QueryChanged, Search};

mod search;

#[cfg(all(test, feature = "gpui-test"))]
mod tests;

gpui::actions!(catalog_results, [PreviousResult, NextResult]);

/// Состояние каталога в окне. Оболочка передаёт выбранный вход и актуальный снимок.
pub struct Sidebar {
    active: Option<Active>, // Показанный каталог или ожидание выбора входа.
    saved: HashMap<AccountSession, Preferences>, // Поиск и раскрытые каналы других входов.
}

/// Состояние, которое можно вернуть при повторном выборе того же входа.
#[derive(Default)]
struct Preferences {
    query: String,           // Последний запрос без нормализации для отображения в поле.
    expanded: HashSet<Uuid>, // Каналы, которые были раскрыты.
}

/// Каталог одного выбранного входа и состояние только его текущего показа.
struct Active {
    session: AccountSession,              // Владелец данных и поколение входа.
    catalog: Option<ChatList>,            // Нет снимка до начала или завершения первого чтения.
    selected: Option<HistoryScope>,       // Разговор, который сейчас открыт в окне.
    search: Entity<Search>,               // Поле системного ввода, живущее до смены входа.
    _search_observer: gpui::Subscription, // Подписка на изменение поискового запроса.
    expanded: HashSet<Uuid>,              // Каналы, раскрытые вне режима поиска.
    scroll: ScrollHandle,                 // Позиция прокрутки только текущего показа.
    focus: HashMap<RowKey, FocusHandle>,  // Фокус строки остаётся у UUID при перестановке.
    #[cfg(all(test, feature = "gpui-test"))]
    fixture_details: HashMap<RowKey, RowDetails>, // Синтетические метаданные этого показа.
}

/// Устойчивый ключ строки внутри каталога.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RowKey {
    Chat(Uuid),        // UUID личного чата или канала.
    Topic(Uuid, Uuid), // UUID канала и его темы.
}

/// Готовые визуальные сведения строки. Каталог core пока передаёт только имя и темы.
#[derive(Clone, Default)]
struct RowDetails {
    preview: Option<ChatRowPreview>, // Превью последнего сообщения, если источник его передал.
    time: Option<SharedString>,      // Подпись времени последнего сообщения.
    unread: Option<(u64, BadgeTone)>, // Число и стиль бейджа для именно этого разговора.
    mention: bool,                   // Есть непрочитанное личное упоминание.
    presence: AvatarPresence,        // Статус собеседника, если он известен.
}

impl Active {
    /// Берёт метаданные только для текущего владельца. Без источника поля остаются пустыми.
    fn row_details(&self, _key: RowKey) -> RowDetails {
        #[cfg(all(test, feature = "gpui-test"))]
        if let Some(details) = self.fixture_details.get(&_key) {
            return details.clone();
        }
        RowDetails::default()
    }

    /// Переносит фокус между реальными результатами, не вмешиваясь в системный ввод.
    fn move_result(
        &self,
        results: &[(usize, FocusHandle)],
        previous: bool,
        window: &mut Window,
        cx: &mut Context<Sidebar>,
    ) {
        let search = self.search.read(cx);
        let index = if search.is_focused(window) {
            // Стрелки во время набора IME принадлежат системному списку кандидатов.
            if search.is_composing() {
                cx.propagate();
                return;
            }
            if previous {
                results.len().checked_sub(1)
            } else {
                Some(0)
            }
        } else if let Some(index) = results
            .iter()
            .position(|(_, focus)| focus.is_focused(window))
        {
            if previous {
                index.checked_sub(1)
            } else {
                Some(index + 1)
            }
        } else {
            cx.propagate();
            return;
        };
        if let Some((item, focus)) = index.and_then(|index| results.get(index)) {
            self.scroll.scroll_to_item(*item);
            window.focus(focus);
            cx.notify();
        }
    }
}

impl Sidebar {
    /// Создаёт каталог без выбранного входа.
    pub fn new(_: &mut Context<Self>) -> Self {
        Self {
            active: None,
            saved: HashMap::new(),
        }
    }

    /// Регистрирует команды редактирования поиска один раз при запуске GPUI.
    pub fn bind_keys(app: &mut App) {
        Search::bind_keys(app);
        app.bind_keys([
            KeyBinding::new("up", PreviousResult, Some("CatalogResults")),
            KeyBinding::new("down", NextResult, Some("CatalogResults")),
        ]);
    }

    /// Передаёт снимок оболочки. Запрос и раскрытие сохраняются только в пределах того же входа.
    pub fn update_snapshot(
        &mut self,
        session: Option<&AccountSession>,
        catalog: Option<&ChatList>,
        route: Option<&ConversationAddress>,
        cx: &mut Context<Self>,
    ) {
        let current = self.active.as_ref().map(|active| &active.session);
        let switched = current != session;
        if switched {
            if let Some(previous) = self.active.take() {
                self.saved.insert(
                    previous.session,
                    Preferences {
                        query: previous.search.read(cx).text().to_owned(),
                        expanded: previous.expanded,
                    },
                );
            }
            if let Some(session) = session {
                // После повторного входа состояние старого поколения уже не пригодится.
                self.saved.retain(|key, _| {
                    key.owner != session.owner || key.generation == session.generation
                });
                let preferences = self.saved.remove(session).unwrap_or_default();
                let search = cx.new(|cx| Search::new(preferences.query, cx));
                let observer = cx.subscribe(&search, |this, _, _: &QueryChanged, cx| {
                    if let Some(active) = &this.active {
                        active.scroll.set_offset(gpui::point(px(0.), px(0.)));
                    }
                    cx.notify();
                });
                self.active = Some(Active {
                    session: session.clone(),
                    catalog: None,
                    selected: None,
                    search,
                    _search_observer: observer,
                    expanded: preferences.expanded,
                    scroll: ScrollHandle::new(),
                    focus: HashMap::new(),
                    #[cfg(all(test, feature = "gpui-test"))]
                    fixture_details: HashMap::new(),
                });
            }
        }

        if let Some(active) = &mut self.active {
            let catalog = catalog.filter(|list| list.generation == active.session.generation);
            let selected = route
                .filter(|address| {
                    address.route.owner == active.session.owner
                        && address.generation == active.session.generation
                })
                .map(|address| address.route.scope);
            if active.selected != selected {
                active.selected = selected;
                if let Some(scope) = selected.filter(|scope| scope.topic_id.is_some()) {
                    active.expanded.insert(scope.stream_id);
                }
            } else if !switched && same_catalog(active.catalog.as_ref(), catalog) {
                return;
            }

            active.catalog = catalog.cloned();
            if let Some(chats) = catalog.and_then(|list| list.chats.as_deref()) {
                let keys = chats
                    .iter()
                    .flat_map(|chat| {
                        std::iter::once(RowKey::Chat(chat.id)).chain(
                            chat.topics
                                .iter()
                                .map(|topic| RowKey::Topic(chat.id, topic.id)),
                        )
                    })
                    .collect::<HashSet<_>>();
                active.focus.retain(|key, _| keys.contains(key));
            }
            cx.notify();
        } else if switched {
            cx.notify();
        }
    }

    /// Принимает активацию только от строки, которая ещё видна в том же входе.
    fn activate(&mut self, origin: &AccountSession, key: RowKey, cx: &mut Context<Self>) {
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| &active.session == origin)
        else {
            return;
        };
        let Some(chats) = active
            .catalog
            .as_ref()
            .and_then(|list| list.chats.as_deref())
        else {
            return;
        };
        let stream_id = match key {
            RowKey::Chat(id) | RowKey::Topic(id, _) => id,
        };
        let Some(chat) = chats.iter().find(|chat| chat.id == stream_id) else {
            return;
        };
        let query = active.search.read(cx).text().trim().to_lowercase();
        let chat_matches = matches_query(&chat.name, &query);
        let topic_id = match key {
            RowKey::Chat(_)
                if chat.direct
                    && (chat_matches
                        || chat
                            .topics
                            .iter()
                            .any(|topic| matches_query(&topic.name, &query))) =>
            {
                None
            }
            RowKey::Chat(_) if !chat.direct && query.is_empty() && !chat.topics.is_empty() => {
                if !active.expanded.remove(&stream_id) {
                    active.expanded.insert(stream_id);
                }
                cx.notify();
                return;
            }
            RowKey::Topic(_, id)
                if !chat.direct
                    && (!query.is_empty() || active.expanded.contains(&stream_id))
                    && chat.topics.iter().any(|topic| {
                        topic.id == id && (chat_matches || matches_query(&topic.name, &query))
                    }) =>
            {
                Some(id)
            }
            _ => return,
        };
        cx.emit(SidebarAction::OpenChat {
            route: ChatRoute {
                owner: origin.owner.clone(),
                scope: HistoryScope {
                    stream_id,
                    topic_id,
                },
                entry: ChatEntry::Latest,
            },
            generation: origin.generation,
        });
    }

    /// Повторяет только загрузку, которая всё ещё показывает ошибку.
    fn retry(&self, origin: &AccountSession, cx: &mut Context<Self>) {
        if self.active.as_ref().is_some_and(|active| {
            &active.session == origin
                && active
                    .catalog
                    .as_ref()
                    .is_some_and(|list| list.status == ChatListStatus::Failed)
        }) {
            cx.emit(SidebarAction::RetryCatalog(origin.clone()));
        }
    }

    /// Рисует строку чата и передаёт её нажатие с ключом владельца.
    fn chat_row(
        active: &mut Active,
        chat: &Chat,
        opened: bool,
        expandable: bool,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = chat.id;
        let key = RowKey::Chat(id);
        let details = active.row_details(key);
        let handle = active.focus.entry(key).or_insert_with(|| cx.focus_handle());
        let entity = cx.entity().downgrade();
        let origin = active.session.clone();
        let selected = active.selected.is_some_and(|scope| {
            scope.stream_id == chat.id && (scope.topic_id.is_none() || !opened)
        });
        let initials = chat
            .name
            .trim_start()
            .graphemes(true)
            .next()
            .unwrap_or_default();
        let chevron = expandable.then(|| {
            let icon = if opened {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            };
            div()
                .size(px(20.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(icon.element(px(16.), palette.icon_hover))
        });
        let mut avatar = Avatar::from_initials(initials.to_owned());
        avatar.presence = details.presence;
        let row = ChatRow {
            id: ElementId::Uuid(chat.id),
            avatar,
            title: chat.name.clone().into(),
            preview: details.preview,
            time: details.time,
            unread: details.unread,
            mention: details.mention,
            selected,
            disabled: !chat.direct && !expandable,
        }
        .render_with_action(palette, Some(handle), chevron, move |_, app| {
            let _ = entity.update(app, |this, cx| this.activate(&origin, key, cx));
        })
        .min_h(px(64.))
        .debug_selector(move || format!("catalog-chat-{id}"));
        div().w_full().min_w_0().child(row)
    }

    /// Рисует тему как компактную выбираемую строку внутри канала.
    fn topic_row(
        active: &mut Active,
        stream_id: Uuid,
        topic: &Topic,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let key = RowKey::Topic(stream_id, topic.id);
        let details = active.row_details(key);
        let handle = active.focus.entry(key).or_insert_with(|| cx.focus_handle());
        let entity = cx.entity().downgrade();
        let origin = active.session.clone();
        let selected = active.selected
            == Some(HistoryScope {
                stream_id,
                topic_id: Some(topic.id),
            });
        let id = format!("catalog-topic-{stream_id}-{}", topic.id);
        let title = topic.name.trim();
        let title = if title.is_empty() || title.starts_with('#') {
            title.to_owned().into()
        } else {
            format!("#{title}").into()
        };
        let button = Button::new(ElementId::Name(id.clone().into()), "");
        let row = button
            .render(palette, Some(handle), move |_, app| {
                let _ = entity.update(app, |this, cx| this.activate(&origin, key, cx));
            })
            .w_full()
            .min_w_0()
            .h_auto()
            .min_h(px(38.))
            .pl(px(37.))
            .pr(px(7.))
            .py(px(7.))
            .map(|mut row| {
                // Текст и цветная полоса занимают всю высоту темы.
                row.style().align_items = Some(gpui::AlignItems::Stretch);
                row
            })
            .gap(px(12.))
            .when(selected, |row| row.bg(palette.card_selected))
            .child(
                div()
                    .w(px(3.))
                    .flex_none()
                    .rounded_full()
                    .bg(palette.icon)
                    .debug_selector(|| "catalog-topic-bar".into()),
            )
            .child(ChatRow::text(palette, title, details.preview))
            .children(ChatRow::metadata(
                palette,
                details.time,
                details.unread,
                details.mention,
                None,
            ))
            .debug_selector(move || id);
        div().w_full().min_w_0().child(row)
    }

    /// Показывает ошибку и рабочий повтор, сохраняя доступ к загруженным строкам.
    fn failed_status(
        active: &Active,
        palette: Palette,
        has_rows: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let origin = active.session.clone();
        let entity = cx.entity().downgrade();
        let mut retry = Button::new("catalog-retry", "Повторить");
        retry.appearance = ButtonAppearance::Ghost;
        let retry = retry
            .render(palette, None, move |_, app| {
                let _ = entity.update(app, |this, cx| this.retry(&origin, cx));
            })
            .debug_selector(|| "catalog-retry".into());
        if has_rows {
            div()
                .mx(px(12.))
                .p_2()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap_1()
                .rounded(px(8.))
                .border_1()
                .border_color(palette.border)
                .bg(palette.card)
                .child(label(palette, TextRole::Caption, "Не все чаты обновились"))
                .child(retry)
                .debug_selector(|| "catalog-partial-error".into())
        } else {
            catalog_status(
                palette,
                "Не удалось загрузить чаты",
                Some(retry.into_any_element()),
            )
            .debug_selector(|| "catalog-error".into())
        }
    }
}

impl EventEmitter<SidebarAction> for Sidebar {}

impl Render for Sidebar {
    /// Показывает поиск и состояние загрузки, прокручивая отдельно строки каталога.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(ThemeMode::from(window.appearance()));
        let id = self.active.as_ref().map_or_else(
            || "sidebar-unselected".into(),
            |active| ElementId::View(active.search.entity_id()),
        );
        let mut panel = div()
            .id(id)
            .w_full()
            .h_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(palette.surface);

        let Some(active) = &mut self.active else {
            return panel.child(catalog_status(palette, "Выберите аккаунт", None));
        };
        panel = panel.child(
            div()
                .px(px(12.))
                .pt(px(16.))
                .pb(px(12.))
                .flex_none()
                .child(active.search.clone()),
        );
        let catalog = active.catalog.clone();
        let chats = catalog.as_ref().and_then(|list| list.chats.as_deref());
        let status = catalog.as_ref().map(|list| list.status);
        let has_rows = chats.is_some_and(|chats| !chats.is_empty());
        if status == Some(ChatListStatus::Failed) {
            panel = panel.child(Self::failed_status(active, palette, has_rows, cx));
        } else if status != Some(ChatListStatus::Ready) && !has_rows {
            panel = panel.child(
                catalog_status(palette, "Загрузка чатов…", None)
                    .debug_selector(|| "catalog-loading".into()),
            );
        }

        let mut rows = div()
            .id("catalog-scroll")
            .w_full()
            .min_w_0()
            .min_h_0()
            .flex_1()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(8.))
            .overflow_y_scroll()
            .track_scroll(&active.scroll)
            .debug_selector(|| "catalog-scroll".into());
        let query = active.search.read(cx).text().trim().to_lowercase();
        let searching = !query.is_empty();
        let mut results = Vec::new();
        let mut row_index = 0;
        if let Some(chats) = chats {
            let mut found = false;
            for chat in chats {
                let name_matches = matches_query(&chat.name, &query);
                let visible_topics = chat
                    .topics
                    .iter()
                    .filter(|topic| name_matches || matches_query(&topic.name, &query));
                if !name_matches && visible_topics.clone().next().is_none() {
                    continue;
                }
                found = true;
                let opened = !chat.direct && (searching || active.expanded.contains(&chat.id));
                rows = rows.child(Self::chat_row(
                    active,
                    chat,
                    opened,
                    !searching && !chat.topics.is_empty(),
                    palette,
                    cx,
                ));
                if searching && chat.direct {
                    results.push((row_index, active.focus[&RowKey::Chat(chat.id)].clone()));
                }
                row_index += 1;
                if opened {
                    for topic in visible_topics {
                        rows = rows.child(Self::topic_row(active, chat.id, topic, palette, cx));
                        if searching {
                            results.push((
                                row_index,
                                active.focus[&RowKey::Topic(chat.id, topic.id)].clone(),
                            ));
                        }
                        row_index += 1;
                    }
                }
            }
            if !found && status == Some(ChatListStatus::Ready) {
                panel = panel.child(
                    catalog_status(
                        palette,
                        if chats.is_empty() {
                            "У вас пока нет чатов"
                        } else {
                            "Ничего не найдено"
                        },
                        None,
                    )
                    .debug_selector(|| "catalog-empty".into()),
                );
            }
        }
        if searching && !results.is_empty() {
            let previous = results.clone();
            panel = panel
                .key_context("CatalogResults")
                .on_action(cx.listener(move |this, _: &PreviousResult, window, cx| {
                    if let Some(active) = &this.active {
                        active.move_result(&previous, true, window, cx);
                    }
                }))
                .on_action(cx.listener(move |this, _: &NextResult, window, cx| {
                    if let Some(active) = &this.active {
                        active.move_result(&results, false, window, cx);
                    }
                }));
        }
        panel.child(rows)
    }
}

/// Оформляет загрузку, ошибку и пустой каталог в одинаковой области под поиском.
fn catalog_status(palette: Palette, text: &'static str, action: Option<gpui::AnyElement>) -> Div {
    div()
        .mx(px(12.))
        .py(px(20.))
        .px(px(10.))
        .flex_none()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .rounded(px(8.))
        .border_1()
        .border_color(palette.border)
        .bg(palette.card)
        .child(label(palette, TextRole::Label, text))
        .children(action)
}

/// Сопоставляет имя с нормализованным запросом без учёта регистра.
fn matches_query(name: &str, query: &str) -> bool {
    query.is_empty() || name.to_lowercase().contains(query)
}

/// Отличает обновлённый каталог от повторной передачи того же снимка.
fn same_catalog(previous: Option<&ChatList>, next: Option<&ChatList>) -> bool {
    match (previous, next) {
        (None, None) => true,
        (Some(previous), Some(next)) if previous.status == next.status => {
            match (&previous.chats, &next.chats) {
                (None, None) => true,
                (Some(left), Some(right)) => std::sync::Arc::ptr_eq(left, right),
                _ => false,
            }
        }
        _ => false,
    }
}
