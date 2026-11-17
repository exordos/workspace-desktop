//! Собирает редактор сообщения, состояние проверки темы и типизированные действия отправки.
//! Экран разговора передаёт эти действия в рабочий поток; здесь остаются текст и его версия.

mod editor;
mod source;

use gpui::{
    App, Context, Entity, EventEmitter, Focusable, IntoElement, Render, SharedString, Subscription,
    Window, div, prelude::*, px,
};
use uuid::Uuid;

pub use self::editor::Editor;
use self::editor::SubmitRequested;
pub(super) use self::source::{DestinationError, SendSource};
use super::{
    contracts::{ConversationAddress, ConversationReply},
    ui::{
        Button, ButtonAppearance, FieldAppearance, Icon, Palette, TextRole, ThemeMode, field, label,
    },
};

const MAX_MESSAGE_CHARS: usize = 40_000;

/// Точная тема, в которую сервер разрешил подготовить обычный текст.
enum Destination {
    Unchecked,                     // Проверка ещё не запущена или доступ к истории изменился.
    Checking(Uuid),                // Идентификатор текущей проверки темы.
    Ready(Uuid),                   // UUID подтверждённой темы отправки.
    Unavailable(DestinationError), // Причина, по которой сообщение нельзя поставить в очередь.
}

/// Идентификатор записи и версии черновика на момент нажатия.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Attempt {
    pub op_id: Uuid, // Связывает нажатие, работу с очередью и ответ редактору.
    version: u64,    // Версия текста, которую пользователь отправил.
}

/// Снимок одного намерения отправить текст.
#[derive(Clone)]
pub(super) struct Submission {
    pub address: ConversationAddress, // Разговор, его владелец, поколение и ревизия перехода.
    pub attempt: Attempt,             // Идентификатор операции и версия исходного черновика.
    pub topic: Uuid,                  // Отдельная тема для отправки, подтверждённая сервером.
    pub markdown: String,             // Текст на момент нажатия; не попадает в журнал.
}

/// Намерение, которое редактор передаёт экрану разговора.
pub(super) enum ComposerEvent {
    Send(Submission), // Просьба сохранить подготовленный текст в очереди.
    RetryDestination(ConversationAddress), // Повторная проверка разрешения для того же экрана.
}

/// Редактор одного открытого разговора. Текст живёт в дочернем поле.
pub(super) struct Composer {
    address: ConversationAddress, // Снимок адреса разговора на момент открытия.
    editor: Entity<Editor>,       // Многострочный ввод, выделение и версия текста.
    destination: Destination,     // Состояние проверки возможности отправки.
    writing: Option<Attempt>,     // Текущая запись; None позволяет начать следующую.
    error_version: Option<u64>,   // Версия текста, для которой запись не удалась.
    _editor_observer: Subscription, // Обновляет доступность кнопки после редактирования.
    _editor_submit: Subscription, // Связывает Enter с тем же действием, что и кнопку отправки.
}

impl Composer {
    /// Создаёт редактор привязанного к открытому разговору черновика.
    pub fn new(address: ConversationAddress, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(Editor::for_composer);
        let observer = cx.observe(&editor, |_, _, cx| cx.notify());
        let submit = cx.subscribe(&editor, |composer, _, _: &SubmitRequested, cx| {
            composer.submit(cx);
        });
        Self {
            address,
            editor,
            destination: Destination::Unchecked,
            writing: None,
            error_version: None,
            _editor_observer: observer,
            _editor_submit: submit,
        }
    }

    /// Читает содержимое поля для проверок интеграции через окно.
    #[cfg(all(test, feature = "gpui-test"))]
    pub fn draft<'a>(&self, app: &'a App) -> &'a str {
        self.editor.read(app).text()
    }

    /// Даёт тесту доступ к системному обработчику ввода рабочего поля.
    #[cfg(all(test, feature = "gpui-test"))]
    pub fn editor(&self) -> Entity<Editor> {
        self.editor.clone()
    }

    /// Убирает разрешение на отправку, пока история проходит повторную проверку.
    pub fn invalidate_destination(&mut self, cx: &mut Context<Self>) {
        self.destination = Destination::Unchecked;
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
        log::debug!(
            "gpui_send op_id={op_id} event=destination_requested stream_id={} generation={} revision={}",
            address.route.scope.stream_id,
            address.generation.0,
            address.revision.0,
        );
        cx.spawn(async move |editor, cx| {
            let reply = resolve(address, op_id).await;
            let _ = editor.update(cx, |editor, cx| {
                editor.finish_destination(op_id, reply, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// Применяет разрешение только для текущего адреса и действующей проверки.
    fn finish_destination(
        &mut self,
        op_id: Uuid,
        reply: ConversationReply<Result<Uuid, DestinationError>>,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.destination, Destination::Checking(current) if current == op_id)
            || reply.address != self.address
        {
            log::debug!("gpui_send op_id={op_id} event=destination_ignored");
            return;
        }
        self.destination = match reply.value {
            Ok(topic) => Destination::Ready(topic),
            Err(error) => Destination::Unavailable(error),
        };
        log::debug!(
            "gpui_send op_id={op_id} event=destination_finished available={}",
            matches!(self.destination, Destination::Ready(_)),
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
        matches!(self.destination, Destination::Ready(_))
            && self.writing.is_none()
            && !editor.composing()
            && !editor.text().trim().is_empty()
            && !over_limit(editor.text())
    }

    /// Блокирует повторное нажатие и передаёт экрану прежний текст и адрес.
    fn submit(&mut self, cx: &mut Context<Self>) {
        let Destination::Ready(topic) = self.destination else {
            return;
        };
        if !self.can_send(cx) {
            return;
        }
        let editor = self.editor.read(cx);
        let attempt = Attempt {
            op_id: Uuid::new_v4(),
            version: editor.version(),
        };
        let submission = Submission {
            address: self.address.clone(),
            attempt,
            topic,
            markdown: editor.text().to_owned(),
        };
        self.writing = Some(attempt);
        self.error_version = None;
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
            && matches!(self.destination, Destination::Ready(topic) if topic == submission.topic)
    }

    /// Завершает запись, очищая только неизменившийся черновик этого редактора.
    pub fn finish(&mut self, attempt: Attempt, saved: bool, cx: &mut Context<Self>) {
        if self.writing != Some(attempt) {
            return;
        }
        self.writing = None;
        let editor = self.editor.read(cx);
        let same_version = editor.version() == attempt.version && !editor.composing();
        if same_version {
            self.error_version = (!saved).then_some(attempt.version);
            if saved {
                self.editor.update(cx, Editor::clear);
            }
        }
        log::debug!(
            "gpui_send op_id={} event=draft_finished saved={saved} cleared={}",
            attempt.op_id,
            saved && same_version,
        );
        cx.notify();
    }
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Render for Composer {
    /// Показывает поле, доступность отправки и причину, если сохранить текст не получилось.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(ThemeMode::from(window.appearance()));
        let editor = self.editor.read(cx);
        let too_long = over_limit(editor.text());
        let save_failed = self.error_version == Some(editor.version());
        let explanation = match &self.destination {
            Destination::Unchecked | Destination::Checking(_) => {
                Some(("Проверяем возможность отправки…", false))
            }
            Destination::Unavailable(error) => Some((error.text(), false)),
            Destination::Ready(_) if too_long => Some(("Текст длиннее 40 000 символов", true)),
            Destination::Ready(_) if save_failed => {
                Some(("Не удалось сохранить сообщение. Повторите отправку", true))
            }
            Destination::Ready(_) => None,
        };
        let appearance = if too_long || save_failed {
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
        // Сетка сначала задаёт общую ширину поля и пояснения. От неё зависит высота переносов.
        div()
            .grid()
            .grid_cols(1)
            .flex_none()
            .min_w_0()
            .gap(px(4.))
            .p_2()
            .border_t_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .debug_selector(|| "conversation-composer".into())
            .child(
                div()
                    .flex()
                    .min_w_0()
                    .items_end()
                    .gap(px(12.))
                    .child(
                        field(palette, appearance, self.editor.clone())
                            .flex_1()
                            .debug_selector(|| "conversation-composer-field".into())
                            .rounded(px(12.)),
                    )
                    .child(
                        send.render(palette, None, move |window, app| {
                            let _ = target.update(app, |composer, cx| {
                                window.focus(&composer.editor.focus_handle(cx));
                                composer.submit(cx);
                            });
                        })
                        .size(px(40.))
                        .flex_none()
                        .p(px(0.))
                        .rounded(px(12.))
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
            .rounded_md()
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
