//! Связывает панель и формы со снимком папок выбранного входа.
//! Общий каталог остаётся доступен до первого снимка папок.

use gpui::{
    Context, Div, Entity, EventEmitter, Stateful, Subscription, Window, div, point, prelude::*, px,
};
use workspace_core::messenger::folders::FolderKind;

use super::{
    AccountSession, Active, Arc, Chat, FolderCommand, FolderCommandError, FolderDialog, FolderItem,
    FolderRail, FolderRailPreferences, FolderSnapshot, HashMap, HashSet, OwnerContext, Sidebar,
    Uuid,
    folder_rail::{RailAction, RailEntry, RailState},
};

const RAIL_WIDTH: f32 = 72.;
const RAIL_GAP: f32 = 4.;

/// Запрос окна с исходным входом и номером операции для позднего ответа.
#[derive(Clone)]
pub(in crate::gpui_client) enum FolderIntent {
    Command {
        session: AccountSession, // Вход, где нажали действие.
        operation: Uuid,         // Ключ ответа, пока тот же запрос ожидается в окне.
        command: FolderCommand,  // Серверное изменение или запрос на сверку.
    },
    Preferences {
        session: AccountSession,      // Владелец локальной настройки.
        value: FolderRailPreferences, // Новые размещение и видимость.
    },
}

/// Форма, которую откроем при следующем проходе окна с доступным фокусом.
enum PendingDialog {
    Create,       // Новая папка.
    Edit(Uuid),   // Папка, которую изменяют.
    Delete(Uuid), // Папка, которую требуется подтвердить.
}

/// Состояние панели текущего окна и последние данные рабочего потока.
pub(super) struct FolderPane {
    snapshots: HashMap<OwnerContext, Arc<FolderSnapshot>>, // Снимки всех доступных входов.
    preferences: HashMap<OwnerContext, FolderRailPreferences>, // Последние локальные настройки.
    rail: Entity<FolderRail>,                              // Меню, быстрый выбор и кнопки папок.
    dialog: Entity<FolderDialog>,                          // Форма поверх рабочего окна.
    requested: Option<PendingDialog>,                      // Действие, ожидающее контекст окна.
    _subscriptions: [Subscription; 2],                     // События панели и формы.
}

impl FolderPane {
    /// Создаёт панель и передаёт действия родительскому сайдбару.
    pub(super) fn new(cx: &mut Context<Sidebar>) -> Self {
        let rail = cx.new(FolderRail::new);
        let dialog = cx.new(FolderDialog::new);
        let rail_subscription = cx.subscribe(&rail, |view, _, action: &RailAction, cx| {
            view.rail_action(action, cx);
        });
        let dialog_subscription = cx.subscribe(&dialog, |view, _, command: &FolderCommand, cx| {
            view.send_folder_command(command.clone(), true, cx);
        });
        Self {
            snapshots: HashMap::new(),
            preferences: HashMap::new(),
            rail,
            dialog,
            requested: None,
            _subscriptions: [rail_subscription, dialog_subscription],
        }
    }

    /// Возвращает сохранённую настройку, если профиль уже её прислал.
    pub(super) fn preferences(&self, owner: &OwnerContext) -> FolderRailPreferences {
        self.preferences.get(owner).copied().unwrap_or_default()
    }

    /// Закрывает меню и форму при уходе с предыдущего входа или раздела.
    pub(super) fn reset(&mut self, cx: &mut Context<Sidebar>) {
        self.requested = None;
        self.rail.update(cx, |rail, cx| rail.reset(cx));
        self.dialog.update(cx, |dialog, cx| dialog.reset(cx));
    }
}

impl EventEmitter<FolderIntent> for Sidebar {}

impl Sidebar {
    /// Подтверждает, что обход Tab дошёл до поиска в рабочем окне.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_search_focused(
        &self,
        window: &Window,
        app: &gpui::App,
    ) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.search.read(app).is_focused(window))
    }

    /// Проверяет подписку рабочего окна на снимок с только что созданной папкой.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_has_folder(&self, id: Uuid) -> bool {
        self.active
            .as_ref()
            .and_then(|active| active.folder_view.as_ref())
            .and_then(|snapshot| snapshot.folders.as_ref())
            .is_some_and(|folders| folders.iter().any(|folder| folder.id == id))
    }

    /// Получает новые серверные данные и заменяет только проекцию текущего входа.
    pub(in crate::gpui_client) fn set_folder_snapshots(
        &mut self,
        snapshots: HashMap<OwnerContext, Arc<FolderSnapshot>>,
        cx: &mut Context<Self>,
    ) {
        self.folders.snapshots = snapshots;
        self.update_folder_view(cx);
    }

    /// Применяет настройки из профиля для выбранного владельца.
    pub(in crate::gpui_client) fn set_folder_preferences(
        &mut self,
        preferences: HashMap<OwnerContext, FolderRailPreferences>,
        cx: &mut Context<Self>,
    ) {
        self.folders.preferences = preferences;
        if let Some(active) = &mut self.active {
            active.folder_settings = self.folders.preferences(&active.session.owner);
        }
        self.update_folder_view(cx);
    }

    /// Разрешает серверные действия только после подтверждения сессии.
    pub(in crate::gpui_client) fn set_folder_online(
        &mut self,
        online: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = &mut self.active
            && active.folder_online != online
        {
            active.folder_online = online;
            self.update_folder_rail(cx);
        }
    }

    /// Даёт корню окна форму для отрисовки поверх соседних панелей.
    pub(in crate::gpui_client) fn folder_dialog(&self) -> Entity<FolderDialog> {
        self.folders.dialog.clone()
    }

    /// Возвращает добавочную ширину при вертикальном размещении папок.
    pub(in crate::gpui_client) fn folder_rail_width(&self) -> f32 {
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.shows_rail() && active.folder_settings.vertical)
        {
            RAIL_WIDTH + RAIL_GAP
        } else {
            0.
        }
    }

    /// Сбрасывает временные действия при уходе с рабочего раздела.
    pub(in crate::gpui_client) fn dismiss_folders(&mut self, cx: &mut Context<Self>) {
        self.folders.reset(cx);
        if let Some(active) = &mut self.active {
            active.folder_pending = None;
            active.menu = None;
        }
        self.update_folder_rail(cx);
    }

    /// Сообщает о неудачной записи настройки, пока виден исходный выбор.
    pub(in crate::gpui_client) fn folder_preferences_failed(
        &mut self,
        session: &AccountSession,
        value: FolderRailPreferences,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = &mut self.active
            && &active.session == session
            && active.folder_settings == value
        {
            active.folder_error = Some("Настройка папок действует в этом окне, но не сохранилась");
            cx.notify();
        }
    }

    /// Отдаёт ответ только той операции, которая всё ещё ожидает его в этом каталоге.
    pub(in crate::gpui_client) fn finish_folder_command(
        &mut self,
        session: &AccountSession,
        operation: Uuid,
        result: Result<(), FolderCommandError>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| &active.session == session)
        else {
            return;
        };
        let Some((pending, dialog)) = active.folder_pending else {
            return;
        };
        if pending != operation {
            return;
        }
        active.folder_pending = None;
        if dialog {
            self.folders
                .dialog
                .update(cx, |dialog, cx| dialog.finish(result, cx));
        } else {
            active.folder_error = result.err().map(folder_error);
        }
        self.update_folder_rail(cx);
    }

    /// Применяет снимок только к совпадающему владельцу и поколению.
    pub(super) fn update_folder_view(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = &mut self.active {
            active.folder_view = self
                .folders
                .snapshots
                .get(&active.session.owner)
                .filter(|snapshot| {
                    snapshot.generation == active.session.generation
                        && snapshot.owner == active.session.owner
                })
                .cloned();
            let complete = active
                .folder_view
                .as_ref()
                .and_then(|snapshot| snapshot.folders.as_ref());
            if !active.folder.is_nil()
                && complete.is_some_and(|folders| {
                    !folders.iter().any(|folder| {
                        folder.id == active.folder
                            && (active.folder_settings.show_system
                                || !matches!(
                                    folder.kind,
                                    FolderKind::Personal | FolderKind::Channels
                                ))
                    })
                })
            {
                active.select_folder(Uuid::nil());
            }
            if let Some(menu) = &active.menu
                && (active.menu_action(menu.key, cx).is_none()
                    || (active.search.read(cx).text().trim().is_empty()
                        && !active.folder_contains(match menu.key {
                            super::RowKey::Chat(chat) | super::RowKey::Topic(chat, _) => chat,
                        })))
            {
                active.menu = None;
            }
        }
        self.update_folder_rail(cx);
    }

    /// Передаёт панели готовые подписи и абсолютные счётчики Core.
    fn update_folder_rail(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = &self.active {
            let view = active.folder_view.as_deref();
            let mut entries: Vec<_> = view
                .and_then(|snapshot| snapshot.folders.as_ref())
                .into_iter()
                .flatten()
                .map(|folder| RailEntry {
                    id: folder.id,
                    title: match folder.kind {
                        FolderKind::All => "Все чаты".into(),
                        FolderKind::Personal => "Личные".into(),
                        FolderKind::Channels => "Каналы".into(),
                        _ => folder.title.clone(),
                    },
                    kind: folder.kind,
                    color: folder.background_color_value,
                    unread: view.and_then(|snapshot| snapshot.unread_count(folder.id)),
                })
                .collect();
            if !entries.iter().any(|entry| entry.id.is_nil()) {
                entries.insert(
                    0,
                    RailEntry {
                        id: Uuid::nil(),
                        title: "Все чаты".into(),
                        kind: FolderKind::All,
                        color: None,
                        unread: view.and_then(|snapshot| snapshot.all_chats_unread),
                    },
                );
            }
            let state = RailState {
                entries,
                selected: active.folder,
                vertical: active.folder_settings.vertical,
                show_system: active.folder_settings.show_system,
                can_manage: active.folder_online && active.folder_pending.is_none(),
                needs_refresh: view.is_none_or(|snapshot| snapshot.needs_refresh),
            };
            self.folders
                .rail
                .update(cx, |rail, cx| rail.set_state(state, cx));
        }
        cx.notify();
    }

    /// Выбирает папку, сохраняет настройку или запрашивает форму.
    fn rail_action(&mut self, action: &RailAction, cx: &mut Context<Self>) {
        let Some(active) = &mut self.active else {
            return;
        };
        match action {
            RailAction::Select(id) => {
                if active.folder != *id {
                    active.search.update(cx, |search, cx| search.clear(cx));
                }
                active.select_folder(*id);
                self.update_folder_rail(cx);
            }
            RailAction::ToggleSystem | RailAction::TogglePlacement => {
                let value = &mut active.folder_settings;
                if matches!(action, RailAction::ToggleSystem) {
                    value.show_system = !value.show_system;
                } else {
                    value.vertical = !value.vertical;
                }
                let (session, value) = (active.session.clone(), *value);
                self.folders
                    .preferences
                    .insert(session.owner.clone(), value);
                cx.emit(FolderIntent::Preferences { session, value });
                self.update_folder_view(cx);
            }
            RailAction::Retry => self.send_folder_command(FolderCommand::Retry, false, cx),
            RailAction::Create | RailAction::Edit(_) | RailAction::Delete(_) => {
                if active.folder_online && active.folder_pending.is_none() {
                    active.menu = None;
                    self.folders.requested = match *action {
                        RailAction::Create => Some(PendingDialog::Create),
                        RailAction::Edit(id) => Some(PendingDialog::Edit(id)),
                        RailAction::Delete(id) => Some(PendingDialog::Delete(id)),
                        _ => None,
                    };
                    cx.notify();
                }
            }
        }
    }

    /// Открывает форму для актуальной папки и текущего фокуса окна.
    pub(super) fn open_requested_folder_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self.folders.requested.take() else {
            return;
        };
        if matches!(request, PendingDialog::Create) {
            self.folders
                .dialog
                .update(cx, |dialog, cx| dialog.open_create(window, cx));
        } else if let Some(active) = &self.active {
            let (id, delete) = match request {
                PendingDialog::Edit(id) => (id, false),
                PendingDialog::Delete(id) => (id, true),
                PendingDialog::Create => unreachable!(),
            };
            if let Some(folder) = active
                .folder_view
                .as_ref()
                .and_then(|snapshot| snapshot.folders.as_ref())
                .and_then(|folders| {
                    folders
                        .iter()
                        .find(|folder| folder.id == id && folder.kind == FolderKind::Custom)
                })
            {
                let after_delete = self.folders.rail.read(cx).all_chats_focus();
                self.folders.dialog.update(cx, |dialog, cx| {
                    if delete {
                        dialog.open_delete(id, folder.title.clone(), after_delete, window, cx);
                    } else {
                        dialog.open_edit(
                            id,
                            folder.title.clone(),
                            folder.background_color_value,
                            window,
                            cx,
                        );
                    }
                });
            }
        }
    }

    /// Отправляет одну команду и запоминает её до ответа или смены входа.
    pub(super) fn send_folder_command(
        &mut self,
        command: FolderCommand,
        dialog: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = &mut self.active else {
            return;
        };
        if !active.folder_online || active.folder_pending.is_some() {
            if dialog {
                self.folders.dialog.update(cx, |dialog, cx| {
                    dialog.finish(Err(FolderCommandError::Unavailable), cx)
                });
            }
            return;
        }
        let operation = Uuid::new_v4();
        active.folder_pending = Some((operation, dialog));
        active.folder_error = None;
        cx.emit(FolderIntent::Command {
            session: active.session.clone(),
            operation,
            command,
        });
        self.update_folder_rail(cx);
    }

    /// Размещает панель слева либо над полем поиска.
    pub(super) fn with_folder_rail(&self, panel: Stateful<Div>) -> Div {
        if self
            .active
            .as_ref()
            .is_some_and(|active| !active.shows_rail())
        {
            return div().size_full().child(panel);
        }
        if self.folder_rail_width() == 0. {
            return div()
                .size_full()
                .min_h_0()
                .min_w_0()
                .flex()
                .flex_col()
                .child(self.folders.rail.clone())
                .child(panel.h_auto().flex_1());
        }
        div()
            .size_full()
            .min_h_0()
            .min_w_0()
            .flex()
            .gap(px(RAIL_GAP))
            .map(|root| {
                if self.active.is_some() {
                    root.child(self.folders.rail.clone())
                } else {
                    root.child(div().w(px(RAIL_WIDTH)).flex_none())
                }
            })
            .child(panel.w_auto().flex_1())
    }
}

impl Active {
    /// Показывает панель для подтверждённого входа или доступного кэша.
    fn shows_rail(&self) -> bool {
        self.folder_online || self.folder_view.is_some()
    }

    /// Выбирает папку, начиная прокрутку её собственного списка сверху.
    fn select_folder(&mut self, folder: Uuid) {
        if self.folder != folder {
            self.folder = folder;
            self.scroll.set_offset(point(px(0.), px(0.)));
            self.menu = None;
            self.folder_error = None;
        }
    }

    /// Использует согласованный каталог Core. До него доступен обычный снимок сайдбара.
    pub(super) fn chats(&self) -> Option<&[Chat]> {
        self.folder_view
            .as_deref()
            .filter(|view| view.folders.is_some())
            .map(|view| view.chats.as_slice())
            .or_else(|| self.catalog.as_ref()?.chats.as_deref())
    }

    /// Проверяет, что чат всё ещё доступен в выбранной папке вне поиска.
    pub(super) fn folder_contains(&self, chat: Uuid) -> bool {
        self.folder.is_nil()
            || self
                .folder_view
                .as_ref()
                .and_then(|view| view.folders.as_ref())
                .is_none_or(|folders| {
                    folders
                        .iter()
                        .find(|folder| folder.id == self.folder)
                        .is_some_and(|folder| {
                            folder.items.iter().any(|item| item.stream_id == chat)
                        })
                })
    }
}

/// Возвращает строки текущей папки и дополнительные результаты поиска.
pub(super) fn folder_rows<'a>(
    snapshot: Option<&'a FolderSnapshot>,
    catalog: &'a [Chat],
    selected: Uuid,
    searching: bool,
) -> Vec<(&'a Chat, Option<&'a FolderItem>, bool)> {
    let Some(view) = snapshot.filter(|view| view.folders.is_some()) else {
        return catalog.iter().map(|chat| (chat, None, false)).collect();
    };
    let mut rows: Vec<_> = view
        .rows(selected)
        .unwrap_or_default()
        .into_iter()
        .map(|row| (row.chat, row.item, false))
        .collect();
    if searching && !selected.is_nil() {
        let seen: HashSet<_> = rows.iter().map(|row| row.0.id).collect();
        rows.extend(
            view.rows(Uuid::nil())
                .unwrap_or_default()
                .into_iter()
                .filter(|row| !seen.contains(&row.chat.id))
                .map(|row| (row.chat, row.item, true)),
        );
    }
    rows
}

/// Переводит ошибку команды строки в короткое состояние сайдбара.
fn folder_error(error: FolderCommandError) -> &'static str {
    match error {
        FolderCommandError::Unknown => "Результат неизвестен. Обновите папки перед повтором",
        FolderCommandError::Refresh => "Действие выполнено. Обновите папки",
        FolderCommandError::Rejected => "Сервер отклонил изменение папки",
        FolderCommandError::Unavailable | FolderCommandError::WorkerUnavailable => {
            "Папка или аккаунт недоступны"
        }
    }
}
