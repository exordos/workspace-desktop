//! Показывает одну форму для создания, переименования и архивации из списка чатов.

use gpui::{
    App, Context, Entity, FocusHandle, IntoElement, Render, Subscription, WeakEntity, Window, div,
    prelude::*, px,
};
use uuid::Uuid;
use workspace_core::messenger::{
    details::SidebarTopicAction, sidebar_actions::SidebarStreamAction,
};

use super::{AccountSession, Command, Preferences, RightPanel, RowKey, Sidebar};
use crate::gpui_client::ui::{
    Button, ButtonAppearance, Palette, SURFACE_RADIUS, TextInput, TextInputChanged,
    TextInputSubmitted, TextRole, label,
};

/// Действие формы и данные, которые нужны для повторной отправки.
pub(super) enum DialogMode {
    CreateTopic(Uuid),         // UUID будущей темы, одинаковый при повторе.
    RenameTopic(Uuid, String), // UUID темы и начальное название для поля.
    RenameStream,              // Начальное название берётся из строки чата.
    Archive(bool),             // true для личного чата с другим пользователем.
}

/// Источник диалога, зафиксированный до фоновой операции.
pub(super) struct DialogRequest {
    pub(super) sidebar: WeakEntity<Sidebar>, // Возвращает команду каталогу.
    pub(super) session: AccountSession,      // Вход, в котором открыт диалог.
    pub(super) key: RowKey,                  // UUID строки чата или темы.
    pub(super) mode: DialogMode,             // Определяет поля, запрос и подписи.
    pub(super) title: String,                // Название чата для подписи и подтверждения.
}

/// Форма поверх рабочего окна. Команду и её результат принимает каталог.
pub(super) struct CatalogDialog {
    request: DialogRequest,       // Цель и действие, зафиксированные при открытии.
    host: WeakEntity<RightPanel>, // Панель, которая закрывает форму и возвращает фокус.
    status: Entity<Status>,       // Состояние доступно форме и слою модального окна.
    name: Option<Entity<TextInput>>, // Поле есть только у создания и переименования.
    _changes: Option<Subscription>, // Перерисовывает кнопку при вводе.
    _submitted: Option<Subscription>, // Обрабатывает Enter в поле ввода.
}

/// Общий снимок фоновой команды для каталога и формы.
#[derive(Default)]
pub(super) struct Status {
    pending: bool, // Операция выполняется.
    failed: bool,  // Последняя операция завершилась ошибкой.
}

impl CatalogDialog {
    /// Открывает форму текущего входа через общий модальный слой.
    pub(super) fn open(
        host: &Entity<RightPanel>,
        request: DialogRequest,
        opener: FocusHandle,
        window: &mut Window,
        app: &mut App,
    ) {
        host.update(app, |panel, cx| {
            let session = request.session.clone();
            let host = cx.weak_entity();
            let status = cx.new(|_| Status::default());
            let dialog_status = status.clone();
            panel.open_modal(&session, opener, window, cx, move |_, cx| {
                cx.new(move |cx| Self::new(request, host, dialog_status, cx))
            });
            if panel.modal_is::<Self>() {
                panel.set_modal_dismiss(move |app| !status.read(app).pending);
            }
        });
    }

    /// Подготавливает поле ввода и наблюдение за состоянием команды.
    fn new(
        request: DialogRequest,
        host: WeakEntity<RightPanel>,
        status: Entity<Status>,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial = match &request.mode {
            DialogMode::CreateTopic(_) => Some(""),
            DialogMode::RenameTopic(_, title) => Some(title.as_str()),
            DialogMode::RenameStream => Some(request.title.as_str()),
            DialogMode::Archive(_) => None,
        };
        let language = Preferences::read_for(cx, &request.session.owner).language;
        let placeholder = language.tr(match request.mode {
            DialogMode::RenameStream => "channel.channelName",
            _ => "channel.topicName",
        });
        let name = initial.map(|initial| {
            cx.new(|cx| {
                let mut field = TextInput::new("catalog-dialog-name", placeholder, false, cx);
                field.set_text(initial, cx);
                field
            })
        });
        let changes = name
            .as_ref()
            .map(|name| cx.subscribe(name, |_, _, _: &TextInputChanged, cx| cx.notify()));
        cx.observe(&status, |_, _, cx| cx.notify()).detach();
        Self {
            request,
            host,
            status,
            name,
            _changes: changes,
            _submitted: None,
        }
    }

    /// Сообщает результат только той форме, которая ждёт отправленный запрос.
    pub(super) fn finished(
        host: &Entity<RightPanel>,
        status: &Entity<Status>,
        success: bool,
        window: &mut Window,
        app: &mut App,
    ) {
        if !host.read(app).modal_is::<Self>() || !status.read(app).pending {
            return;
        }
        status.update(app, |status, cx| {
            status.pending = false;
            status.failed = !success;
            cx.notify();
        });
        if success {
            host.update(app, |host, cx| host.close_modal(window, cx));
        }
    }

    /// Проверяет ввод, возвращает команду каталогу и блокирует повторную отправку.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.status.read(cx).pending
            || self
                .name
                .as_ref()
                .is_some_and(|field| field.read(cx).is_composing())
        {
            return;
        }
        let name = self
            .name
            .as_ref()
            .map_or("", |field| field.read(cx).text().trim());
        let command = match &self.request.mode {
            DialogMode::Archive(_) => Command::Stream(SidebarStreamAction::Archive),
            _ if name.is_empty() => return,
            DialogMode::CreateTopic(id) => Command::Stream(SidebarStreamAction::CreateTopic {
                id: *id,
                name: name.into(),
            }),
            DialogMode::RenameTopic(_, initial) if name == initial => {
                self.close(window, cx);
                return;
            }
            DialogMode::RenameTopic(id, _) => {
                Command::Topic(*id, SidebarTopicAction::Rename(name.into()))
            }
            DialogMode::RenameStream if name == self.request.title => {
                self.close(window, cx);
                return;
            }
            DialogMode::RenameStream => Command::Stream(SidebarStreamAction::Rename(name.into())),
        };
        let request = &self.request;
        let started = request
            .sidebar
            .update(cx, |view, cx| {
                view.send_catalog_command(
                    &request.session,
                    request.key,
                    command,
                    Some(self.status.clone()),
                    window,
                    cx,
                )
            })
            .unwrap_or(false);
        self.status.update(cx, |status, cx| {
            status.pending = started;
            status.failed = !started;
            cx.notify();
        });
    }

    /// Закрывает форму и восстанавливает фокус через модальный слой.
    fn close(&self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self
            .host
            .update(cx, |host, cx| host.close_modal(window, cx));
    }
}

impl Render for CatalogDialog {
    /// Показывает название, поле или подтверждение и доступные кнопки.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let language = Preferences::read_for(cx, &self.request.session.owner).language;
        let palette = Palette::for_window(window, cx);
        let (heading, input_label, submit_label) = match &self.request.mode {
            DialogMode::CreateTopic(_) => (
                "desktop.sidebar.actions.newTopic",
                "channel.topicName",
                "common.create",
            ),
            DialogMode::RenameTopic(..) => {
                ("channel.renameTopic", "channel.topicName", "common.save")
            }
            DialogMode::RenameStream => (
                "desktop.sidebar.actions.renameChannel",
                "channel.channelName",
                "common.save",
            ),
            DialogMode::Archive(_) => (
                "desktop.sidebar.actions.archiveTitle",
                "",
                "desktop.sidebar.actions.archive",
            ),
        };
        let status = self.status.read(cx);
        let (pending, failed) = (status.pending, status.failed);
        if let Some(name) = &self.name {
            if self._submitted.is_none() {
                name.read(cx).focus(window);
                self._submitted = Some(cx.subscribe_in(
                    name,
                    window,
                    |view, _, _: &TextInputSubmitted, window, cx| {
                        view.submit(window, cx);
                    },
                ));
            }
            let stable_create = failed && matches!(self.request.mode, DialogMode::CreateTopic(_));
            name.update(cx, |name, cx| {
                name.set_enabled(!pending && !stable_create, cx)
            });
        }
        let mut submit = Button::new("catalog-dialog-submit", language.tr(submit_label));
        submit.appearance = ButtonAppearance::Primary;
        submit.disabled = pending
            || self
                .name
                .as_ref()
                .is_some_and(|name| name.read(cx).text().trim().is_empty());
        let mut cancel = Button::new("catalog-dialog-cancel", language.tr("common.cancel"));
        cancel.disabled = pending;
        let target = cx.weak_entity();
        let dismiss = target.clone();
        let mut body = div()
            .debug_selector(|| "catalog-action-dialog".into())
            .w((window.viewport_size().width - px(24.)).clamp(px(0.), px(420.)))
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .rounded(SURFACE_RADIUS)
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .child(label(palette, TextRole::Heading, language.tr(heading)));
        if let DialogMode::Archive(direct) = self.request.mode {
            let description = language.tr_with(
                if direct {
                    "desktop.sidebar.actions.archiveDirectDescription"
                } else {
                    "desktop.sidebar.actions.archiveDescription"
                },
                &[("name", &self.request.title)],
            );
            body = body.child(label(palette, TextRole::Body, description));
        } else {
            body = body
                .child(label(palette, TextRole::Caption, self.request.title.clone()).truncate())
                .child(label(palette, TextRole::Label, language.tr(input_label)))
                .children(self.name.clone());
        }
        body.when(failed, |body| {
            body.child(
                label(
                    palette,
                    TextRole::Body,
                    language.tr("desktop.sidebar.actions.failed"),
                )
                .text_color(palette.danger),
            )
        })
        .child(
            div()
                .flex()
                .justify_end()
                .gap_2()
                .child(cancel.render(palette, None, move |window, app| {
                    let _ = dismiss.update(app, |view, cx| view.close(window, cx));
                }))
                .child(
                    submit
                        .render(palette, None, move |window, app| {
                            let _ = target.update(app, |view, cx| view.submit(window, cx));
                        })
                        .debug_selector(|| "catalog-dialog-submit".into()),
                ),
        )
    }
}
