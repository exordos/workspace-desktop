//! Собирает контекстное меню подтверждённого сообщения и передаёт действия их владельцам.

use std::rc::Rc;

use gpui::{
    ClipboardItem, Context, Deferred, Div, FocusHandle, KeyDownEvent, Pixels, Point, Stateful,
    Window, div, point, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        encryption::fragments::fragment_display_spans, forward::forward_spans,
        references::message_permalink,
    },
    storage::{HistoryMessage, MessageBody},
};

use super::{
    Conversation, State,
    attachments::AttachmentKey,
    encryption::EncryptedPresentation,
    key_exchange::{KeyAction, KeyActionState},
};
use crate::gpui_client::{
    composer::ReplyAction,
    preferences::{Language, Preferences},
    ui::{
        Icon, Palette, TextRole, activate, label,
        popup::{MenuInput, menu_layer, menu_navigation, menu_surface},
    },
};

/// Элемент сообщения, на котором открыли меню.
pub(super) enum MenuHit {
    Bubble,       // Обычная подложка или текст.
    Link(String), // Проверенный веб-адрес под указателем.
    Image {
        key: AttachmentKey, // Точное вложение текущей строки.
        original: bool,     // Для HEIC показывается отдельное скачивание оригинала.
    },
}

/// Контекст запоминает один подтверждённый снимок и выделение на момент открытия.
struct Target {
    row: HistoryMessage,       // Строка должна ещё совпадать с текущей версией.
    anchor: Point<Pixels>,     // Положение меню в окне.
    hit: MenuHit,              // Ссылка, изображение или сам бабл.
    selection: Option<String>, // Выделение из открытого Markdown этой версии строки.
    invalidated: bool,         // Событие уже отозвало цель до получения нового окна.
}

/// Фокус и выбранное действие общего меню истории.
pub(super) struct MessageMenu {
    focus: FocusHandle,       // Фокус переноса клавиатурных команд в меню.
    target: Option<Target>,   // None, пока меню закрыто.
    selected: Option<Action>, // Действие, подсвеченное с клавиатуры.
}

impl MessageMenu {
    /// Готовит одно закрытое меню для всех строк текущего разговора.
    pub(super) fn new(focus: FocusHandle) -> Self {
        Self {
            focus,
            target: None,
            selected: None,
        }
    }

    /// Отбрасывает сохранённый снимок при сбросе истории.
    pub(super) fn clear(&mut self) {
        self.target = None;
        self.selected = None;
    }

    /// Запрещает выполнение действия сразу по событию, фокус вернёт отрисовка.
    pub(super) fn invalidate(&mut self, changed: Option<Uuid>) {
        if let Some(target) = &mut self.target
            && changed.is_none_or(|id| id == target.row.id)
        {
            target.invalidated = true;
        }
    }

    /// Проверяет, перехватывает ли меню команды истории.
    pub(super) fn is_open(&self) -> bool {
        self.target.is_some()
    }
}

/// Действия меню в порядке исходного интерфейса.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Action {
    RequestKey, // Запросить ключ текущего сообщения.
    Reply,      // Заменить текущий набор цитат.
    AddReply,   // Добавить ещё одну цитату.
    Forward,    // Открыть выбор получателя пересылки.
    CopyImage,  // Поместить изображение в буфер обмена.
    Download,   // Скачать оригинал HEIC.
    CopyLink,   // Скопировать веб-адрес под указателем.
    Copy,       // Скопировать выделение или тело сообщения.
    Permalink,  // Скопировать ссылку на сообщение.
    Select,     // Войти в режим выбора нескольких сообщений.
    Edit,       // Открыть правку собственного сообщения.
    Delete,     // Открыть подтверждение удаления.
}

impl Action {
    /// Возвращает стабильный ID строки, ключ перевода и значок.
    fn label(self) -> (&'static str, &'static str, Icon) {
        match self {
            Self::RequestKey => ("history-request-key", "e2ee.requestAccess", Icon::ChatLock),
            Self::Reply => ("history-reply-replace", "message.reply", Icon::Reply),
            Self::AddReply => ("history-reply-add", "message.addReply", Icon::Reply),
            Self::Forward => ("history-forward-message", "message.forward", Icon::Forward),
            Self::CopyImage => ("history-copy-image", "message.copyImage", Icon::Image),
            Self::Download => (
                "history-download-original",
                "message.downloadOriginal",
                Icon::Download,
            ),
            Self::CopyLink => ("history-copy-link", "message.copyLink", Icon::Links),
            Self::Copy => ("history-copy-message", "message.copy", Icon::Copy),
            Self::Permalink => (
                "history-copy-message-link",
                "message.copyMessageLink",
                Icon::Links,
            ),
            Self::Select => ("history-select-message", "message.select", Icon::Check),
            Self::Edit => ("history-edit-message", "message.edit", Icon::Stylus),
            Self::Delete => ("history-delete-message", "message.delete", Icon::Delete),
        }
    }
}

/// Строка меню с известной доступностью действия.
struct Entry {
    action: Action, // Намерение, которое передаём экрану при активации.
    enabled: bool,  // Недоступное действие только рисуется и пропускается при обходе.
}

/// Раскладка меню из групп действий и отдельной полосы реакций.
#[derive(Default)]
struct Entries {
    reactions: bool, // Для обычных и раскрытых сообщений показываем полосу реакций.
    groups: Vec<Vec<Entry>>, // Разделители рисуем только между непустыми группами.
}

impl Entries {
    /// Идёт по доступным пунктам в визуальном порядке.
    fn enabled(&self) -> impl DoubleEndedIterator<Item = Action> + '_ {
        self.groups
            .iter()
            .flatten()
            .filter(|entry| entry.enabled)
            .map(|entry| entry.action)
    }

    /// Сохраняет непустую группу в заданном порядке.
    fn group(&mut self, entries: impl IntoIterator<Item = (Action, bool)>) {
        let group: Vec<_> = entries
            .into_iter()
            .map(|(action, enabled)| Entry { action, enabled })
            .collect();
        if !group.is_empty() {
            self.groups.push(group);
        }
    }
}

impl Conversation {
    /// Открывает меню текущей серверной строки независимо от состояния поля ввода.
    pub(super) fn open_message_menu(
        &mut self,
        id: Uuid,
        anchor: Point<Pixels>,
        hit: MenuHit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.menu_row(id).cloned() else {
            return;
        };
        let selection = row
            .body
            .markdown()
            .and_then(|text| self.viewport.selection().borrow().fragment(id, text));
        let target = Target {
            row,
            anchor: anchor + point(px(6.), px(0.)),
            hit,
            selection,
            invalidated: false,
        };
        self.message_menu.selected = self.message_entries(&target, cx).enabled().next();
        self.message_menu.target = Some(target);
        window.focus(&self.message_menu.focus);
        cx.notify();
    }

    /// Открывает меню у строки с фокусом по клавише меню или Shift+F10.
    pub(super) fn message_menu_hotkey(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke;
        let context_menu =
            matches!(key.key.as_str(), "contextmenu" | "menu") && !key.modifiers.modified();
        let shifted_f10 = key.key == "f10" && key.modifiers == gpui::Modifiers::shift();
        if event.is_held || !(context_menu || shifted_f10) {
            return;
        }
        let Some(id) = self
            .viewport
            .selection()
            .borrow()
            .focused(&self.history_focus, window)
        else {
            return;
        };
        let Some(anchor) = self.viewport.message_menu_anchor(id) else {
            return;
        };
        self.open_message_menu(id, anchor, MenuHit::Bubble, window, cx);
        cx.stop_propagation();
    }

    /// Ограничивает меню проверенной текущей историей и исключает служебные строки.
    fn menu_row(&self, id: Uuid) -> Option<&HistoryMessage> {
        let State::Ready(history) = &self.state else {
            return None;
        };
        history.messages.iter().find(|row| {
            row.id == id && !id.is_nil() && !matches!(row.body, MessageBody::KeyService { .. })
        })
    }

    /// Убирает меню и отдаёт фокус истории или экрану после потери доступа.
    fn close_message_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.message_menu.is_open() {
            self.message_menu.clear();
            let focus = if self.has_access() {
                &self.history_focus
            } else {
                &self.screen_focus
            };
            window.focus(focus);
            cx.notify();
        }
    }

    /// Возвращает текущий снимок, если строка и её доступность не изменились.
    fn menu_target(&self) -> Option<&Target> {
        let target = self.message_menu.target.as_ref()?;
        let current = self.menu_row(target.row.id)?;
        (!target.invalidated
            && current.stream_id == target.row.stream_id
            && current.topic_id == target.row.topic_id
            && current.metadata == target.row.metadata
            && current.body == target.row.body)
            .then_some(target)
    }

    /// Вычисляет доступность из открытой истории, редактора и текущего состояния ключей.
    fn message_entries(&self, target: &Target, cx: &Context<Self>) -> Entries {
        let row = &target.row;
        let id = row.id;
        let composer = self.sending.as_ref().map(|sending| sending.view.read(cx));
        let own = row.metadata.author_id.is_some()
            && row.metadata.author_id == self.address.route.owner.user_id.parse().ok();
        let permalink = (
            Action::Permalink,
            message_permalink(&self.address.route.owner, id).is_some(),
        );
        let mut entries = Entries::default();
        let encryption = self.encryption.as_ref();
        let protected = matches!(row.body, MessageBody::Encrypted { .. });
        let decrypted = encryption.is_some_and(|encryption| {
            matches!(
                encryption.presentation(id),
                Some(EncryptedPresentation::Ready(access)) if access.with_current(|| ()).is_some()
            )
        });
        if protected && !decrypted {
            let can_request = encryption.is_some_and(|encryption| {
                self.avatar_runtime.is_some()
                    && matches!(
                        encryption.presentation(id),
                        Some(EncryptedPresentation::Locked | EncryptedPresentation::Failed)
                    )
                    && !encryption.message_change_pending(id)
                    && !encryption.key_actions.get(&id).is_some_and(|record| {
                        matches!(
                            record.state,
                            KeyActionState::Running(_)
                                | KeyActionState::Waiting(_)
                                | KeyActionState::ReplySent(_)
                        )
                    })
            });
            entries.group([(Action::RequestKey, can_request), permalink]);
            return entries;
        }

        entries.reactions = !self.is_archived();
        let reply_available = composer.is_some_and(|composer| composer.reply_action_enabled(cx));
        let protected_reply = composer.is_some_and(|composer| composer.can_edit_protected());
        let can_add = composer.is_some_and(|composer| composer.has_reply_tabs());
        let protected_actions = self.heading.encrypted != Some(false);
        if protected {
            entries.group(
                protected_actions
                    .then_some((Action::Reply, protected_reply))
                    .into_iter()
                    .chain((protected_actions && can_add).then_some((Action::AddReply, false))),
            );
        } else {
            let can_reply = reply_available
                && row
                    .body
                    .markdown()
                    .is_some_and(|text| !text.trim().is_empty());
            entries.group(
                [(Action::Reply, can_reply)]
                    .into_iter()
                    .chain(can_add.then_some((Action::AddReply, can_reply)))
                    .chain([(Action::Forward, self.can_forward_message(id))]),
            );
        }

        let image = match target.hit {
            MenuHit::Image { key, original } => Some((
                Action::CopyImage,
                !original && self.can_copy_attachment(key),
            )),
            _ => None,
        };
        let download = match target.hit {
            MenuHit::Image {
                key,
                original: true,
            } => Some((Action::Download, self.can_save_history_attachment(key))),
            _ => None,
        };
        let link = matches!(target.hit, MenuHit::Link(_)).then_some((Action::CopyLink, true));
        entries.group(
            image
                .into_iter()
                .chain(download)
                .chain(link)
                .chain([(Action::Copy, true), permalink])
                .chain((!protected).then_some((Action::Select, false))),
        );

        if own && !self.is_archived() {
            let edit = if protected {
                protected_actions.then_some((Action::Edit, protected_reply))
            } else {
                row.body
                    .markdown()
                    .filter(|text| forward_spans(text).is_empty())
                    .map(|text| {
                        let enabled = !text.trim().is_empty()
                            && composer.and_then(|composer| composer.fragment_edit_topic())
                                == Some(row.topic_id)
                            && self.fragment_runtime(cx).is_some();
                        (Action::Edit, enabled)
                    })
            };
            entries.group(
                edit.into_iter()
                    .chain([(Action::Delete, self.delete_message_enabled(id))]),
            );
        }
        entries
    }

    /// Вызывает только ещё доступный пункт для совпадающего снимка сообщения.
    fn run_message_action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.menu_target() else {
            self.close_message_menu(window, cx);
            return;
        };
        if !self
            .message_entries(target, cx)
            .enabled()
            .any(|enabled| enabled == action)
        {
            return;
        }
        let Target {
            row,
            hit,
            selection,
            ..
        } = self
            .message_menu
            .target
            .take()
            .expect("цель меню проверена");
        self.message_menu.clear();
        window.focus(&self.history_focus);
        cx.notify();
        let id = row.id;
        if matches!(row.body, MessageBody::Encrypted { .. }) {
            let access =
                self.encryption
                    .as_ref()
                    .and_then(|encryption| match encryption.presentation(id) {
                        Some(EncryptedPresentation::Ready(access)) => Some(access.clone()),
                        _ => None,
                    });
            match action {
                Action::Copy => {
                    if let Some(access) = access {
                        self.copy_protected_body(id, &access, cx);
                    }
                    return;
                }
                Action::Reply => {
                    if let Some(access) = access {
                        self.reply_to_protected_body(id, access, window, cx);
                    }
                    return;
                }
                Action::Edit => {
                    self.open_protected_edit(id, Some(window), cx);
                    return;
                }
                _ => {}
            }
        }
        match action {
            Action::RequestKey => {
                let mode = if self.encryption.as_ref().is_some_and(|encryption| {
                    matches!(
                        encryption.presentation(id),
                        Some(EncryptedPresentation::Failed)
                    )
                }) {
                    KeyAction::Recover
                } else {
                    KeyAction::Request
                };
                self.key_action(id, mode, cx);
            }
            Action::Reply | Action::AddReply => {
                let mode = if action == Action::Reply {
                    ReplyAction::Replace
                } else {
                    ReplyAction::Add
                };
                self.reply_to_selection(id, mode, selection, window, cx);
            }
            Action::Forward => self.open_message_forward(id, selection, window, cx),
            Action::CopyLink => {
                if let MenuHit::Link(url) = hit {
                    cx.write_to_clipboard(ClipboardItem::new_string(url));
                }
            }
            Action::Copy => {
                if let MessageBody::Markdown(text) = row.body {
                    let selection = selection.filter(|_| fragment_display_spans(&text).is_empty());
                    let language = Preferences::read_for(cx, &self.address.route.owner).language;
                    let copied = selection.unwrap_or_else(|| {
                        super::inline_references::localize_copied_times(text, language)
                    });
                    cx.write_to_clipboard(ClipboardItem::new_string(copied));
                }
            }
            Action::Permalink => {
                if let Some(link) = message_permalink(&self.address.route.owner, id) {
                    cx.write_to_clipboard(ClipboardItem::new_string(link));
                }
            }
            Action::Edit => self.edit_fragments(id, Some(window), cx),
            Action::Delete => self.open_message_delete(id, window, cx),
            Action::CopyImage => {
                if let MenuHit::Image { key, .. } = hit {
                    self.copy_attachment(key, cx);
                }
            }
            Action::Download => {
                if let MenuHit::Image {
                    key,
                    original: true,
                } = hit
                {
                    self.save_history_attachment(key, window, cx);
                }
            }
            Action::Select => {}
        }
    }

    /// Двигает клавиатурное выделение только по доступным строкам.
    fn message_menu_input(
        &mut self,
        input: MenuInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(input, MenuInput::Close) {
            self.close_message_menu(window, cx);
            return;
        }
        let Some(target) = self.menu_target() else {
            self.close_message_menu(window, cx);
            return;
        };
        let entries = self.message_entries(target, cx);
        let mut items = entries.enabled();
        let current = self.message_menu.selected;
        self.message_menu.selected = match input {
            MenuInput::First => items.next(),
            MenuInput::Last => items.next_back(),
            MenuInput::Next => items
                .find(|&item| Some(item) == current)
                .and_then(|_| items.next())
                .or_else(|| entries.enabled().next()),
            MenuInput::Previous => items
                .rfind(|&item| Some(item) == current)
                .and_then(|_| items.next_back())
                .or_else(|| entries.enabled().next_back()),
            MenuInput::Select => {
                if let Some(action) = current {
                    self.run_message_action(action, window, cx);
                }
                return;
            }
            MenuInput::Close => return,
        };
        cx.notify();
    }

    /// Рисует подложку, строку реакций и непустые группы в одном всплывающем слое.
    pub(super) fn render_message_menu(
        &mut self,
        window: &mut Window,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Option<[Deferred; 2]> {
        self.message_menu.target.as_ref()?;
        let Some(target) = self.menu_target() else {
            self.close_message_menu(window, cx);
            return None;
        };
        let anchor = target.anchor;
        let id = target.row.id;
        let entries = self.message_entries(target, cx);
        let language = Preferences::read_for(cx, &self.address.route.owner).language;
        let viewport = window.viewport_size();
        let width = if entries.reactions {
            px(240.)
        } else {
            px(200.)
        };
        let surface = menu_surface(
            "history-message-menu",
            palette,
            viewport,
            width,
            viewport.height,
        )
        .rounded(px(8.))
        .px_0()
        .py_1()
        .overflow_y_scroll()
        .on_mouse_down_out(cx.listener(|view, _, window, cx| view.close_message_menu(window, cx)))
        .when(entries.reactions, |surface| {
            surface.child(self.message_reactions(id, palette, language, cx))
        })
        .children(
            entries
                .groups
                .into_iter()
                .enumerate()
                .flat_map(|(index, group)| {
                    let separator = (index > 0).then(|| separator(palette).into_any_element());
                    separator.into_iter().chain(group.into_iter().map(|entry| {
                        self.message_menu_row(entry, palette, language, cx)
                            .into_any_element()
                    }))
                }),
        );
        Some(menu_layer(
            viewport,
            anchor,
            menu_navigation(
                surface,
                &self.message_menu.focus,
                cx,
                Self::message_menu_input,
            ),
        ))
    }

    /// Добавляет взаимодействия только к доступному действию.
    fn message_menu_row(
        &self,
        entry: Entry,
        palette: Palette,
        language: Language,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let (id, caption, icon) = entry.action.label();
        let foreground = if entry.enabled {
            palette.text
        } else {
            palette.text_muted
        };
        let row = div()
            .id(id)
            .debug_selector(move || id.into())
            .flex()
            .items_center()
            .px_3()
            .py_2()
            .gap_2()
            .child(icon.element(px(20.), foreground))
            .child(label(palette, TextRole::Body, language.tr(caption)).text_color(foreground));
        if !entry.enabled {
            return row;
        }
        let target = cx.entity().downgrade();
        let action = entry.action;
        activate(
            row.when(self.message_menu.selected == Some(action), |row| {
                row.bg(palette.card_hover)
            })
            .hover(|style| style.bg(palette.card_hover)),
            None,
            palette,
            move |window, cx| {
                let _ = target.update(cx, |view, cx| view.run_message_action(action, window, cx));
            },
        )
    }

    /// Использует те же рейтинг, разрешения и команды, что и реакции у строки истории.
    fn message_reactions(
        &self,
        id: Uuid,
        palette: Palette,
        language: Language,
        cx: &Context<Self>,
    ) -> Div {
        let controls = self.reaction_controls(language, cx).map(Rc::new);
        let quick = controls
            .as_ref()
            .map_or_else(Vec::new, |controls| controls.quick(id));
        let mut row = div()
            .id("history-quick-reaction-scroll")
            .flex()
            .flex_1()
            .min_w_0()
            .overflow_x_scroll()
            .gap(px(2.));
        let center = |element: Div| {
            element
                .size(px(28.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
        };
        if let Some(controls) = controls.filter(|_| !quick.is_empty()) {
            for reaction in quick {
                let emoji = reaction.emoji;
                let target = cx.entity().downgrade();
                let controls = Rc::clone(&controls);
                let name = reaction.label;
                let button = center(div())
                    .id(emoji.clone())
                    .rounded(px(4.))
                    .child(label(palette, TextRole::Body, reaction.glyph))
                    .when(reaction.own, |button| button.bg(palette.card_selected))
                    .when(reaction.pending, |button| button.opacity(0.45))
                    .tooltip(move |_, cx| {
                        crate::gpui_client::ui::popup::tooltip(palette, name.clone(), cx)
                    });
                row = if reaction.pending {
                    row.child(button)
                } else {
                    row.child(activate(
                        button.hover(|button| button.bg(palette.card_hover)),
                        None,
                        palette,
                        move |window, cx| {
                            let current = target
                                .update(cx, |view, cx| {
                                    let current = view
                                        .menu_target()
                                        .is_some_and(|target| target.row.id == id);
                                    if current {
                                        view.close_message_menu(window, cx);
                                    }
                                    current
                                })
                                .unwrap_or(false);
                            if current {
                                controls.toggle(id, &emoji, cx);
                            }
                        },
                    ))
                };
            }
        } else {
            row = row.children(["❤️", "👍", "😂", "😮", "😢", "👏"].map(|glyph| {
                center(div())
                    .child(label(palette, TextRole::Body, glyph))
                    .opacity(0.45)
            }));
        }
        div()
            .debug_selector(|| "history-quick-reactions".into())
            .flex()
            .items_center()
            .px_3()
            .py_2()
            .gap(px(2.))
            .border_b_1()
            .border_color(palette.border)
            .child(row)
            .child(
                center(div())
                    .debug_selector(|| "history-more-reactions".into())
                    .child(Icon::Plus.element(px(14.), palette.disabled)),
            )
    }
}

/// Разделяет группы пунктов по образцу меню Electron.
fn separator(palette: Palette) -> Div {
    div().mx_2().my_1().h(px(1.)).bg(palette.border)
}
