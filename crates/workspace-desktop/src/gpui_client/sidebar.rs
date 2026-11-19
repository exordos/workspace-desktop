//! Показывает каталог выбранного входа, хранит поиск и раскрытие каналов.
//! Строки передают оболочке адрес разговора; запросы остаются у родителя.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use gpui::{
    App, Context, Div, ElementId, Entity, EventEmitter, FocusHandle, KeyBinding, MouseButton,
    Pixels, Point, Render, ScrollHandle, SharedString, Size, Stateful, Window, div, point,
    prelude::*, px,
};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;
use workspace_core::{
    bootstrap::{ChatList, ChatListStatus},
    context::OwnerContext,
    messenger::{
        FolderSnapshot,
        catalog::{Chat, Topic},
        folders::FolderItem,
    },
    storage::HistoryScope,
};

use super::{
    avatars::AvatarImages,
    contracts::{
        AccountSession, ChatEntry, ChatRoute, ConversationAddress, FolderCommand,
        FolderCommandError, FolderRailPreferences, SidebarAction,
    },
    runtime::RuntimeClient,
    ui::{
        Avatar, AvatarPresence, BadgeTone, Button, ButtonAppearance, ChatRow, ChatRowPreview, Icon,
        Palette, TextInput, TextInputChanged, TextRole, label,
        popup::{MenuInput, menu_layer, menu_navigation, menu_surface, tooltip},
    },
};

mod folder_dialog;
mod folder_menu;
mod folder_rail;
mod folders;
mod search;

use folder_dialog::FolderDialog;
use folder_menu::{RowAction, RowActions};
use folder_rail::FolderRail;
pub(super) use folders::FolderIntent;
use folders::FolderPane;

#[cfg(all(test, feature = "gpui-test"))]
mod tests;

gpui::actions!(catalog_results, [PreviousResult, NextResult, OpenRowMenu]);

/// Состояние каталога в окне. Оболочка передаёт выбранный вход и актуальный снимок.
pub struct Sidebar {
    active: Option<Active>, // Показанный каталог или ожидание выбора входа.
    runtime: Option<RuntimeClient>, // Рабочий поток для фото. None оставляет заглушки.
    saved: HashMap<AccountSession, Preferences>, // Поиск и раскрытые каналы других входов.
    menu_focus: FocusHandle, // Удерживает клавиатурный ввод внутри открытого меню.
    menu_scroll: ScrollHandle, // Не даёт подсветке уйти за края длинного списка папок.
    folders: FolderPane,    // Подписки, панель и форма управления папками.
}

/// Состояние, которое можно вернуть при повторном выборе того же входа.
#[derive(Default)]
struct Preferences {
    query: String,           // Последний запрос без нормализации для отображения в поле.
    expanded: HashSet<Uuid>, // Каналы, которые были раскрыты.
    folder: Uuid,            // Выбранная папка в этом входе. Нулевой UUID означает «Все чаты».
}

/// Каталог одного выбранного входа и состояние только его текущего показа.
struct Active {
    session: AccountSession,              // Владелец данных и поколение входа.
    catalog: Option<ChatList>,            // Нет снимка до начала или завершения первого чтения.
    selected: Option<HistoryScope>,       // Разговор, который сейчас открыт в окне.
    search: Entity<TextInput>,            // Поле системного ввода, живущее до смены входа.
    _search_observer: gpui::Subscription, // Подписка на изменение поискового запроса.
    expanded: HashSet<Uuid>,              // Каналы, раскрытые вне режима поиска.
    scroll: ScrollHandle,                 // Позиция прокрутки только текущего показа.
    avatars: AvatarImages,                // Подписки на фото в видимой части каталога.
    avatar_users: Vec<Uuid>,              // UUID собеседников из последнего видимого диапазона.
    avatar_frame: u64,                    // Номер кадра, для которого рассчитаны подписки.
    focus: HashMap<RowKey, FocusHandle>,  // Фокус строки остаётся у UUID при перестановке.
    menu: Option<RowMenu>,                // Открытая строка и место раскрытия в окне.
    folder: Uuid,                         // UUID выбранной папки.
    folder_view: Option<Arc<FolderSnapshot>>, // Подтверждённые данные только текущего входа.
    folder_settings: FolderRailPreferences, // Локальные настройки панели этого владельца.
    folder_pending: Option<(Uuid, bool)>, // UUID операции и признак ответа для формы.
    folder_error: Option<&'static str>,   // Безопасная подпись ошибки команды строки.
    folder_online: bool,                  // Авторизация разрешает отправить серверную команду.
    #[cfg(all(test, feature = "gpui-test"))]
    fixture_details: HashMap<RowKey, RowDetails>, // Синтетические метаданные этого показа.
}

/// Устойчивый ключ строки внутри каталога.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RowKey {
    Chat(Uuid),        // UUID личного чата или канала.
    Topic(Uuid, Uuid), // UUID канала и его темы.
}

/// Меню привязано к строке текущего входа, даже если каталог переставит строки.
#[derive(Clone)]
struct RowMenu {
    key: RowKey,                 // Цель действия в текущем каталоге.
    anchor: Point<Pixels>,       // Координаты раскрытия относительно окна.
    selected: Option<RowAction>, // Действие с устойчивой целью, даже если список переставился.
    folders: bool,               // Сейчас открыта вложенная страница назначения папок.
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
    /// Определяет подпись и доступность уже существующего действия каталога.
    fn menu_action(&self, key: RowKey, app: &App) -> Option<(&'static str, bool)> {
        let stream = match key {
            RowKey::Chat(stream) | RowKey::Topic(stream, _) => stream,
        };
        let chats = self.chats()?;
        let chat = chats.iter().find(|chat| chat.id == stream)?;
        match key {
            RowKey::Chat(_) if chat.direct => Some(("Открыть чат", true)),
            RowKey::Chat(_) => Some((
                if self.expanded.contains(&stream) {
                    "Свернуть темы"
                } else {
                    "Развернуть темы"
                },
                !chat.topics.is_empty() && self.search.read(app).text().trim().is_empty(),
            )),
            RowKey::Topic(_, topic) => chat
                .topics
                .iter()
                .any(|entry| entry.id == topic)
                .then_some(("Открыть тему", true)),
        }
    }

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
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            active: None,
            runtime: None,
            saved: HashMap::new(),
            menu_focus: cx.focus_handle(),
            menu_scroll: ScrollHandle::new(),
            folders: FolderPane::new(cx),
        }
    }

    /// Создаёт рабочий каталог с доступом к аватарам текущего входа.
    pub(super) fn with_runtime(runtime: RuntimeClient, cx: &mut Context<Self>) -> Self {
        Self {
            runtime: Some(runtime),
            ..Self::new(cx)
        }
    }

    /// Регистрирует команды редактирования поиска один раз при запуске GPUI.
    pub fn bind_keys(app: &mut App) {
        search::bind_keys(app);
        FolderRail::bind_keys(app);
        FolderDialog::bind_keys(app);
        app.bind_keys([
            KeyBinding::new("up", PreviousResult, Some("CatalogResults")),
            KeyBinding::new("down", NextResult, Some("CatalogResults")),
            KeyBinding::new("shift-f10", OpenRowMenu, Some("CatalogRow")),
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
                        folder: previous.folder,
                    },
                );
            }
            self.folders.reset(cx);
            if let Some(session) = session {
                // После повторного входа состояние старого поколения уже не пригодится.
                self.saved.retain(|key, _| {
                    key.owner != session.owner || key.generation == session.generation
                });
                let preferences = self.saved.remove(session).unwrap_or_default();
                let search = cx.new(|cx| TextInput::for_search(preferences.query, cx));
                let observer = cx.subscribe(&search, |this, _, _: &TextInputChanged, cx| {
                    if let Some(active) = &mut this.active {
                        active.scroll.set_offset(gpui::point(px(0.), px(0.)));
                        active.menu = None;
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
                    avatars: AvatarImages::default(),
                    avatar_users: Vec::new(),
                    avatar_frame: 0,
                    focus: HashMap::new(),
                    menu: None,
                    folder: preferences.folder,
                    folder_view: None,
                    folder_settings: self.folders.preferences(&session.owner),
                    folder_pending: None,
                    folder_error: None,
                    folder_online: false,
                    #[cfg(all(test, feature = "gpui-test"))]
                    fixture_details: HashMap::new(),
                });
            }
        }
        if switched {
            self.update_folder_view(cx);
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
            if let Some(menu) = &active.menu
                && active.menu_action(menu.key, cx).is_none()
            {
                active.menu = None;
            }
            cx.notify();
        } else if switched {
            cx.notify();
        }
    }

    /// Открывает меню из указателя или клавиатурного фокуса на строке.
    fn open_row_menu(
        &mut self,
        origin: &AccountSession,
        menu: RowMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = &mut self.active
            && &active.session == origin
            && active.menu_action(menu.key, cx).is_some()
        {
            let mut menu = menu;
            menu.selected = active
                .row_actions(menu.key, false, cx)
                .into_iter()
                .find(|action| action.enabled)
                .map(|action| action.action);
            self.menu_scroll.set_offset(point(px(0.), px(0.)));
            active.menu = Some(menu);
            window.focus(&self.menu_focus);
            cx.notify();
        }
    }

    /// Перемещает выбор меню или выполняет подтверждённое действие.
    fn menu_input(&mut self, input: MenuInput, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = &mut self.active else {
            return;
        };
        let Some(mut menu) = active.menu.clone() else {
            return;
        };
        let actions = active.row_actions(menu.key, menu.folders, cx);
        let enabled: Vec<_> = actions
            .iter()
            .enumerate()
            .filter(|(_, action)| action.enabled)
            .collect();
        let index = enabled
            .iter()
            .position(|(_, action)| Some(&action.action) == menu.selected.as_ref());
        let next = match input {
            MenuInput::First => enabled.first(),
            MenuInput::Last => enabled.last(),
            MenuInput::Next => enabled.get(index.map_or(0, |index| index + 1)),
            MenuInput::Previous => index
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| enabled.get(index)),
            MenuInput::Select | MenuInput::Close => {
                let action = if matches!(input, MenuInput::Select) {
                    let Some(index) = index else {
                        return;
                    };
                    Some(enabled[index].1.action.clone())
                } else {
                    None
                };
                if matches!(action, Some(RowAction::Assign | RowAction::Back)) {
                    menu.folders = matches!(action, Some(RowAction::Assign));
                    menu.selected = active
                        .row_actions(menu.key, menu.folders, cx)
                        .into_iter()
                        .find(|choice| choice.enabled)
                        .map(|choice| choice.action);
                    active.menu = Some(menu);
                    self.menu_scroll.set_offset(point(px(0.), px(0.)));
                    cx.notify();
                    return;
                }
                if menu.folders
                    && let Some(RowAction::Command(command)) = action
                {
                    self.send_folder_command(command, false, cx);
                    return;
                }
                let session = active.session.clone();
                active.menu = None;
                if let Some(focus) = active.focus.get(&menu.key) {
                    window.focus(focus);
                } else {
                    active.search.read(cx).focus(window);
                }
                cx.notify();
                match action {
                    Some(RowAction::Open) => self.activate(&session, menu.key, cx),
                    Some(RowAction::Command(command)) => {
                        self.send_folder_command(command, false, cx)
                    }
                    _ => {}
                }
                return;
            }
        };
        if let Some((index, choice)) = next {
            menu.selected = Some(choice.action.clone());
            active.menu = Some(menu);
            self.menu_scroll.scroll_to_item(*index);
            cx.notify();
        }
    }

    /// Добавляет правую кнопку мыши, Shift+F10 и подсказку полного названия.
    fn context_trigger(
        row: Stateful<Div>,
        active: &Active,
        key: RowKey,
        index: usize,
        title: SharedString,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let origin = active.session.clone();
        let keyboard_origin = origin.clone();
        row.key_context("CatalogRow")
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |view, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let menu = RowMenu {
                        key,
                        anchor: event.position,
                        selected: None,
                        folders: false,
                    };
                    view.open_row_menu(&origin, menu, window, cx);
                }),
            )
            .on_action(cx.listener(move |view, _: &OpenRowMenu, window, cx| {
                let Some(bounds) = view
                    .active
                    .as_ref()
                    .filter(|active| active.session == keyboard_origin)
                    .and_then(|active| active.scroll.bounds_for_item(index))
                else {
                    return;
                };
                let menu = RowMenu {
                    key,
                    anchor: point(bounds.left(), bounds.bottom()),
                    selected: None,
                    folders: false,
                };
                view.open_row_menu(&keyboard_origin, menu, window, cx);
            }))
            .tooltip(move |_, app| tooltip(palette, title.clone(), app))
    }

    /// Рисует главное меню или список папок, сохраняя тот же фокус.
    fn row_menu(
        actions: RowActions,
        menu: &RowMenu,
        origin: &AccountSession,
        viewport: Size<Pixels>,
        palette: Palette,
        scroll: &ScrollHandle,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let target = cx.entity().downgrade();
        menu_surface("catalog-menu", palette, viewport, px(248.), px(320.))
            .debug_selector(|| "catalog-menu".into())
            .overflow_y_scroll()
            .track_scroll(scroll)
            .on_mouse_down_out(cx.listener(|view, _, window, cx| {
                cx.stop_propagation();
                view.menu_input(MenuInput::Close, window, cx);
            }))
            .children(actions.into_iter().enumerate().map(|(index, action)| {
                let choice = action.action;
                let selected = menu.selected.as_ref() == Some(&choice);
                let source = origin.clone();
                let key = menu.key;
                let assignments = menu.folders;
                let mut button = Button::new(("catalog-menu-item", index), action.title);
                button.appearance = ButtonAppearance::Ghost;
                button.disabled = !action.enabled;
                let target = target.clone();
                button
                    .render(palette, None, move |window, app| {
                        let _ = target.update(app, |view, cx| {
                            let Some(active) = view
                                .active
                                .as_mut()
                                .filter(|active| active.session == source)
                            else {
                                return;
                            };
                            let Some(menu) = active
                                .menu
                                .as_mut()
                                .filter(|menu| menu.key == key && menu.folders == assignments)
                            else {
                                return;
                            };
                            menu.selected = Some(choice.clone());
                            view.menu_input(MenuInput::Select, window, cx);
                        });
                    })
                    .w_full()
                    .min_w_0()
                    .justify_start()
                    .flex_none()
                    .when(action.enabled && selected, |row| {
                        row.bg(palette.card_selected)
                    })
                    .debug_selector(move || {
                        if index == 0 {
                            "catalog-menu-action".into()
                        } else {
                            format!("catalog-menu-item-{index}")
                        }
                    })
            }))
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
        let Some(chats) = active.chats() else {
            return;
        };
        let stream_id = match key {
            RowKey::Chat(id) | RowKey::Topic(id, _) => id,
        };
        let Some(chat) = chats.iter().find(|chat| chat.id == stream_id) else {
            return;
        };
        let query = active.search.read(cx).text().trim().to_lowercase();
        if query.is_empty() && !active.folder_contains(chat.id) {
            return;
        }
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
        (chat, item): (&Chat, Option<&FolderItem>),
        opened: bool,
        expandable: bool,
        index: usize,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = chat.id;
        let key = RowKey::Chat(id);
        let mut details = active.row_details(key);
        if let Some(item) = item {
            details.unread = if item.active_unread_count > 0 {
                Some((u64::from(item.active_unread_count), BadgeTone::Accent))
            } else if item.passive_unread_count > 0 {
                Some((u64::from(item.passive_unread_count), BadgeTone::Muted))
            } else {
                None
            };
        }
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
        let trailing = if item.is_some_and(|item| item.pinned_at.is_some()) {
            Some(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .flex_none()
                    .child(Icon::Pin.element(px(14.), palette.icon_hover))
                    .children(chevron),
            )
        } else {
            chevron
        };
        let mut avatar = Avatar::from_initials(initials.to_owned());
        avatar.presence = details.presence;
        if chat.direct {
            avatar.image = chat
                .metadata
                .direct_peer_id
                .and_then(|user| active.avatars.image(user, cx));
        }
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
        .render_with_action(palette, Some(handle), trailing, move |_, app| {
            let _ = entity.update(app, |this, cx| this.activate(&origin, key, cx));
        })
        .min_h(px(64.))
        .debug_selector(move || format!("catalog-chat-{id}"));
        let row = Self::context_trigger(
            row,
            active,
            key,
            index,
            chat.name.clone().into(),
            palette,
            cx,
        );
        div().w_full().min_w_0().child(row)
    }

    /// Рисует тему как компактную выбираемую строку внутри канала.
    fn topic_row(
        active: &mut Active,
        stream_id: Uuid,
        topic: &Topic,
        index: usize,
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
        let row = Self::context_trigger(
            row,
            active,
            key,
            index,
            topic.name.clone().into(),
            palette,
            cx,
        );
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

    /// После размещения списка подписывает каталог на фото видимых собеседников.
    fn track_avatar_rows(
        rows: Div,
        active: &Active,
        peers: Vec<Option<Uuid>>,
        cx: &Context<Self>,
    ) -> Div {
        let scroll = active.scroll.clone();
        let previous = active.avatar_users.clone();
        let session = active.session.clone();
        let avatar_frame = active.avatar_frame;
        let sidebar = cx.entity().downgrade();
        rows.on_children_prepainted(move |_, _, app| {
            let start = scroll.top_item();
            let end = (scroll.bottom_item() + 1).min(peers.len());
            let users: Vec<_> = peers
                .get(start..end)
                .unwrap_or_default()
                .iter()
                .flatten()
                .copied()
                .collect();
            if users == previous {
                return;
            }
            let sidebar = sidebar.clone();
            let session = session.clone();
            // Открываем подписки после кадра. Поиск и смена входа могут заменить эту проекцию.
            app.defer(move |app| {
                let _ = sidebar.update(app, |this, cx| {
                    this.update_avatar_users(&session, avatar_frame, users, cx);
                });
            });
        })
    }

    /// Обновляет подписки, только пока измеренный кадр относится к текущему входу и списку.
    fn update_avatar_users(
        &mut self,
        session: &AccountSession,
        avatar_frame: u64,
        users: Vec<Uuid>,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = &mut self.active
            && &active.session == session
            && active.avatar_frame == avatar_frame
            && active.avatar_users != users
        {
            if let Some(runtime) = &self.runtime {
                active
                    .avatars
                    .refresh(users.iter().copied(), runtime, session, cx);
            }
            active.avatar_users = users;
            cx.notify();
        }
    }
}

impl EventEmitter<SidebarAction> for Sidebar {}

impl Render for Sidebar {
    /// Показывает поиск и состояние загрузки, прокручивая отдельно строки каталога.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.open_requested_folder_dialog(window, cx);
        let palette = Palette::for_window(window, cx);
        let id = self.active.as_ref().map_or_else(
            || "sidebar-unselected".into(),
            |active| ElementId::View(active.search.entity_id()),
        );
        let mut panel = div()
            .id(id)
            .debug_selector(|| "workspace-sidebar-panel".into())
            .w_full()
            .h_full()
            .rounded(px(6.))
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(palette.surface);

        let Some(active) = &mut self.active else {
            if self.menu_focus.is_focused(window) {
                window.blur();
            }
            return self.with_folder_rail(panel.child(catalog_status(
                palette,
                "Выберите аккаунт",
                None,
            )));
        };
        active.avatar_frame = active.avatar_frame.wrapping_add(1);
        if active.menu.is_none() && self.menu_focus.is_focused(window) {
            active.search.read(cx).focus(window);
        }
        panel = panel.child(
            div()
                .px(px(12.))
                .pt(px(16.))
                .pb(px(12.))
                .flex_none()
                .child(search::render(&active.search, palette, window, cx)),
        );
        let catalog = active.catalog.clone();
        let snapshot = active.folder_view.clone();
        let chats = catalog.as_ref().and_then(|list| list.chats.as_deref());
        let status = catalog.as_ref().map(|list| list.status);
        let query = active.search.read(cx).text().trim().to_lowercase();
        let searching = !query.is_empty();
        let entries = folders::folder_rows(
            snapshot.as_deref(),
            chats.unwrap_or_default(),
            active.folder,
            searching,
        );
        let has_rows = !entries.is_empty();
        if let Some(error) = active.folder_error {
            panel = panel.child(
                catalog_status(palette, error, None)
                    .debug_selector(|| "catalog-folder-error".into()),
            );
        }
        if status == Some(ChatListStatus::Failed) {
            panel = panel.child(Self::failed_status(active, palette, has_rows, cx));
        } else if status != Some(ChatListStatus::Ready) && !has_rows {
            panel = panel.child(
                catalog_status(palette, "Загрузка чатов…", None)
                    .debug_selector(|| "catalog-loading".into()),
            );
        }

        let mut rows = div()
            .w_full()
            .min_w_0()
            .min_h_0()
            .flex_1()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(8.))
            .debug_selector(|| "catalog-scroll".into());
        let mut results = Vec::new();
        let mut peers = Vec::new();
        let mut row_index = 0;
        if snapshot
            .as_deref()
            .is_some_and(|snapshot| snapshot.folders.is_some())
            || chats.is_some()
        {
            let mut found = false;
            let mut group = None;
            let groups_visible = searching && !active.folder.is_nil();
            for (chat, item, outside) in entries {
                let name_matches = matches_query(&chat.name, &query);
                let visible_topics = chat
                    .topics
                    .iter()
                    .filter(|topic| name_matches || matches_query(&topic.name, &query));
                if !name_matches && visible_topics.clone().next().is_none() {
                    continue;
                }
                found = true;
                if groups_visible && group != Some(outside) {
                    group = Some(outside);
                    let (heading, id) = if outside {
                        ("В других папках", "catalog-search-outside-folder")
                    } else {
                        ("В этой папке", "catalog-search-in-folder")
                    };
                    rows = rows.child(
                        label(palette, TextRole::Caption, heading)
                            .py_1()
                            .px_2()
                            .debug_selector(move || id.into()),
                    );
                    peers.push(None);
                    row_index += 1;
                }
                peers.push(if chat.direct {
                    chat.metadata.direct_peer_id.filter(|id| !id.is_nil())
                } else {
                    None
                });
                let opened = !chat.direct && (searching || active.expanded.contains(&chat.id));
                rows = rows.child(Self::chat_row(
                    active,
                    (chat, item),
                    opened,
                    !searching && !chat.topics.is_empty(),
                    row_index,
                    palette,
                    cx,
                ));
                if searching && chat.direct {
                    results.push((row_index, active.focus[&RowKey::Chat(chat.id)].clone()));
                }
                row_index += 1;
                if opened {
                    for topic in visible_topics {
                        peers.push(None);
                        rows = rows.child(Self::topic_row(
                            active, chat.id, topic, row_index, palette, cx,
                        ));
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
                        if searching {
                            "Ничего не найдено"
                        } else if active.folder.is_nil() {
                            "У вас пока нет чатов"
                        } else {
                            "В папке пока нет чатов"
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
        let panel = panel
            .child(
                Self::track_avatar_rows(rows, active, peers, cx)
                    .id("catalog-scroll")
                    .overflow_y_scroll()
                    .track_scroll(&active.scroll),
            )
            .when_some(active.menu.as_ref(), |panel, menu| {
                let viewport = window.viewport_size();
                let actions = active.row_actions(menu.key, menu.folders, cx);
                menu_navigation(panel, &self.menu_focus, cx, Self::menu_input).children(menu_layer(
                    viewport,
                    menu.anchor,
                    Self::row_menu(
                        actions,
                        menu,
                        &active.session,
                        viewport,
                        palette,
                        &self.menu_scroll,
                        cx,
                    ),
                ))
            });
        self.with_folder_rail(panel)
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
