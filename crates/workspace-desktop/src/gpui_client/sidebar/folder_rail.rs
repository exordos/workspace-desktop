//! Показывает системные и пользовательские папки в панели каталога.
//! Сначала описаны данные от оболочки, затем действия, меню и быстрый выбор папки.

use std::collections::{HashMap, HashSet};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, KeyBinding, Pixels, Point, ScrollHandle,
    Subscription, Window, point, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::messenger::folders::{ALL_CHATS_ID, FolderKind};

use super::super::ui::{Icon, TextInput, TextInputChanged, TextInputSubmitted, popup::MenuInput};

#[cfg(all(test, feature = "gpui-test"))]
mod tests;
mod view;

gpui::actions!(
    folder_rail,
    [
        OpenMenu,
        PreviousFolder,
        NextFolder,
        CloseQuickList,
        KeepQuickFocus
    ]
);

/// Одна кнопка панели, подготовленная из снимка текущего аккаунта.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RailEntry {
    pub id: Uuid,            // UUID папки на сервере.
    pub title: String,       // Название для панели и поиска.
    pub kind: FolderKind,    // Вид папки для значка и доступных действий.
    pub color: Option<u32>,  // ARGB пользовательской папки, если цвет задан.
    pub unread: Option<u32>, // Серверный счётчик, если он известен.
}

/// Входные данные и настройки панели папок текущего аккаунта.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RailState {
    pub entries: Vec<RailEntry>, // Папки в порядке Core.
    pub selected: Uuid,          // UUID выбранной папки.
    pub vertical: bool,          // Панель слева. Иначе папки идут над каталогом.
    pub show_system: bool,       // Показывать «Личные» и «Каналы» рядом со «Все чаты».
    pub can_manage: bool,        // Доступны ли действия, которые меняют серверные папки.
    pub needs_refresh: bool,     // Можно ли запросить повтор сверки снимка папок.
}

/// Намерение пользователя, которое оболочка связывает с текущим аккаунтом.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RailAction {
    Select(Uuid),    // Открыть список выбранной папки.
    ToggleSystem,    // Переключить видимость дополнительных системных папок.
    TogglePlacement, // Переместить панель.
    Create,          // Открыть форму создания.
    Edit(Uuid),      // Открыть форму изменения названия и цвета.
    Delete(Uuid),    // Спросить подтверждение удаления одной папки.
    Retry,           // Повторить сверку папок.
}

/// Открытая панель действий или быстрый выбор папки.
enum Overlay {
    Menu {
        id: Uuid,              // Папка под меню.
        anchor: Point<Pixels>, // Координаты раскрытия в окне.
        active: usize,         // Индекс подсвеченного доступного действия.
    },
    Quick {
        anchor: Point<Pixels>, // Координаты раскрытия рядом с кнопкой списка.
        active: Option<Uuid>,  // Подсвеченная папка. None выбирает из текущих результатов.
        _submit: Subscription, // Подтверждение поля, привязанное к этому окну.
    },
}

/// Строка меню с актуальной доступностью.
struct MenuItem {
    id: &'static str,    // Стабильный идентификатор кнопки.
    label: &'static str, // Подпись действия.
    icon: Icon,          // Значок в начале строки.
    action: RailAction,  // Событие для оболочки.
    enabled: bool,       // Доступна ли строка клавиатуре и мыши.
}

/// Панель папок. Хранит только состояние прокрутки, фокуса и открытого меню.
pub(super) struct FolderRail {
    state: RailState,                    // Снимок активного аккаунта.
    serial: u64,                         // Номер показа. Меняется при каждом сбросе владельца.
    focuses: HashMap<Uuid, FocusHandle>, // Фокус кнопки остаётся у UUID при обновлении бейджа.
    scroll: ScrollHandle,                // Прокрутка папок в текущем размещении.
    first: ScrollHandle, // Границы неподвижной папки «Все чаты» в вертикальной панели.
    quick_button: ScrollHandle, // Границы кнопки быстрого выбора.
    quick_focus: FocusHandle, // Фокус кнопки быстрого выбора.
    quick_scroll: ScrollHandle, // Прокрутка результатов поиска по папкам.
    query: Entity<TextInput>, // Поле поиска в быстром выборе.
    _query_changed: Subscription, // Сбрасывает подсветку и прокрутку при новом запросе.
    menu_focus: FocusHandle, // Удерживает клавиатурный ввод в контекстном меню.
    overlay: Option<Overlay>, // Текущее всплывающее меню.
    overflow: bool,      // Вертикальный список тесен в области, выделенной окном.
}

impl RailState {
    /// Показывает имя хотя бы одной папки, пока её серверные данные загружаются.
    fn with_all_chats(mut self) -> Self {
        let all = self
            .entries
            .iter()
            .position(|entry| entry.id == ALL_CHATS_ID)
            .map(|index| self.entries.remove(index))
            .unwrap_or(RailEntry {
                id: ALL_CHATS_ID,
                title: "Все чаты".into(),
                kind: FolderKind::All,
                color: None,
                unread: None,
            });
        self.entries.insert(0, all);
        self
    }

    /// Возвращает папки, доступные в обоих вариантах размещения и быстром выборе.
    fn visible(&self) -> impl Iterator<Item = &RailEntry> {
        self.entries.iter().filter(|entry| {
            self.show_system || !matches!(entry.kind, FolderKind::Personal | FolderKind::Channels)
        })
    }
}

impl FolderRail {
    /// Создаёт панель и поле быстрого выбора с пустым поиском.
    pub(super) fn new(cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| TextInput::new("folder-rail-query", "Найти папку", false, cx));
        let changed = cx.subscribe(&query, |rail, _, _: &TextInputChanged, cx| {
            if let Some(Overlay::Quick { active, .. }) = &mut rail.overlay {
                *active = None;
                rail.quick_scroll.set_offset(point(px(0.), px(0.)));
                cx.notify();
            }
        });
        Self {
            state: RailState {
                entries: Vec::new(),
                selected: ALL_CHATS_ID,
                vertical: true,
                show_system: true,
                can_manage: false,
                needs_refresh: false,
            }
            .with_all_chats(),
            serial: 0,
            focuses: HashMap::new(),
            scroll: ScrollHandle::new(),
            first: ScrollHandle::new(),
            quick_button: ScrollHandle::new(),
            quick_focus: cx.focus_handle(),
            quick_scroll: ScrollHandle::new(),
            query,
            _query_changed: changed,
            menu_focus: cx.focus_handle(),
            overlay: None,
            overflow: false,
        }
    }

    /// Регистрирует клавиши, которые действуют только внутри кнопки или быстрого списка.
    pub(super) fn bind_keys(app: &mut App) {
        app.bind_keys([
            KeyBinding::new("shift-f10", OpenMenu, Some("FolderRailButton")),
            KeyBinding::new("up", PreviousFolder, Some("FolderQuickList")),
            KeyBinding::new("down", NextFolder, Some("FolderQuickList")),
            KeyBinding::new("escape", CloseQuickList, Some("FolderQuickList")),
            KeyBinding::new("tab", KeepQuickFocus, Some("FolderQuickList")),
            KeyBinding::new("shift-tab", KeepQuickFocus, Some("FolderQuickList")),
        ]);
    }

    /// Применяет серверные данные и локальные настройки, сохраняя фокус живых кнопок.
    pub(super) fn set_state(&mut self, state: RailState, cx: &mut Context<Self>) {
        let state = state.with_all_chats();
        if state == self.state {
            return;
        }
        if self.state.vertical != state.vertical {
            self.overlay = None;
            self.scroll = ScrollHandle::new();
            self.overflow = false;
        }
        if let Some(Overlay::Menu { id, .. }) = self.overlay
            && !state.visible().any(|entry| entry.id == id)
        {
            self.overlay = None;
        }
        let visible: HashSet<_> = state.visible().map(|entry| entry.id).collect();
        self.focuses.retain(|id, _| visible.contains(id));
        self.state = state;
        cx.notify();
    }

    /// Закрывает меню и освобождает фокус, поисковый ввод и прокрутку старого аккаунта.
    pub(super) fn reset(&mut self, cx: &mut Context<Self>) {
        self.serial = self.serial.wrapping_add(1);
        self.overlay = None;
        self.focuses.clear();
        self.scroll = ScrollHandle::new();
        self.overflow = false;
        self.query.update(cx, |query, cx| query.clear(cx));
        cx.notify();
    }

    /// Возвращает запасную цель фокуса, если кнопка удалённой папки исчезла.
    pub(super) fn all_chats_focus(&self) -> Option<FocusHandle> {
        self.focuses.get(&ALL_CHATS_ID).cloned()
    }

    /// Выполняет выбор только для видимой папки текущего показа.
    fn select(&self, serial: u64, id: Uuid, cx: &mut Context<Self>) {
        if self.serial == serial && self.state.visible().any(|entry| entry.id == id) {
            cx.emit(RailAction::Select(id));
        }
    }

    /// Готовит действия меню, проверяя права и вид папки по текущему снимку.
    fn menu_items(&self, id: Uuid) -> Vec<MenuItem> {
        let custom = self.state.can_manage
            && self
                .state
                .visible()
                .any(|entry| entry.id == id && entry.kind == FolderKind::Custom);
        let mut items = vec![
            MenuItem {
                id: "folder-rename",
                label: "Переименовать папку",
                icon: Icon::Folder,
                action: RailAction::Edit(id),
                enabled: custom,
            },
            MenuItem {
                id: "folder-placement",
                label: if self.state.vertical {
                    "Разместить горизонтально"
                } else {
                    "Разместить вертикально"
                },
                icon: Icon::Folders,
                action: RailAction::TogglePlacement,
                enabled: true,
            },
            MenuItem {
                id: "folder-system-visibility",
                label: if self.state.show_system {
                    "Скрыть системные папки"
                } else {
                    "Показать системные папки"
                },
                icon: Icon::Folder,
                action: RailAction::ToggleSystem,
                enabled: true,
            },
            MenuItem {
                id: "folder-delete",
                label: "Удалить папку",
                icon: Icon::Close,
                action: RailAction::Delete(id),
                enabled: custom,
            },
        ];
        if self.state.needs_refresh {
            items.push(MenuItem {
                id: "folder-refresh",
                label: "Обновить папки",
                icon: Icon::Folders,
                action: RailAction::Retry,
                enabled: self.state.can_manage,
            });
        }
        items
    }

    /// Открывает меню текущей папки и выделяет первое доступное действие.
    fn open_menu(
        &mut self,
        serial: u64,
        id: Uuid,
        anchor: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.serial != serial || !self.state.visible().any(|entry| entry.id == id) {
            return;
        }
        let active = self
            .menu_items(id)
            .iter()
            .position(|item| item.enabled)
            .unwrap_or(0);
        self.overlay = Some(Overlay::Menu { id, anchor, active });
        window.focus(&self.menu_focus);
        cx.notify();
    }

    /// Выбирает доступный пункт меню, возвращает фокус кнопке и передаёт действие.
    fn pick_menu(&mut self, action: RailAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Overlay::Menu { id, .. }) = self.overlay else {
            return;
        };
        if self
            .menu_items(id)
            .iter()
            .any(|item| item.action == action && item.enabled)
        {
            self.close_overlay(window, cx);
            cx.emit(action);
        }
    }

    /// Перемещает подсветку между доступными пунктами или закрывает меню.
    fn menu_input(&mut self, input: MenuInput, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Overlay::Menu { id, active, .. }) = self.overlay else {
            return;
        };
        let items = self.menu_items(id);
        let enabled: Vec<_> = items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| item.enabled.then_some(i))
            .collect();
        let index = enabled.iter().position(|ix| *ix == active).unwrap_or(0);
        let target = match input {
            MenuInput::Previous => index.checked_sub(1).unwrap_or(enabled.len() - 1),
            MenuInput::Next => (index + 1) % enabled.len(),
            MenuInput::First => 0,
            MenuInput::Last => enabled.len() - 1,
            MenuInput::Select => {
                self.pick_menu(items[enabled[index]].action, window, cx);
                return;
            }
            MenuInput::Close => {
                self.close_overlay(window, cx);
                return;
            }
        };
        if let Some(Overlay::Menu { active, .. }) = &mut self.overlay {
            *active = enabled[target];
            cx.notify();
        }
    }

    /// Закрывает всплывающий слой и возвращает фокус его кнопке.
    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.overlay.take() {
            Some(Overlay::Menu { id, .. }) => {
                if let Some(focus) = self.focuses.get(&id) {
                    window.focus(focus);
                }
            }
            Some(Overlay::Quick { .. }) => {
                if self.overflow {
                    window.focus(&self.quick_focus);
                } else if let Some(focus) = self.focuses.get(&self.state.selected) {
                    window.focus(focus);
                }
            }
            None => return,
        }
        cx.notify();
    }

    /// Подготавливает быстрый список и сразу переводит ввод в поиск.
    fn open_quick(&mut self, serial: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.serial != serial || !self.state.vertical || !self.overflow {
            return;
        }
        self.query.update(cx, |query, cx| query.clear(cx));
        self.quick_scroll = ScrollHandle::new();
        if let Some(index) = self
            .state
            .visible()
            .position(|entry| entry.id == self.state.selected)
        {
            self.quick_scroll.scroll_to_item(index);
        }
        let submit = cx.subscribe_in(
            &self.query,
            window,
            |rail, _, _: &TextInputSubmitted, window, cx| {
                // Подтверждение приходит из TextInput, в том числе после системного ввода.
                let rows = rail.quick_entries(cx);
                if let Some(index) = rail.quick_active(&rows) {
                    let id = rows[index].id;
                    rail.pick_quick(id, window, cx);
                }
            },
        );
        let bounds = self.quick_button.bounds();
        self.overlay = Some(Overlay::Quick {
            anchor: point(bounds.right(), bounds.top()),
            active: None,
            _submit: submit,
        });
        self.query.read(cx).focus(window);
        cx.notify();
    }

    /// Возвращает видимые папки, отфильтрованные по названию без учёта регистра.
    fn quick_entries(&self, app: &App) -> Vec<&RailEntry> {
        let query = self.query.read(app).text().trim().to_lowercase();
        self.state
            .visible()
            .filter(|entry| query.is_empty() || entry.title.to_lowercase().contains(&query))
            .collect()
    }

    /// Находит подсвеченную строку после фильтрации и обновления данных.
    fn quick_active(&self, rows: &[&RailEntry]) -> Option<usize> {
        let Some(Overlay::Quick { active, .. }) = self.overlay else {
            return None;
        };
        let target = active.unwrap_or(self.state.selected);
        rows.iter()
            .position(|entry| entry.id == target)
            .or_else(|| (!rows.is_empty()).then_some(0))
    }

    /// Перемещает подсветку, не забирая стрелки у незавершённого системного ввода.
    fn move_quick(&mut self, direction: isize, cx: &mut Context<Self>) {
        if self.query.read(cx).is_composing() {
            cx.propagate();
            return;
        }
        let rows = self.quick_entries(cx);
        let Some(index) = self.quick_active(&rows) else {
            return;
        };
        let next = index.saturating_add_signed(direction).min(rows.len() - 1);
        let id = rows[next].id;
        if let Some(Overlay::Quick { active, .. }) = &mut self.overlay {
            *active = Some(id);
            self.quick_scroll.scroll_to_item(next);
            cx.notify();
        }
    }

    /// Выбирает только строку открытого списка, которая всё ещё видна в поиске.
    fn pick_quick(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.overlay, Some(Overlay::Quick { .. }))
            && self.quick_entries(cx).iter().any(|entry| entry.id == id)
        {
            self.close_overlay(window, cx);
            self.select(self.serial, id, cx);
        }
    }
}

impl EventEmitter<RailAction> for FolderRail {}
