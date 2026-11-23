//! Передаёт локальные настройки рабочему окну: следит за неактивностью и выполняет сочетания.

use std::time::SystemTime;

use gpui::{App, Context, Entity, KeystrokeEvent, Window};
use workspace_core::auth::SessionStatus;

use super::Client;
use crate::gpui_client::{
    contracts::{AccountSession, ChatRoute},
    preferences::{IdleTimeout, Preferences, appearance::ThemeChoice, shortcuts::matching},
    ui::{ThemeMode, TopBarSection},
};

/// Видимый маршрут одной записи истории рабочего окна.
#[derive(Clone, PartialEq, Eq)]
struct Place {
    section: TopBarSection, // Экран, на который пользователь может вернуться.
    conversation: Option<ChatRoute>, // Открытый или сохранённый каталогом разговор.
}

/// История переходов внутри одного действующего входа.
#[derive(Default)]
pub(super) struct Places {
    account: Option<AccountSession>, // Поколение, которому принадлежат адреса.
    locations: Vec<Place>,           // Последние 80 подтверждённых экранов.
    cursor: usize,                   // Текущий экран в списке.
    restoring: bool,                 // Переход по истории нельзя записывать как новый.
}

impl Places {
    /// Добавляет новый адрес, удаляя только ветку «Вперёд» после текущего экрана.
    fn record(&mut self, account: Option<&AccountSession>, item: Place) {
        if self.restoring {
            return;
        }
        if self.account.as_ref() != account {
            self.account = account.cloned();
            self.locations.clear();
            self.cursor = 0;
        }
        if account.is_none() || self.locations.get(self.cursor) == Some(&item) {
            return;
        }
        self.locations.truncate(self.cursor + 1);
        self.locations.push(item);
        if self.locations.len() > 80 {
            self.locations.remove(0);
        }
        self.cursor = self.locations.len() - 1;
    }

    /// Возвращает позиции в направлении перехода. Можно пропустить уже закрытый разговор.
    fn candidates(&self, forward: bool) -> impl Iterator<Item = (usize, &Place)> {
        let locations = self.locations.iter().enumerate();
        let forward_items = locations.clone().skip(self.cursor.saturating_add(1));
        let back_items = locations.take(self.cursor).rev();
        forward_items
            .filter(move |_| forward)
            .chain(back_items.filter(move |_| !forward))
    }
}

/// Время действий пользователя для текущего поколения выбранного входа.
pub(super) struct IdleSession {
    session: AccountSession,   // Вход, который должен завершиться после ожидания.
    timeout: IdleTimeout,      // Порог для этого входа.
    last_activity: SystemTime, // Последнее действие — учитывает время сна Mac.
    pending: bool,             // Команда завершения этого входа уже отправлена.
}

/// Проверяет истечение срока. При переводе часов назад начинает отсчёт с новой отметки.
fn idle_elapsed(
    last_activity: &mut SystemTime,
    now: SystemTime,
    timeout: std::time::Duration,
) -> bool {
    match now.duration_since(*last_activity) {
        Ok(elapsed) => elapsed >= timeout,
        Err(_) => {
            *last_activity = now;
            false
        }
    }
}

impl Client {
    /// Подключает изменяемые сочетания только к главному рабочему окну.
    pub(in crate::gpui_client) fn install_settings_input(client: &Entity<Self>, app: &mut App) {
        let keys = client.downgrade();
        app.intercept_keystrokes(move |event, window, app| {
            if window
                .root::<Self>()
                .flatten()
                .is_some_and(|root| root.downgrade() == keys)
            {
                let _ = keys.update(app, |client, cx| {
                    client.settings_keystroke(event, window, cx)
                });
            }
        })
        .detach();
    }

    /// Запоминает штатный маршрут после того, как оболочка разрешила переход.
    pub(super) fn record_place(&mut self) {
        self.places.record(
            self.shell.selected(),
            Place {
                section: self.section,
                conversation: self
                    .shell
                    .conversation()
                    .map(|address| address.route.clone()),
            },
        );
    }

    /// Возвращается к ближайшему доступному экрану через штатные команды оболочки.
    fn move_place(&mut self, forward: bool, cx: &mut Context<Self>) {
        let target = self.places.candidates(forward).find_map(|(index, place)| {
            let effect = self
                .shell
                .restore_navigation(place.conversation.as_ref())
                .ok()?;
            Some((index, place.section, effect))
        });
        if let Some((index, section, effect)) = target {
            self.places.cursor = index;
            self.places.restoring = true;
            self.apply(effect, cx);
            self.open_section(section, cx);
            self.places.restoring = false;
        }
    }

    /// Циклически передаёт фокус шапке, каталогу, истории, редактору и открытой панели.
    pub(super) fn move_focus_area(
        &self,
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = self.right_panel.read(cx);
        if self.sign_in.is_some() || self.status().is_some() || panel.has_modal() {
            return;
        }
        let mut areas = vec![(self.top_zone.clone(), self.top_bar.read(cx).profile_focus())];
        let chat = self.section == TopBarSection::Chats && self.licenses.is_none();
        if chat
            && self.sidebar_visible
            && let Some(search) = self.sidebar.read(cx).search_focus(cx)
        {
            areas.push((self.sidebar_zone.clone(), search));
        }
        let (history, editor) = if chat {
            self.conversation
                .as_ref()
                .map(|view| view.read(cx).history_and_editor_focus(cx))
                .unwrap_or_else(|| (self.main_zone.clone(), None))
        } else {
            (self.main_zone.clone(), None)
        };
        areas.push((self.main_zone.clone(), history));
        if let Some(editor) = editor {
            areas.push((editor.clone(), editor));
        }
        if let Some(area) = panel.keyboard_area() {
            areas.push(area);
        }
        let position = areas
            .iter()
            .rposition(|(region, _)| region.contains_focused(window, cx));
        let index = if reverse {
            (position.unwrap_or(0) + areas.len() - 1) % areas.len()
        } else {
            position.map_or(0, |index| (index + 1) % areas.len())
        };
        window.focus(&areas[index].1);
        cx.stop_propagation();
    }

    /// Запоминает недавний ввод для активного входа и обновляет период при смене настроек.
    pub(super) fn idle_activity(&mut self, cx: &Context<Self>) {
        self.update_idle(cx, true);
    }

    /// Проверяет тайм-аут в активном окне и даёт Core завершить только действующее поколение.
    pub(in crate::gpui_client) fn check_idle(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.update_idle(cx, false);
        let Some(idle) = self.idle.as_mut() else {
            return;
        };
        if !window.is_window_active()
            || idle.pending
            || !idle.timeout.duration().is_some_and(|timeout| {
                idle_elapsed(&mut idle.last_activity, SystemTime::now(), timeout)
            })
        {
            return;
        }
        let session = idle.session.clone();
        let runtime = self.runtime.clone();
        idle.pending = true;
        log::info!("event=workspace_idle_logout_requested");
        cx.spawn(async move |client, cx| {
            let result = runtime.logout(session.clone()).await;
            log::info!(
                "event=workspace_idle_logout_finished success={}",
                result.is_ok()
            );
            if result.is_err() {
                let _ = client.update(cx, |client, _| {
                    if let Some(idle) = client.idle.as_mut().filter(|idle| idle.session == session)
                    {
                        idle.pending = false;
                    }
                });
            }
        })
        .detach();
    }

    /// Начинает отсчёт заново после смены выбранного входа или значения тайм-аута.
    fn update_idle(&mut self, cx: &Context<Self>, activity: bool) {
        let session = self.shell.selected().filter(|session| {
            self.sign_in.is_none()
                && self.status().is_none()
                && self.shell.accounts().iter().any(|account| {
                    &account.session == *session
                        && matches!(
                            account.status,
                            SessionStatus::Active | SessionStatus::TemporarilyUnavailable
                        )
                })
        });
        let Some(session) = session else {
            self.idle = None;
            return;
        };
        let timeout = Preferences::read(cx).idle_timeout;
        if let Some(idle) = self
            .idle
            .as_mut()
            .filter(|idle| idle.session == *session && idle.timeout == timeout)
        {
            if activity {
                idle.last_activity = SystemTime::now();
            }
        } else {
            self.idle = Some(IdleSession {
                session: session.clone(),
                timeout,
                last_activity: SystemTime::now(),
                pending: false,
            });
        }
    }

    /// Выполняет изменяемые команды окна до обработки стандартных клавиш GPUI.
    fn settings_keystroke(
        &mut self,
        event: &KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.idle_activity(cx);
        if self.sign_in.is_some() || self.status().is_some() || self.shell.selected().is_none() {
            return;
        }
        let Some(shortcut) = matching(&event.keystroke, Preferences::read(cx)) else {
            return;
        };
        if self.right_panel.read(cx).has_modal() {
            if shortcut.id == "interface.help"
                && !event
                    .context_stack
                    .iter()
                    .any(|context| context.contains("WorkspaceHotkeyRecording"))
                && self
                    .right_panel
                    .read(cx)
                    .modal_is::<super::profile::Hotkeys>()
            {
                self.right_panel
                    .update(cx, |panel, cx| panel.close_modal(window, cx));
                cx.stop_propagation();
            }
            return;
        }
        let has_context = |name| {
            event
                .context_stack
                .iter()
                .any(|context| context.contains(name))
        };
        let composer = has_context("WorkspaceComposerEditor");
        let typing = composer || has_context("WorkspaceTextInput");
        if typing
            && !(shortcut.id == "navigation.chatSearch"
                && (composer || has_context("CatalogSearch")))
        {
            return;
        }
        match shortcut.id {
            "navigation.back" => self.move_place(false, cx),
            "navigation.forward" => self.move_place(true, cx),
            "navigation.messenger" => self.open_section(TopBarSection::Chats, cx),
            "navigation.calendar" => self.open_section(TopBarSection::Calendar, cx),
            "navigation.mail" => self.open_section(TopBarSection::Mail, cx),
            "navigation.calls" => self.open_section(TopBarSection::Calls, cx),
            "navigation.chatSearch" if self.section == TopBarSection::Chats => {
                if !self.sidebar_visible {
                    self.search_return = window.focused(cx);
                    self.sidebar_visible = true;
                    // В новом кадре поле снова есть в дереве фокуса.
                    let sidebar = self.sidebar.clone();
                    window.defer(cx, move |window, app| {
                        sidebar.read(app).focus_search(window, app);
                    });
                    cx.notify();
                } else if let Some(focused) = self
                    .sidebar
                    .update(cx, |sidebar, cx| sidebar.cancel_search(window, cx))
                {
                    if focused {
                        window.focus(
                            &self
                                .search_return
                                .take()
                                .unwrap_or_else(|| self.initial_focus.clone()),
                        );
                    }
                    self.search_return = None;
                } else {
                    self.search_return = window.focused(cx);
                    self.sidebar.read(cx).focus_search(window, cx);
                }
            }
            "navigation.folderList"
                if self.section == TopBarSection::Chats && self.sidebar_visible =>
            {
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.open_folder_list(window, cx));
            }
            "interface.theme" => {
                let mode = match Preferences::read(cx).appearance.theme {
                    ThemeChoice::Light => ThemeMode::Light,
                    ThemeChoice::Dark => ThemeMode::Dark,
                    ThemeChoice::System => ThemeMode::from(window.appearance()),
                };
                if Preferences::update(cx, |settings| {
                    settings.appearance.theme = match mode {
                        ThemeMode::Light => ThemeChoice::Dark,
                        ThemeMode::Dark => ThemeChoice::Light,
                    };
                })
                .is_ok()
                {
                    window.refresh();
                }
            }
            "interface.help" => {
                let session = self
                    .shell
                    .selected()
                    .cloned()
                    .expect("для сочетания нужен вход");
                let opener = window
                    .focused(cx)
                    .unwrap_or_else(|| self.initial_focus.clone());
                super::profile::open_hotkey_dialog(&self.right_panel, &session, opener, window, cx);
            }
            "interface.sidebar" if self.section == TopBarSection::Chats => {
                self.sidebar_visible = !self.sidebar_visible;
                if !self.sidebar_visible {
                    self.sidebar.update(cx, |sidebar, cx| sidebar.on_hidden(cx));
                    window.focus(&self.initial_focus);
                }
                cx.notify();
            }
            "interface.infoPanel" if self.section == TopBarSection::Chats => {
                if let Some(conversation) = &self.conversation {
                    conversation.update(cx, |conversation, cx| {
                        conversation.toggle_details(window, cx)
                    });
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    /// Перевод часов назад не разлогинивает до истечения нового срока.
    #[test]
    fn clock_rewind_restarts_idle_timeout() {
        let timeout = Duration::from_secs(60);
        let now = UNIX_EPOCH + timeout;
        let mut last = now;
        assert!(!idle_elapsed(&mut last, UNIX_EPOCH, timeout));
        assert!(!idle_elapsed(
            &mut last,
            now - Duration::from_secs(1),
            timeout
        ));
        assert!(idle_elapsed(&mut last, now, timeout));
    }
}
