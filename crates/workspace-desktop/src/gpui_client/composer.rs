//! Собирает редактор сообщения, состояние проверки темы и типизированные действия отправки.
//! Экран разговора передаёт эти действия в рабочий поток; здесь остаются текст и его версия.

mod draft;
mod editor;
mod replies;
mod source;
mod toolbar;
mod toolbar_preferences;

use gpui::{
    App, Context, Entity, EventEmitter, Focusable, IntoElement, Pixels, Render, SharedString,
    Subscription, Window, div, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::messenger::quote::{Source as QuoteSource, SourceCheckError};

pub(super) use self::draft::DraftBuffer;
pub use self::editor::Editor;
pub(super) use self::replies::{ReplyAction, ReplyQuote};
pub(super) use self::source::{
    DestinationError, QueueWriteOutcome, ResolvedDestination, SendSource,
};
use self::{
    draft::{DraftSend, DraftSession},
    editor::SubmitRequested,
    replies::Replies,
};
use super::{
    contracts::{ConversationAddress, ConversationReply},
    runtime::RuntimeClient,
    ui::{
        Button, ButtonAppearance, CONTROL_BORDER, FieldAppearance, Icon, Palette, SURFACE_RADIUS,
        TextRole, field, label,
    },
};

use toolbar::Toolbar;
use toolbar_preferences::ToolbarPreferences;

/// Загружает выбор панели для штатного приложения до построения редактора.
pub(super) fn install_toolbar_preferences(profile: &std::path::Path, app: &mut App) {
    ToolbarPreferences::install(profile, app);
}

const MAX_MESSAGE_CHARS: usize = 40_000;
const SEND_SIZE: Pixels = px(40.);
const FIELD_SEND_GAP: Pixels = px(12.);
const COMPOSER_INSET: Pixels = px(8.);

/// Точная тема, в которую сервер разрешил подготовить обычный текст.
enum Destination {
    Unchecked,      // Проверка ещё не запущена или доступ к истории изменился.
    Checking(Uuid), // Идентификатор текущей проверки темы.
    Ready {
        topic: Uuid,    // UUID подтверждённой темы отправки.
        check_id: Uuid, // Идентификатор проверки, которая подтвердила тему.
    },
    Unavailable(DestinationError), // Причина, по которой сообщение нельзя поставить в очередь.
}

/// Идентификатор записи, версии черновика и подтверждения цели на момент нажатия.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Attempt {
    pub op_id: Uuid,         // Связывает нажатие, работу с очередью и ответ редактору.
    version: u64,            // Версия текста, которую пользователь отправил.
    replies_version: u64,    // Версия выбранных цитат в снимке.
    destination_check: Uuid, // Подтверждение цели, с которым начата отправка.
}

/// Снимок одного намерения отправить текст.
#[derive(Clone)]
pub(super) struct Submission {
    pub address: ConversationAddress, // Разговор, его владелец, поколение и ревизия перехода.
    pub attempt: Attempt,             // Идентификатор операции и версия исходного черновика.
    pub topic: Uuid,                  // Отдельная тема для отправки, подтверждённая сервером.
    pub markdown: String,             // Текст на момент нажатия; не попадает в журнал.
    pub quote_sources: Vec<QuoteSource>, // Исходники структурированных ответов.
    draft: Option<std::sync::Arc<DraftSend>>, // Локальная версия, которую получит исходящая очередь.
}

/// Намерение, которое редактор передаёт экрану разговора.
pub(super) enum ComposerEvent {
    Send(Submission), // Просьба сохранить подготовленный текст в очереди.
    RetryDestination(ConversationAddress), // Повторная проверка разрешения для того же экрана.
    ReplyAvailabilityChanged, // Истории нужно обновить доступность действий ответа.
    DraftRestored,    // Истории нужно заполнить доступные предпросмотры восстановленных цитат.
}

/// Пояснение к той попытке, которая не попала в очередь.
#[derive(Clone, Copy)]
enum WriteFailure {
    Queue,                   // Запись в хранилище не подтверждена.
    Quote(SourceCheckError), // Сервер не подтвердил источники.
}

impl WriteFailure {
    /// Подсказывает, нужно ли восстановить цитату или повторить попытку.
    fn text(self) -> &'static str {
        match self {
            Self::Queue => "Не удалось сохранить сообщение. Повторите отправку",
            Self::Quote(SourceCheckError::Missing) => {
                "Источник цитаты недоступен. Уберите цитату или выберите сообщение заново"
            }
            Self::Quote(SourceCheckError::Changed) => {
                "Выделенный текст цитаты изменился. Выберите фрагмент заново"
            }
            Self::Quote(SourceCheckError::Unavailable) => {
                "Не удалось проверить цитаты. Повторите отправку"
            }
        }
    }
}

/// Редактор одного открытого разговора. Текст живёт в дочернем поле.
pub(super) struct Composer {
    address: ConversationAddress, // Снимок адреса разговора на момент открытия.
    editor: Entity<Editor>,       // Многострочный ввод, выделение и версия текста.
    replies: Replies,             // Отдельные ответы и цитаты текущего разговора.
    draft_runtime: Option<RuntimeClient>, // Рабочая запись есть у редактора штатного окна.
    draft_session: Option<DraftSession>, // Точная локальная версия открытого поля.
    draft_topic: Option<Uuid>,    // Последняя точная тема, к которой относился показанный ввод.
    destination: Destination,     // Состояние проверки возможности отправки.
    toolbar: Toolbar,             // Кнопка раскрытия, команды и их клавиатурный фокус.
    writing: Option<Attempt>,     // Текущая запись; None позволяет начать следующую.
    write_failure: Option<(u64, u64, WriteFailure)>, // Версии ввода и вкладок при отказе записи.
    _editor_observer: Subscription, // Обновляет доступность кнопки после редактирования.
    _editor_submit: Subscription, // Связывает Enter с тем же действием, что и кнопку отправки.
}

impl Composer {
    /// Переводит фокус на ввод, когда пользователь выбирает «Написать» в сведениях лички.
    pub(super) fn focus_editor(&self, window: &mut Window, app: &App) {
        self.editor.read(app).focus_handle(app).focus(window);
    }

    /// Создаёт редактор привязанного к открытому разговору черновика.
    pub fn new(address: ConversationAddress, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(Editor::for_composer);
        let observer = cx.observe(&editor, |composer, _, cx| {
            composer.persist_draft(cx);
            cx.notify();
        });
        let submit = cx.subscribe(&editor, |composer, _, _: &SubmitRequested, cx| {
            composer.submit(cx);
        });
        Self {
            address,
            editor,
            replies: Replies::default(),
            draft_runtime: None,
            draft_session: None,
            draft_topic: None,
            destination: Destination::Unchecked,
            toolbar: Toolbar::new(cx),
            writing: None,
            write_failure: None,
            _editor_observer: observer,
            _editor_submit: submit,
        }
    }

    /// Передаёт редактору доступ к локальным черновикам штатного профиля.
    pub(super) fn attach_drafts(&mut self, runtime: RuntimeClient) {
        self.draft_runtime = Some(runtime);
    }

    /// Читает содержимое поля для проверок интеграции через окно.
    #[cfg(all(test, feature = "gpui-test"))]
    pub fn draft<'a>(&self, app: &'a App) -> &'a str {
        self.editor.read(app).text()
    }

    /// Возвращает разрешённую тему и признак отказа для текущего текста в сквозном тесте.
    #[cfg(all(test, feature = "gpui-test"))]
    pub fn tested_send_state(&self, app: &App) -> (Option<Uuid>, bool) {
        let topic = match self.destination {
            Destination::Ready { topic, .. } => Some(topic),
            _ => None,
        };
        (topic, self.current_write_failure(app).is_some())
    }

    /// Даёт тесту доступ к системному обработчику ввода рабочего поля.
    #[cfg(all(test, feature = "gpui-test"))]
    pub fn editor(&self) -> Entity<Editor> {
        self.editor.clone()
    }

    /// Убирает разрешение на отправку, пока история проходит повторную проверку.
    pub fn invalidate_destination(&mut self, cx: &mut Context<Self>) {
        let was_available = self.reply_available();
        // При временной проверке уже подтверждённая версия продолжает принимать ввод.
        // Отзыв доступа к прежней области проверяется в Core при каждой записи.
        self.persist_draft(cx);
        self.destination = Destination::Unchecked;
        if was_available {
            cx.emit(ComposerEvent::ReplyAvailabilityChanged);
        }
        cx.notify();
    }

    /// Запрашивает тему после подтверждения доступа к истории. Повторные кадры не запускают запрос.
    pub fn check_destination(&mut self, source: &SendSource, cx: &mut Context<Self>) {
        if !matches!(self.destination, Destination::Unchecked) {
            return;
        }
        let op_id = Uuid::new_v4();
        self.destination = Destination::Checking(op_id);
        let address = self.address.clone();
        let resolve = source.resolve.clone();
        let editor = self.editor.read(cx);
        let stamp = (editor.version(), self.replies.revision);
        log::debug!(
            "gpui_send op_id={op_id} event=destination_requested stream_id={} generation={} revision={}",
            address.route.scope.stream_id,
            address.generation.0,
            address.revision.0,
        );
        cx.spawn(async move |editor, cx| {
            let reply = resolve(address, op_id).await;
            let _ = editor.update(cx, |editor, cx| {
                editor.finish_destination(op_id, stamp, reply, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// Применяет разрешение только для текущего адреса и действующей проверки.
    fn finish_destination(
        &mut self,
        op_id: Uuid,
        stamp: (u64, u64),
        reply: ConversationReply<Result<ResolvedDestination, DestinationError>>,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.destination, Destination::Checking(current) if current == op_id)
            || reply.address != self.address
        {
            log::debug!("gpui_send op_id={op_id} event=destination_ignored");
            return;
        }
        self.destination = match reply.value {
            Ok(resolved) => {
                let topic = resolved.topic;
                if let Some((access, saved)) = resolved.draft {
                    self.draft_topic = Some(topic);
                    self.hydrate_draft(access, saved, stamp, cx);
                }
                if resolved.closed {
                    Destination::Unavailable(DestinationError::TopicClosed)
                } else {
                    Destination::Ready {
                        topic,
                        check_id: op_id,
                    }
                }
            }
            Err(error) => {
                if matches!(
                    error,
                    DestinationError::EncryptionUnavailable | DestinationError::AccessLost
                ) && self.draft_topic.is_some()
                {
                    self.revoke_draft(cx);
                }
                Destination::Unavailable(error)
            }
        };
        if self.reply_available() {
            cx.emit(ComposerEvent::ReplyAvailabilityChanged);
        }
        log::debug!(
            "gpui_send op_id={op_id} event=destination_finished available={}",
            matches!(self.destination, Destination::Ready { .. }),
        );
        cx.notify();
    }

    /// Повторяет только завершившуюся неудачную проверку.
    pub fn retry_destination(&mut self, source: &SendSource, cx: &mut Context<Self>) {
        if matches!(self.destination, Destination::Unavailable(_)) {
            self.invalidate_destination(cx);
            self.check_destination(source, cx);
        }
    }

    /// Проверяет, доступно ли действие для показанного текста.
    fn can_send(&self, app: &App) -> bool {
        let editor = self.editor.read(app);
        matches!(self.destination, Destination::Ready { .. })
            && self.writing.is_none()
            && !editor.composing()
            && self.replies.has_answer(editor.text())
            && !over_limit(&self.replies.markdown(editor.text()))
    }

    /// Блокирует повторное нажатие и передаёт экрану прежний текст и адрес.
    fn submit(&mut self, cx: &mut Context<Self>) {
        let Destination::Ready { topic, check_id } = self.destination else {
            return;
        };
        if !self.can_send(cx) {
            return;
        }
        let editor = self.editor.read(cx);
        let attempt = Attempt {
            op_id: Uuid::new_v4(),
            version: editor.version(),
            replies_version: self.replies.revision,
            destination_check: check_id,
        };
        let draft = self.prepare_draft_send(attempt.op_id, cx);
        let editor = self.editor.read(cx);
        let submission = Submission {
            draft,
            address: self.address.clone(),
            attempt,
            topic,
            markdown: self.replies.markdown(editor.text()).into_owned(),
            quote_sources: self.replies.sources(),
        };
        self.writing = Some(attempt);
        self.editor
            .update(cx, |editor, cx| editor.set_text_menu_busy(true, cx));
        self.write_failure = None;
        log::debug!(
            "gpui_send op_id={} event=submit stream_id={} topic_id={topic} generation={} revision={}",
            attempt.op_id,
            self.address.route.scope.stream_id,
            self.address.generation.0,
            self.address.revision.0,
        );
        cx.emit(ComposerEvent::Send(submission));
        cx.notify();
    }

    /// Проверяет, что отложенное действие редактора ещё может поставить текст в очередь.
    pub fn accepts(&self, submission: &Submission) -> bool {
        self.address == submission.address
            && self.writing == Some(submission.attempt)
            && matches!(
                self.destination,
                Destination::Ready { topic, check_id }
                    if topic == submission.topic && check_id == submission.attempt.destination_check
            )
    }

    /// Завершает запись, сохраняя черновик и причину, если проверка цели не прошла.
    pub fn finish(&mut self, attempt: Attempt, outcome: QueueWriteOutcome, cx: &mut Context<Self>) {
        if self.writing != Some(attempt) {
            return;
        }
        self.writing = None;
        self.editor
            .update(cx, |editor, cx| editor.set_text_menu_busy(false, cx));
        let editor = self.editor.read(cx);
        let same_version = editor.version() == attempt.version
            && self.replies.revision == attempt.replies_version
            && !editor.composing();
        let saved = matches!(outcome, QueueWriteOutcome::Saved(_));
        match outcome {
            QueueWriteOutcome::Saved(_) if same_version => {
                self.replies.clear();
                self.editor.update(cx, Editor::clear);
            }
            QueueWriteOutcome::WriteFailed if same_version => {
                self.write_failure = Some((
                    attempt.version,
                    attempt.replies_version,
                    WriteFailure::Queue,
                ));
            }
            QueueWriteOutcome::QuoteUnavailable(error) if same_version => {
                self.write_failure = Some((
                    attempt.version,
                    attempt.replies_version,
                    WriteFailure::Quote(error),
                ));
            }
            QueueWriteOutcome::DestinationUnavailable(error)
                if matches!(
                    self.destination,
                    Destination::Ready { check_id, .. } if check_id == attempt.destination_check
                ) =>
            {
                self.destination = Destination::Unavailable(error);
                cx.emit(ComposerEvent::ReplyAvailabilityChanged);
            }
            _ => {}
        }
        log::debug!(
            "gpui_send op_id={} event=draft_finished saved={saved} cleared={}",
            attempt.op_id,
            saved && same_version,
        );
        cx.notify();
    }

    /// Показывает ошибку только пока текст и сессия ответа соответствуют отклонённой попытке.
    fn current_write_failure(&self, app: &App) -> Option<WriteFailure> {
        let (version, replies_version, failure) = self.write_failure?;
        (version == self.editor.read(app).version() && replies_version == self.replies.revision)
            .then_some(failure)
    }
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Render for Composer {
    /// Показывает поле, доступность отправки и причину, если сохранить текст не получилось.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let editor = self.editor.read(cx);
        let expanded = ToolbarPreferences::expanded(cx);
        let can_format = editor.can_format();
        let too_long = over_limit(&self.replies.markdown(editor.text()));
        let write_failure = self.current_write_failure(cx);
        let explanation = match &self.destination {
            Destination::Unchecked | Destination::Checking(_) => {
                Some(("Проверяем возможность отправки…", false))
            }
            Destination::Unavailable(error) => Some((error.text(), false)),
            Destination::Ready { .. } if too_long => Some(("Текст длиннее 40 000 символов", true)),
            Destination::Ready { .. } => write_failure.map(|failure| (failure.text(), true)),
        };
        let appearance = if too_long || write_failure.is_some() {
            FieldAppearance::Invalid
        } else if self.editor.focus_handle(cx).is_focused(window) {
            FieldAppearance::Focused
        } else {
            FieldAppearance::Normal
        };
        let saving = self.writing.is_some();
        let mut send = Button::new("conversation-send", "");
        send.appearance = ButtonAppearance::Primary;
        send.disabled = !self.can_send(cx);
        let send_icon = if saving {
            Icon::Pending.element(px(20.), palette.text_secondary)
        } else {
            let color = if send.disabled {
                palette.disabled
            } else {
                palette.on_accent
            };
            Icon::Send.element(px(24.), color)
        };
        let target = cx.entity().downgrade();
        let retry = target.clone();
        let toolbar = expanded.then(|| self.toolbar.row(palette, can_format, window, cx));
        // Сетка сначала задаёт общую ширину поля и пояснения. От неё зависит высота переносов.
        div()
            .grid()
            .grid_cols(1)
            .flex_none()
            .min_w_0()
            .gap(px(4.))
            .p(COMPOSER_INSET)
            .rounded(SURFACE_RADIUS)
            .bg(palette.surface)
            .debug_selector(|| "conversation-composer".into())
            .children(self.render_replies(palette, window, cx))
            .child(
                div()
                    .flex()
                    .min_w_0()
                    .items_end()
                    .gap(FIELD_SEND_GAP)
                    .child(
                        field(
                            palette,
                            appearance,
                            div()
                                .flex()
                                .items_end()
                                .w_full()
                                .min_w_0()
                                .gap(px(8.))
                                // Кнопка слева от поля, но Tab из истории сначала возвращает в текст.
                                .flex_row_reverse()
                                .child(div().min_w_0().flex_1().child(self.editor.clone()))
                                .children((!expanded).then(|| {
                                    self.toolbar
                                        .toggle(false, palette, cx)
                                        .mb(px(4.) - CONTROL_BORDER)
                                })),
                        )
                        // Рамка входит в отступ. Переключатель начинается на одной оси с панелью.
                        .px(if expanded { px(20.) } else { COMPOSER_INSET } - CONTROL_BORDER)
                        .min_h(SEND_SIZE)
                        .flex_1()
                        .debug_selector(|| "conversation-composer-field".into())
                        .rounded(SURFACE_RADIUS),
                    )
                    .child(
                        send.render(palette, None, move |window, app| {
                            let _ = target.update(app, |composer, cx| {
                                window.focus(&composer.editor.focus_handle(cx));
                                composer.submit(cx);
                            });
                        })
                        .size(SEND_SIZE)
                        .flex_none()
                        .p(px(0.))
                        .rounded(SURFACE_RADIUS)
                        .when(saving, |button| button.bg(palette.accent_soft))
                        .child(send_icon)
                        .tooltip(move |_, app| {
                            app.new(|_| SendTooltip {
                                palette,
                                text: if saving {
                                    "Сохраняем сообщение…"
                                } else {
                                    "Отправить (Enter)"
                                }
                                .into(),
                            })
                            .into()
                        })
                        .debug_selector(|| "conversation-send".into()),
                    ),
            )
            // Панель прокручивается в пределах поля, оставляя справа место под отправку.
            .children(toolbar.map(|row| div().min_w_0().pr(SEND_SIZE + FIELD_SEND_GAP).child(row)))
            .when_some(explanation, |view, (text, error)| {
                view.child(
                    label(palette, TextRole::Caption, text)
                        .text_color(if error {
                            palette.danger
                        } else {
                            palette.text_secondary
                        })
                        .debug_selector(|| "conversation-send-explanation".into()),
                )
            })
            .when(
                matches!(self.destination, Destination::Unavailable(_)),
                |view| {
                    view.child(
                        div().flex().justify_end().child(
                            Button::new("conversation-send-retry", "Проверить отправку")
                                .render(palette, None, move |_, app| {
                                    let _ = retry.update(app, |view, cx| {
                                        cx.emit(ComposerEvent::RetryDestination(
                                            view.address.clone(),
                                        ));
                                    });
                                })
                                .debug_selector(|| "conversation-send-retry".into()),
                        ),
                    )
                },
            )
    }
}

/// Подпись компактной кнопки отправки, видимая при наведении.
struct SendTooltip {
    palette: Palette,   // Цвета текущей темы окна.
    text: SharedString, // Действие с клавишей или состояние текущей записи.
}

impl Render for SendTooltip {
    /// Рисует подсказку теми же цветами и размером текста, что и редактор.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        label(self.palette, TextRole::Caption, self.text.clone())
            .px_2()
            .py_1()
            .rounded(SURFACE_RADIUS)
            .bg(self.palette.surface)
            .border_1()
            .border_color(self.palette.border)
            .shadow_md()
    }
}

/// Проверяет лимит попытки сразу в окне. Core независимо проверяет его при записи.
fn over_limit(text: &str) -> bool {
    text.chars().take(MAX_MESSAGE_CHARS + 1).count() > MAX_MESSAGE_CHARS
}
