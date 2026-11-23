//! Показывает каталог выбранного входа, хранит поиск и раскрытие каналов.
//! Строки передают оболочке адрес разговора; запросы остаются у родителя.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};

use gpui::{
    App, Bounds, Context, Div, ElementId, Entity, EventEmitter, FocusHandle, KeyBinding,
    MouseButton, Pixels, Point, Render, ScrollHandle, SharedString, Size, Stateful, Window, div,
    point, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{
    bootstrap::{ChatList, ChatListStatus},
    messenger::{
        FolderSnapshot,
        catalog::{Chat, SidebarPreview, Topic},
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
    preferences::{ChatClick, Preferences as AppPreferences, appearance::ChatDensity},
    runtime::RuntimeClient,
    ui::{
        BadgeTone, Button, ButtonAppearance, ChatRowPreview, PANEL_RADIUS, Palette, TextInput,
        TextInputChanged, TextRole, label,
        popup::{MenuInput, menu_layer, menu_navigation, menu_surface, tooltip},
    },
};

mod folder_dialog;
mod folder_menu;
mod folder_rail;
mod folders;
pub(super) mod new_chats;
pub(in crate::gpui_client) mod order;
mod projection;
mod rows;
mod search;

use folder_dialog::FolderDialog;
use folder_menu::{RowAction, RowActions};
use folder_rail::FolderRail;
pub(super) use folders::FolderIntent;
use folders::FolderPane;
use new_chats::NewChats;
#[cfg(debug_assertions)]
pub use new_chats::preview::{NewChatPreview, new_chat_preview};
use projection::{ChatGroup, Presentation, TopicToggleFocus, TopicToggleKind};
use rows::{ChannelActions, ChannelRow, ChatRowDetails as RowDetails, PreviewAction, TopicRow};

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
    new_chats: Entity<NewChats>, // Временный режим создания и архив той же панели.
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
    reveal_route: Option<HistoryScope>, // Внешняя тема, которую нужно показать, когда она станет доступна.
    search_target: Option<HistoryScope>, // Маршрут, открытый из поиска с сохранением ручного раскрытия.
    search: Entity<TextInput>,           // Поле системного ввода, живущее до смены входа.
    _search_observer: gpui::Subscription, // Подписка на изменение поискового запроса.
    presentation: Presentation,          // Локальные состояния поиска и раскрытия тем.
    scroll: ScrollHandle,                // Позиция прокрутки только текущего показа.
    avatars: AvatarImages,               // Подписки на фото собеседников в видимой части.
    avatar_users: Vec<Uuid>,             // UUID собеседников из последнего видимого диапазона.
    focus: HashMap<RowKey, FocusHandle>, // Фокус строки остаётся у UUID при перестановке.
    bounds: RowBounds,                   // Фактическое положение строк внутри раскрытой карточки.
    revision: u64, // Меняется при публикации проекции с другим составом строк.
    pending_focus: Option<RowKey>, // Строка, которую пока нельзя сфокусировать до нового кадра.
    menu: Option<RowMenu>, // Открытая строка и место раскрытия в окне.
    folder: Uuid,  // UUID выбранной папки.
    folder_view: Option<Arc<FolderSnapshot>>, // Подтверждённые данные только текущего входа.
    folder_settings: FolderRailPreferences, // Локальные настройки панели этого владельца.
    folder_pending: Option<(Uuid, bool)>, // UUID операции и признак ответа для формы.
    folder_error: Option<&'static str>, // Безопасная подпись ошибки команды строки.
    folder_online: bool, // Авторизация разрешает отправить серверную команду.
    #[cfg(all(test, feature = "gpui-test"))]
    fixture_details: HashMap<RowKey, RowDetails>, // Синтетические метаданные этого показа.
}

/// Устойчивый ключ строки внутри каталога.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RowKey {
    Chat(Uuid),        // UUID личного чата или канала.
    Topic(Uuid, Uuid), // UUID канала и его темы.
    Preview(Uuid),     // Превью сообщения в карточке канала.
    Avatar(Uuid),      // Отдельный переход в общую тему канала.
    Chevron(Uuid),     // Стрелка раскрытия канала.
    TopicToggle(Uuid), // Кнопка показа следующих тем внутри канала.
}

impl RowKey {
    /// Возвращает UUID карточки, которой принадлежит элемент.
    fn chat_id(self) -> Uuid {
        match self {
            Self::Chat(id)
            | Self::Topic(id, _)
            | Self::Preview(id)
            | Self::Avatar(id)
            | Self::Chevron(id)
            | Self::TopicToggle(id) => id,
        }
    }
}

/// Намерение пользователя. Источник и доступность проверяются перед выполнением.
#[derive(Clone, Copy)]
enum Intent {
    Open(RowKey),              // Открыть личный чат, общую тему канала или точную тему.
    Preview(Uuid, Uuid, Uuid), // Открыть тему канала только для того же сообщения.
    ToggleChannel(Uuid),       // Раскрыть или свернуть канал без навигации.
    Reveal(Uuid, TopicToggleKind, u64), // Выполнить видимую ступень этого кадра.
}

impl Intent {
    /// Возвращает карточку, к которой относится действие.
    fn chat_id(self) -> Uuid {
        match self {
            Self::Open(key) => key.chat_id(),
            Self::Preview(id, _, _) | Self::ToggleChannel(id) | Self::Reveal(id, _, _) => id,
        }
    }
}

/// Фактические границы интерактивных строк после размещения и прокрутки.
type RowBounds = Rc<RefCell<HashMap<RowKey, Bounds<Pixels>>>>;

/// Меню привязано к строке текущего входа, даже если каталог переставит строки.
#[derive(Clone)]
struct RowMenu {
    key: RowKey,                 // Цель действия в текущем каталоге.
    anchor: Point<Pixels>,       // Координаты раскрытия относительно окна.
    selected: Option<RowAction>, // Действие с устойчивой целью, даже если список переставился.
    folders: bool,               // Сейчас открыта вложенная страница назначения папок.
}

impl Active {
    /// Возвращает доступную карточку из последнего снимка этого входа.
    fn chat(&self, id: Uuid) -> Option<&Chat> {
        self.chats()?.iter().find(|chat| chat.id == id)
    }

    /// Определяет подпись и действие контекстного меню только для видимой строки.
    fn menu_intent(&self, key: RowKey) -> Option<(&'static str, Option<Intent>)> {
        let stream = key.chat_id();
        if !self.presentation.searching() && !self.folder_contains(stream) {
            return None;
        }
        let chat = self.chat(stream)?;
        let group = self.presentation.group(chat, self.selected)?;
        match key {
            RowKey::Chat(_) if chat.direct => Some(("Открыть чат", Some(Intent::Open(key)))),
            RowKey::Chat(_) => Some((
                if group.expanded {
                    "Свернуть темы"
                } else {
                    "Развернуть темы"
                },
                (!chat.topics.is_empty() && !self.presentation.searching())
                    .then_some(Intent::ToggleChannel(stream)),
            )),
            RowKey::Topic(_, topic) => group
                .topics
                .iter()
                .any(|entry| entry.id == topic)
                .then_some(("Открыть тему", Some(Intent::Open(key)))),
            RowKey::Preview(_)
            | RowKey::Avatar(_)
            | RowKey::Chevron(_)
            | RowKey::TopicToggle(_) => None,
        }
    }

    /// Возвращает подпись и доступность первого пункта для общего меню с папками.
    fn menu_action(&self, key: RowKey, _app: &App) -> Option<(&'static str, bool)> {
        self.menu_intent(key)
            .map(|(title, intent)| (title, intent.is_some()))
    }

    /// Берёт метаданные строки текущего каталога или тестовой сцены.
    fn row_details(&self, _key: RowKey, details: RowDetails) -> RowDetails {
        #[cfg(all(test, feature = "gpui-test"))]
        if let Some(details) = self.fixture_details.get(&_key) {
            return details.clone();
        }
        details
    }

    /// Возвращает стабильный фокус видимой строки после обновления каталога.
    fn row_focus(&mut self, key: RowKey, cx: &App) -> FocusHandle {
        self.focus
            .entry(key)
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    /// Раскрывает внешнюю тему после выхода из поиска или получения её в каталоге.
    fn apply_route_reveal(&mut self) {
        if self.presentation.searching() {
            return;
        }
        if let Some(scope) = self.reveal_route
            && let Some(topic) = scope.topic_id
            && let Some(chat) = self
                .session
                .catalog_chats(self.catalog.as_ref(), self.folder_view.as_deref())
                .and_then(|chats| chats.iter().find(|chat| chat.id == scope.stream_id))
            && self.presentation.ensure_topic_visible(chat, topic)
        {
            self.reveal_route = None;
        }
    }

    /// Сдвигает список на минимальную величину, чтобы вложенная строка оказалась видна.
    fn scroll_to_row(&self, key: RowKey) {
        let bounds = self.bounds.borrow();
        let row = if matches!(
            key,
            RowKey::Chevron(_) | RowKey::Preview(_) | RowKey::Avatar(_)
        ) {
            bounds.get(&RowKey::Chat(key.chat_id()))
        } else {
            bounds.get(&key)
        };
        let Some(row) = row else {
            return;
        };
        let viewport = self.scroll.bounds();
        let delta = if row.top() < viewport.top() {
            viewport.top() - row.top()
        } else if row.bottom() > viewport.bottom() {
            viewport.bottom() - row.bottom()
        } else {
            return;
        };
        let offset = self.scroll.offset();
        self.scroll.set_offset(point(
            offset.x,
            (offset.y + delta).clamp(-self.scroll.max_offset().height, px(0.)),
        ));
    }

    /// Переносит фокус между реальными результатами, не вмешиваясь в системный ввод.
    fn move_result(
        &self,
        results: &[RowKey],
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
            .position(|key| self.focus[key].is_focused(window))
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
        if let Some(key) = index.and_then(|index| results.get(index)) {
            self.scroll_to_row(*key);
            window.focus(&self.focus[key]);
            cx.notify();
        }
    }
}

impl Sidebar {
    /// Создаёт каталог без выбранного входа.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::make(None, cx)
    }

    /// Создаёт рабочий каталог с доступом к серверным действиям и аватарам.
    pub(super) fn with_runtime(runtime: RuntimeClient, cx: &mut Context<Self>) -> Self {
        Self::make(Some(runtime), cx)
    }

    /// Подключает отдельный режим создания к перерисовке того же сайдбара.
    fn make(runtime: Option<RuntimeClient>, cx: &mut Context<Self>) -> Self {
        let new_chats = cx.new(|cx| NewChats::new(runtime.clone(), cx));
        cx.observe(&new_chats, |_, _, cx| cx.notify()).detach();
        Self {
            active: None,
            runtime,
            saved: HashMap::new(),
            menu_focus: cx.focus_handle(),
            menu_scroll: ScrollHandle::new(),
            new_chats,
            folders: FolderPane::new(cx),
        }
    }

    /// Передаёт окну вложенный режим для открытия модалки и маршрутов.
    pub(super) fn new_chats(&self) -> Entity<NewChats> {
        self.new_chats.clone()
    }

    /// Регистрирует команды редактирования поиска один раз при запуске GPUI.
    pub fn bind_keys(app: &mut App) {
        search::bind_keys(app);
        NewChats::bind_keys(app);
        FolderRail::bind_keys(app);
        FolderDialog::bind_keys(app);
        app.bind_keys([
            KeyBinding::new("up", PreviousResult, Some("CatalogResults")),
            KeyBinding::new("down", NextResult, Some("CatalogResults")),
            KeyBinding::new("shift-f10", OpenRowMenu, Some("CatalogRow")),
        ]);
    }

    /// Направляет ввод клавиатуры в поле поиска текущего каталога.
    pub(super) fn focus_search(&self, window: &mut Window, app: &App) {
        let creation = self.new_chats.read(app);
        if creation.is_open() {
            creation.active_input().read(app).focus(window);
        } else if let Some(active) = &self.active {
            active.search.read(app).focus(window);
        }
    }

    /// Сбросит активный поиск и сообщит, надо ли вернуть прежний фокус.
    pub(super) fn cancel_search(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        let creation = self.new_chats.read(cx);
        if creation.is_open() {
            return None;
        }
        let active = self.active.as_ref()?;
        let input = active.search.read(cx);
        let focused = input.is_focused(window);
        if !focused && input.text().is_empty() {
            return None;
        }
        active.search.update(cx, TextInput::clear);
        Some(focused)
    }

    /// Возвращает фокус поиска, с которого начинается область каталога.
    pub(super) fn search_focus(&self, app: &App) -> Option<FocusHandle> {
        let creation = self.new_chats.read(app);
        if creation.is_open() {
            Some(creation.active_input().read(app).keyboard_focus())
        } else {
            self.active
                .as_ref()
                .map(|active| active.search.read(app).keyboard_focus())
        }
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
            self.new_chats
                .update(cx, |view, cx| view.select(session, cx));
            if let Some(previous) = self.active.take() {
                self.saved.insert(
                    previous.session,
                    Preferences {
                        query: previous.search.read(cx).text().to_owned(),
                        expanded: previous.presentation.expanded().clone(),
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
                let mut presentation = Presentation::default();
                presentation.set_sort(AppPreferences::read_for(cx, &session.owner).appearance.sort);
                presentation.set_query(&preferences.query);
                presentation.set_expanded(preferences.expanded);
                let search = cx.new(|cx| TextInput::for_search(preferences.query, cx));
                let observer = cx.subscribe(&search, |this, search, _: &TextInputChanged, cx| {
                    let Some(active) = &mut this.active else {
                        return;
                    };
                    if active.search != search {
                        return;
                    }
                    if active.presentation.set_query(search.read(cx).text()) {
                        active.scroll.set_offset(point(px(0.), px(0.)));
                        active.menu = None;
                        active.pending_focus = None;
                        active.revision = active.revision.wrapping_add(1);
                        active.apply_route_reveal();
                    }
                    cx.notify();
                });
                self.active = Some(Active {
                    session: session.clone(),
                    catalog: None,
                    selected: None,
                    reveal_route: None,
                    search_target: None,
                    search,
                    _search_observer: observer,
                    presentation,
                    scroll: ScrollHandle::new(),
                    avatars: AvatarImages::default(),
                    avatar_users: Vec::new(),
                    focus: HashMap::new(),
                    bounds: RowBounds::default(),
                    revision: 0,
                    pending_focus: None,
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
        if self.new_chats.read(cx).is_open()
            && !same_catalog(
                self.active
                    .as_ref()
                    .and_then(|active| active.catalog.as_ref()),
                catalog,
            )
        {
            self.new_chats.update(cx, NewChats::load_catalog);
        }
        if let Some(active) = &mut self.active {
            let catalog = catalog.filter(|list| list.generation == active.session.generation);
            let selected = route
                .filter(|address| {
                    address.route.owner == active.session.owner
                        && address.generation == active.session.generation
                })
                .map(|address| address.route.scope);
            let same_snapshot = same_catalog(active.catalog.as_ref(), catalog);
            let route_changed = active.selected != selected;
            if !route_changed && !switched && same_snapshot {
                return;
            }

            if route_changed {
                let from_search = active.search_target.take() == selected;
                active.reveal_route =
                    selected.filter(|scope| !from_search && scope.topic_id.is_some());
                active.selected = selected;
            }
            active.catalog = catalog.cloned();
            if !same_snapshot || route_changed {
                active.revision = active.revision.wrapping_add(1);
            }
            if let Some(chats) = catalog.and_then(|list| list.chats.as_deref()) {
                active.presentation.retain_channels(chats);
            }
            active.apply_route_reveal();
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
                let primary = active.menu_intent(menu.key).and_then(|(_, intent)| intent);
                active.menu = None;
                if let Some(focus) = active
                    .focus
                    .get(&menu.key)
                    .or_else(|| active.focus.get(&RowKey::Chevron(menu.key.chat_id())))
                {
                    window.focus(focus);
                } else {
                    active.search.read(cx).focus(window);
                }
                cx.notify();
                match action {
                    Some(RowAction::Open) => {
                        if let Some(intent) = primary {
                            self.activate(&session, intent, cx);
                        }
                    }
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
                    .and_then(|active| active.bounds.borrow().get(&key).copied())
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

    /// Выполняет действие только для видимой строки в том же входе и поколении.
    fn activate(&mut self, origin: &AccountSession, intent: Intent, cx: &mut Context<Self>) {
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| &active.session == origin)
        else {
            return;
        };
        let stream_id = intent.chat_id();
        if !active.presentation.searching() && !active.folder_contains(stream_id) {
            return;
        }
        let chat = active
            .session
            .catalog_chats(active.catalog.as_ref(), active.folder_view.as_deref())
            .and_then(|chats| chats.iter().find(|chat| chat.id == stream_id));
        let Some(chat) = chat else {
            return;
        };
        let Some(group) = active.presentation.group(chat, active.selected) else {
            return;
        };
        let topic_id = match intent {
            Intent::Open(RowKey::Chat(_)) if chat.direct => None,
            Intent::Open(RowKey::Chat(_)) => {
                let action = AppPreferences::read_for(cx, &active.session.owner).chat_click;
                let Some(id) = channel_topic(chat, action) else {
                    return;
                };
                if action == ChatClick::General && !active.presentation.searching() {
                    active.presentation.ensure_topic_visible(chat, id);
                    active.revision = active.revision.wrapping_add(1);
                    cx.notify();
                }
                Some(id)
            }
            Intent::Open(RowKey::Avatar(_)) if !chat.direct => {
                let Some(id) = default_topic(chat) else {
                    return;
                };
                Some(id)
            }
            Intent::Open(RowKey::Topic(_, topic))
                if group.topics.iter().any(|entry| entry.id == topic) =>
            {
                Some(topic)
            }
            Intent::Preview(_, message, topic)
                if preview_target(chat) == Some((message, topic)) =>
            {
                if !active.presentation.searching() {
                    active.presentation.ensure_topic_visible(chat, topic);
                    active.revision = active.revision.wrapping_add(1);
                    cx.notify();
                }
                Some(topic)
            }
            Intent::ToggleChannel(_)
                if !chat.direct && !chat.topics.is_empty() && !active.presentation.searching() =>
            {
                active.presentation.toggle_channel(stream_id);
                active.revision = active.revision.wrapping_add(1);
                cx.notify();
                return;
            }
            Intent::Reveal(_, expected, revision) if revision == active.revision => {
                if let Some(focus) =
                    active
                        .presentation
                        .apply_toggle(chat, active.selected, expected)
                {
                    active.pending_focus = Some(match focus {
                        TopicToggleFocus::Topic(id) => RowKey::Topic(stream_id, id),
                        TopicToggleFocus::Button => RowKey::TopicToggle(stream_id),
                    });
                    active.menu = None;
                    active.revision = active.revision.wrapping_add(1);
                    cx.notify();
                }
                return;
            }
            _ => return,
        };
        let scope = HistoryScope {
            stream_id,
            topic_id,
        };
        active.search_target =
            (active.presentation.searching() && active.selected != Some(scope)).then_some(scope);
        cx.emit(SidebarAction::OpenChat {
            route: ChatRoute {
                owner: origin.owner.clone(),
                scope,
                entry: ChatEntry::Latest,
            },
            generation: origin.generation,
        });
    }

    /// Связывает элемент карточки с выбранным входом и проверяемым намерением.
    fn row_action(active: &Active, intent: Intent, cx: &Context<Self>) -> rows::RowAction {
        let entity = cx.entity().downgrade();
        let origin = active.session.clone();
        Box::new(move |_, app| {
            let _ = entity.update(app, |this, cx| this.activate(&origin, intent, cx));
        })
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

    /// Рисует строку личного чата или верхнюю строку карточки канала.
    fn chat_row(
        active: &mut Active,
        group: &ChatGroup<'_>,
        item: Option<&FolderItem>,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let chat = group.chat;
        let id = chat.id;
        let title: SharedString = chat.name.clone().into();
        let key = RowKey::Chat(id);
        let mut details = active.row_details(key, stream_details(chat, &active.session));
        if let Some(item) = item {
            details.unread = unread_badge_value(
                Some(u64::from(item.active_unread_count)),
                Some(u64::from(item.passive_unread_count)),
                None,
                false,
            );
            details.pinned = item.pinned_at.is_some();
        }
        let settings = AppPreferences::read_for(cx, &active.session.owner);
        let chat_click = settings.chat_click;
        details.compact = settings.appearance.density == ChatDensity::Compact;
        details.hide_avatar = details.compact && chat_click != ChatClick::LatestTopic;
        let opens = chat.direct || channel_topic(chat, chat_click).is_some();
        let focus = opens.then(|| active.row_focus(key, cx));
        let row = if chat.direct {
            details.image = chat
                .metadata
                .direct_peer_id
                .and_then(|user| active.avatars.image(user, cx));
            rows::direct_row(
                id,
                title.clone(),
                details,
                group.selected,
                palette,
                focus.as_ref(),
                Self::row_action(active, Intent::Open(key), cx),
            )
        } else {
            let expandable = !active.presentation.searching() && !chat.topics.is_empty();
            let toggle_focus = expandable.then(|| active.row_focus(RowKey::Chevron(id), cx));
            let preview = preview_target(chat)
                .filter(|_| !details.compact && details.preview.is_some())
                .map(|(message, topic)| PreviewAction {
                    id: message,
                    focus: active.row_focus(RowKey::Preview(id), cx),
                    on_open: Self::row_action(active, Intent::Preview(id, message, topic), cx),
                });
            ChannelRow {
                id,
                title: title.clone(),
                details,
                selected: group.selected,
                color: chat.metadata.color,
                expanded: expandable.then_some(group.expanded),
            }
            .render(
                palette,
                focus.as_ref(),
                toggle_focus.as_ref(),
                ChannelActions {
                    open: opens.then(|| Self::row_action(active, Intent::Open(key), cx)),
                    toggle: Self::row_action(active, Intent::ToggleChannel(id), cx),
                    preview,
                    avatar: (chat_click == ChatClick::LatestTopic && default_topic(chat).is_some())
                        .then(|| {
                            let key = RowKey::Avatar(id);
                            (
                                active.row_focus(key, cx),
                                Self::row_action(active, Intent::Open(key), cx),
                            )
                        }),
                },
            )
        };
        let row = Self::context_trigger(row, active, key, title, palette, cx);
        measured(row, key, &active.bounds)
    }

    /// Рисует видимую тему и открывает её точный маршрут.
    fn topic_row(
        active: &mut Active,
        chat: &Chat,
        topic: &Topic,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let stream_id = chat.id;
        let key = RowKey::Topic(stream_id, topic.id);
        let mut details = active.row_details(key, topic_details(chat, topic, &active.session));
        details.compact = AppPreferences::read_for(cx, &active.session.owner)
            .appearance
            .density
            == ChatDensity::Compact;
        let focus = active.row_focus(key, cx);
        let title: SharedString = topic.name.clone().into();
        let row = TopicRow {
            stream_id,
            id: topic.id,
            title: title.clone(),
            details,
            selected: active.selected
                == Some(HistoryScope {
                    stream_id,
                    topic_id: Some(topic.id),
                }),
            color: topic.metadata.color,
            completed: topic.metadata.is_done,
        }
        .render(
            palette,
            Some(&focus),
            Self::row_action(active, Intent::Open(key), cx),
        );
        let row = Self::context_trigger(row, active, key, title, palette, cx);
        measured(row, key, &active.bounds)
    }

    /// Рисует кнопку текущей ступени раскрытия нескольких тем.
    fn topic_toggle(
        active: &mut Active,
        id: Uuid,
        action: TopicToggleKind,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let key = RowKey::TopicToggle(id);
        let focus = active.row_focus(key, cx);
        let title = match action {
            TopicToggleKind::ShowMore(count) => format!("Показать ещё темы ({count})"),
            TopicToggleKind::ShowCompleted(count) => format!("Показать завершённые темы ({count})"),
            TopicToggleKind::HideExtra => "Скрыть лишние темы".into(),
        };
        let row = rows::topic_toggle(
            id,
            active.revision,
            title.into(),
            action == TopicToggleKind::HideExtra,
            palette,
            Some(&focus),
            Self::row_action(active, Intent::Reveal(id, action, active.revision), cx),
        );
        measured(row, key, &active.bounds)
    }

    /// Собирает карточку в порядке показа и запоминает элементы для навигации.
    fn chat_group(
        active: &mut Active,
        group: ChatGroup<'_>,
        item: Option<&FolderItem>,
        visible: &mut Vec<RowKey>,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let chat = group.chat;
        let settings = AppPreferences::read_for(cx, &active.session.owner);
        let chat_click = settings.chat_click;
        let compact = settings.appearance.density == ChatDensity::Compact;
        if chat.direct || channel_topic(chat, chat_click).is_some() {
            visible.push(RowKey::Chat(chat.id));
        }
        let header = Self::chat_row(active, &group, item, palette, cx);
        if chat.direct {
            return header;
        }
        if chat_click == ChatClick::LatestTopic && default_topic(chat).is_some() {
            visible.push(RowKey::Avatar(chat.id));
        }
        if !compact && preview_target(chat).is_some() {
            visible.push(RowKey::Preview(chat.id));
        }
        if !active.presentation.searching() && !chat.topics.is_empty() {
            visible.push(RowKey::Chevron(chat.id));
        }
        let mut card = rows::channel_card(palette).child(header);
        for (index, topic) in group.topics.into_iter().enumerate() {
            if group.divider_before == Some(index) {
                card = card.child(rows::other_topics_divider(palette));
            }
            visible.push(RowKey::Topic(chat.id, topic.id));
            card = card.child(Self::topic_row(active, chat, topic, palette, cx));
        }
        if let Some(action) = group.toggle {
            visible.push(RowKey::TopicToggle(chat.id));
            card = card.child(Self::topic_toggle(active, chat.id, action, palette, cx));
        }
        card
    }

    /// Возвращает фокус новой строке или к каналу и поиску, если строка исчезла.
    fn restore_focus(
        active: &mut Active,
        visible: &[RowKey],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = active
            .focus
            .iter()
            .find_map(|(key, focus)| focus.is_focused(window).then_some(*key));
        let key = active
            .pending_focus
            .take()
            .or_else(|| previous.filter(|key| !visible.contains(key)));
        if let Some(key) = key {
            let replacement = [
                key,
                RowKey::Chat(key.chat_id()),
                RowKey::Chevron(key.chat_id()),
            ]
            .into_iter()
            .find(|key| visible.contains(key));
            if let Some(key) = replacement {
                let focus = active.focus[&key].clone();
                window.focus(&focus);
                let origin = active.session.clone();
                let sidebar = cx.entity().downgrade();
                window.on_next_frame(move |window, app| {
                    let _ = sidebar.update(app, |view, cx| {
                        if let Some(active) =
                            view.active.as_ref().filter(|entry| entry.session == origin)
                            && focus.is_focused(window)
                        {
                            active.scroll_to_row(key);
                            cx.notify();
                        }
                    });
                });
            } else {
                active.search.read(cx).focus(window);
            }
        }
        let still_visible: HashSet<_> = visible.iter().copied().collect();
        active.focus.retain(|key, _| still_visible.contains(key));
    }

    /// Показывает ошибку и рабочий повтор, сохраняя доступ к загруженным строкам.
    fn failed_status(
        active: &Active,
        palette: Palette,
        has_rows: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let origin = active.session.clone();
        let locale = AppPreferences::read_for(cx, &origin.owner).language;
        let entity = cx.entity().downgrade();
        let mut retry = Button::new("catalog-retry", locale.text("Повторить", "Retry"));
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
                .rounded(PANEL_RADIUS)
                .border_1()
                .border_color(palette.border)
                .bg(palette.card)
                .child(label(
                    palette,
                    TextRole::Caption,
                    locale.text(
                        "Не все чаты обновились",
                        "Some chats could not be refreshed",
                    ),
                ))
                .child(retry)
                .debug_selector(|| "catalog-partial-error".into())
        } else {
            catalog_status(
                palette,
                locale.text("Не удалось загрузить чаты", "Could not load chats"),
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
        let revision = active.revision;
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
                    this.update_avatar_users(&session, revision, users, cx);
                });
            });
        })
    }

    /// Обновляет подписки, только пока измеренный кадр относится к текущему входу и списку.
    fn update_avatar_users(
        &mut self,
        session: &AccountSession,
        revision: u64,
        users: Vec<Uuid>,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = &mut self.active
            && &active.session == session
            && active.revision == revision
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
        let locale = AppPreferences::read(cx).language;
        let t = |ru, en| locale.text(ru, en);
        let id = self.active.as_ref().map_or_else(
            || "sidebar-unselected".into(),
            |active| ElementId::View(active.search.entity_id()),
        );
        let mut panel = div()
            .id(id)
            .debug_selector(|| "workspace-sidebar-panel".into())
            .w_full()
            .h_full()
            .rounded(PANEL_RADIUS)
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(palette.surface);

        if self.new_chats.read(cx).is_open() {
            return self.with_folder_rail(panel.child(self.new_chats.clone()));
        }
        let Some(active) = &mut self.active else {
            if self.menu_focus.is_focused(window) {
                window.blur();
            }
            return self.with_folder_rail(panel.child(catalog_status(
                palette,
                t("Выберите аккаунт", "Select an account"),
                None,
            )));
        };
        if active.menu.is_none() && self.menu_focus.is_focused(window) {
            active.search.read(cx).focus(window);
        }
        active.search.update(cx, |search, cx| {
            search.set_placeholder(t("Найти", "Search"), cx)
        });
        panel = panel.child(
            div()
                .px(px(12.))
                .pt(px(16.))
                .pb(px(12.))
                .flex_none()
                .flex()
                .items_center()
                .gap_1()
                .child(div().flex_1().min_w_0().child(search::render(
                    &active.search,
                    palette,
                    window,
                    cx,
                )))
                .child(NewChats::opener(
                    &self.new_chats,
                    palette,
                    locale,
                    active.presentation.searching(),
                    cx,
                )),
        );
        let catalog = active.catalog.clone();
        let snapshot = active.folder_view.clone();
        let chats = active
            .session
            .catalog_chats(catalog.as_ref(), snapshot.as_deref());
        let status = catalog.as_ref().map(|list| list.status);
        let searching = active.presentation.searching();
        let settings = AppPreferences::read_for(cx, &active.session.owner);
        let sort = settings.appearance.sort;
        let language = settings.language;
        if active.presentation.set_sort(sort) {
            active.revision = active.revision.wrapping_add(1);
            active.menu = None;
            active.pending_focus = None;
            active.apply_route_reveal();
        }
        let mut entries = folders::folder_rows(
            snapshot.as_deref(),
            chats.unwrap_or_default(),
            active.folder,
            searching,
        );
        order::sort_chats(&mut entries, sort, language);
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
                catalog_status(palette, t("Загрузка чатов…", "Loading chats…"), None)
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
        let mut found = false;
        let mut heading = None;
        let groups_visible = searching && !active.folder.is_nil();
        active.bounds.borrow_mut().clear();
        for (chat, item, outside) in entries {
            let Some(group) = active.presentation.group(chat, active.selected) else {
                continue;
            };
            found = true;
            if groups_visible && heading != Some(outside) {
                heading = Some(outside);
                let (title, id) = if outside {
                    (
                        t("В других папках", "In other folders"),
                        "catalog-search-outside-folder",
                    )
                } else {
                    (
                        t("В этой папке", "In this folder"),
                        "catalog-search-in-folder",
                    )
                };
                rows = rows.child(
                    label(palette, TextRole::Caption, title)
                        .py_1()
                        .px_2()
                        .debug_selector(move || id.into()),
                );
                peers.push(None);
            }
            peers.push(if chat.direct {
                chat.metadata.direct_peer_id.filter(|id| !id.is_nil())
            } else {
                None
            });
            rows = rows.child(Self::chat_group(
                active,
                group,
                item,
                &mut results,
                palette,
                cx,
            ));
        }
        if !found && status == Some(ChatListStatus::Ready) {
            let message = if searching {
                t("Ничего не найдено", "No results found")
            } else if active.folder.is_nil() {
                t("У вас пока нет чатов", "You have no chats yet")
            } else {
                t("В папке пока нет чатов", "This folder has no chats yet")
            };
            panel = panel.child(
                catalog_status(palette, message, None).debug_selector(|| "catalog-empty".into()),
            );
        }
        Self::restore_focus(active, &results, window, cx);
        if searching {
            results.retain(|key| !matches!(key, RowKey::Preview(_)));
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
        .rounded(PANEL_RADIUS)
        .border_1()
        .border_color(palette.border)
        .bg(palette.card)
        .child(label(palette, TextRole::Label, text))
        .children(action)
}

/// Сохраняет фактическую позицию вложенной строки для прокрутки и Shift+F10.
fn measured(row: Stateful<Div>, key: RowKey, bounds: &RowBounds) -> Div {
    let row_bounds = bounds.clone();
    div()
        .w_full()
        .min_w_0()
        .flex_none()
        .child(row)
        .on_children_prepainted(move |children, _, _| {
            if let Some(bounds) = children.first() {
                row_bounds.borrow_mut().insert(key, *bounds);
            }
        })
}

/// Открывает общую тему, только если она есть в доступном каталоге канала.
fn default_topic(chat: &Chat) -> Option<Uuid> {
    chat.metadata
        .default_topic_id
        .filter(|id| chat.topics.iter().any(|topic| topic.id == *id))
}

/// Выбирает доступную тему последнего сообщения с запасным переходом в общую тему.
fn channel_topic(chat: &Chat, action: ChatClick) -> Option<Uuid> {
    if action == ChatClick::LatestTopic {
        if let Some((_, topic)) = preview_target(chat) {
            return Some(topic);
        }
        if let Some(message) = chat.metadata.last_message_id
            && let Some(topic) = chat
                .topics
                .iter()
                .find(|topic| topic.metadata.last_message_id == Some(message))
        {
            return Some(topic.id);
        }
    }
    default_topic(chat)
}

/// Находит точное сообщение превью и его тему в доступном каталоге.
fn preview_target(chat: &Chat) -> Option<(Uuid, Uuid)> {
    let preview = chat.metadata.preview.as_ref()?;
    if chat.metadata.last_message_id == Some(preview.message_id)
        && chat.topics.iter().any(|topic| topic.id == preview.topic_id)
    {
        return Some((preview.message_id, preview.topic_id));
    }
    None
}

/// Готовит сведения строки канала или личного чата из текущего снимка.
fn stream_details(chat: &Chat, session: &AccountSession) -> RowDetails {
    let metadata = &chat.metadata;
    let muted = order::stream_muted(chat);
    let mut details = message_details(metadata.preview.as_ref(), session);
    details.unread = unread_badge_value(
        metadata.active_unread_count,
        metadata.passive_unread_count,
        metadata.unread_count,
        muted,
    );
    details.muted = muted;
    details
}

/// Готовит сведения темы, учитывая режим уведомлений родительского канала.
fn topic_details(chat: &Chat, topic: &Topic, session: &AccountSession) -> RowDetails {
    let metadata = &topic.metadata;
    let muted = order::topic_muted(chat, topic);
    let mut details = message_details(metadata.preview.as_ref(), session);
    details.unread = unread_badge_value(
        metadata.active_unread_count,
        metadata.passive_unread_count,
        metadata.unread_count,
        muted,
    );
    details.muted = muted;
    details
}

/// Оформляет безопасное превью и время в том же формате, который использует история.
fn message_details(preview: Option<&SidebarPreview>, session: &AccountSession) -> RowDetails {
    let Some(preview) = preview else {
        return RowDetails::default();
    };
    let own = Uuid::parse_str(&session.owner.user_id).ok() == Some(preview.author_id);
    RowDetails {
        preview: Some(ChatRowPreview {
            sender: if own {
                Some("Вы".into())
            } else {
                preview.author_name.clone().map(Into::into)
            },
            text: preview.text.clone().into(),
        }),
        time: preview
            .created_at
            .as_str()
            .get(11..16)
            .map(|time| time.to_owned().into()),
        ..RowDetails::default()
    }
}

/// Показывает активные непрочитанные первыми, иначе использует приглушённый счётчик.
fn unread_badge_value(
    active: Option<u64>,
    passive: Option<u64>,
    total: Option<u64>,
    muted: bool,
) -> Option<(u64, BadgeTone)> {
    if let Some(count) = active.or(total).filter(|count| *count > 0) {
        let tone = if muted {
            BadgeTone::Muted
        } else {
            BadgeTone::Accent
        };
        return Some((count, tone));
    }
    passive
        .filter(|count| *count > 0)
        .map(|count| (count, BadgeTone::Muted))
}

/// Узнаёт тот же неизменяемый снимок. Новые метаданные публикуются новым Arc.
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
