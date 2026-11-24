//! Собирает редактор сообщения, состояние проверки темы и типизированные действия отправки.
//! Экран разговора передаёт эти действия в рабочий поток; здесь остаются текст и его версия.

mod draft;
mod editor;
mod fragments;
mod replies;
mod source;
mod toolbar;
mod toolbar_preferences;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Pixels, Render,
    SharedString, Subscription, Window, div, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        encryption::{ProtectedMessage, fragments::FragmentEdit},
        quote::{Source as QuoteSource, SourceCheckError},
        send::PlainSendTarget,
    },
    storage::HistoryMessage,
};

pub(super) use self::draft::DraftBuffer;
pub use self::editor::Editor;
pub(super) use self::replies::{ReplyAction, ReplyQuote, ReplyText, short_reply_text};
pub(super) use self::source::{
    DestinationError, QueueWriteOutcome, ResolvedDestination, SendSource,
};
use self::{
    draft::{DraftSend, DraftSession},
    editor::{EditPreviousRequested, SubmitRequested},
    replies::Replies,
};
use super::{
    contracts::{ConversationAddress, ConversationReply},
    preferences::{Language, Preferences},
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

/// Точная тема и режим, которые подтвердили перед добавлением сообщения в очередь.
enum Destination {
    Unchecked,      // Проверка ещё не запущена или доступ к истории изменился.
    Checking(Uuid), // Идентификатор текущей проверки темы.
    Ready {
        topic: Uuid,     // UUID подтверждённой темы отправки.
        encrypted: bool, // Защищённый режим не использует локальный черновик открытого текста.
        check_id: Uuid,  // Идентификатор проверки, которая подтвердила тему.
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
    pub encrypted: bool,              // Истинно только после проверки защищённого режима Core.
    pub fragment_new_key: Option<bool>, // Для смешанного Markdown задаёт выбор поколения отправки.
    pub edit: Option<HistoryMessage>, // Исходный конверт и метаданные при правке.
    pub fragment_edit: Option<std::sync::Arc<FragmentEdit>>, // Разрешение на правку смешанного Markdown.
    pub quote_sources: Vec<QuoteSource>,                     // Исходники структурированных ответов.
    pub protected_sources: Vec<HistoryMessage>, // Версии защищённых оригиналов для отправки.
    draft: Option<std::sync::Arc<DraftSend>>, // Локальная версия, которую получит исходящая очередь.
}

/// Намерение, которое редактор передаёт экрану разговора.
pub(super) enum ComposerEvent {
    Send(Box<Submission>), // Просьба сохранить подготовленный текст в очереди.
    DestinationChanged,    // Экрану нужно обновить нижнюю область и действия ответа.
    DraftRestored,         // Истории нужно заполнить доступные предпросмотры восстановленных цитат.
    EditPrevious,          // Пустой защищённый редактор просит точную версию последнего сообщения.
}

/// Пояснение к той попытке, которая не попала в очередь.
#[derive(Clone, Copy)]
enum WriteFailure {
    Queue,                   // Запись в хранилище не подтверждена.
    Quote(SourceCheckError), // Сервер не подтвердил источники.
    Edit,                    // Правка не подтверждена сервером.
}

impl WriteFailure {
    /// Подсказывает, нужно ли восстановить цитату или повторить попытку.
    fn text(self, language: Language) -> &'static str {
        language.tr(match self {
            Self::Queue => "desktop.messenger.composer.queueFailed",
            Self::Edit => "desktop.messenger.composer.editFailed",
            Self::Quote(SourceCheckError::Missing) => "desktop.messenger.composer.quoteMissing",
            Self::Quote(SourceCheckError::Changed) => "desktop.messenger.composer.quoteChanged",
            Self::Quote(SourceCheckError::Unavailable) => {
                "desktop.messenger.composer.quoteCheckFailed"
            }
        })
    }
}

/// Открытая правка и доступ к той версии, с которой начал работать пользователь.
struct ProtectedEdit {
    expected: HistoryMessage, // Только шифротекст и метаданные исходной строки.
    access: EditAccess,       // Отзывной доступ к исходному тексту и всем нужным поколениям.
    draft: zeroize::Zeroizing<String>, // Незаконченный ввод до начала правки.
    draft_replies: Option<Replies>, // Вкладки обычного черновика, временно убранные с поля.
    _watch: Option<gpui::Task<()>>, // Для смешанной правки слушает отзыв ключа исходного чата.
}

/// Разрешение исходного сообщения, которое проверяется до продолжения правки.
enum EditAccess {
    Whole(std::sync::Arc<ProtectedMessage>), // Точный ключ целиком зашифрованного тела.
    Fragments(std::sync::Arc<FragmentEdit>), // Все ключи исходных фрагментов.
}

impl EditAccess {
    /// Проверяет права на дальнейшую работу с открытой строкой.
    fn current(&self) -> bool {
        match self {
            Self::Whole(access) => access.with_body(|_| ()).is_some(),
            Self::Fragments(edit) => edit.with_markdown(|_| ()).is_some(),
        }
    }

    /// Возвращает разрешение для соответствующей ветки рабочего запроса.
    fn fragments(&self) -> Option<std::sync::Arc<FragmentEdit>> {
        match self {
            Self::Fragments(edit) => Some(edit.clone()),
            Self::Whole(_) => None,
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
    plain_target: Option<PlainSendTarget>, // Проверенный тип провайдера открытого Markdown.
    destination: Destination,     // Состояние проверки возможности отправки.
    edit: Option<ProtectedEdit>,  // Правка защищённого сообщения до сохранения или отзыва.
    fragment_new_key: bool, // Нужен свежий локальный ключ только для следующей смешанной отправки.
    toolbar: Toolbar,       // Кнопка раскрытия, команды и их клавиатурный фокус.
    writing: Option<Attempt>, // Текущая запись; None позволяет начать следующую.
    write_failure: Option<(u64, u64, WriteFailure)>, // Версии ввода и вкладок при отказе записи.
    _editor_observer: Subscription, // Обновляет доступность кнопки после редактирования.
    #[cfg(all(test, feature = "gpui-test"))]
    renders: usize, // Вызовы настоящего Render в проверках изоляции GPUI.
    _editor_submit: Subscription, // Связывает Enter с тем же действием, что и кнопку отправки.
    _editor_edit: Subscription, // Запрашивает правку со стрелки вверх в пустом редакторе.
}

impl Composer {
    /// Использует язык владельца этого разговора для подписей редактора.
    fn language(&self, app: &App) -> Language {
        Preferences::read_for(app, &self.address.route.owner).language
    }

    /// Возвращает число сборок настоящего GPUI-представления для проверки изоляции.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn tested_render_count(&self) -> usize {
        self.renders
    }

    /// Переводит фокус на ввод, когда пользователь выбирает «Написать» в сведениях лички.
    pub(super) fn focus_editor(&self, window: &mut Window, app: &App) {
        if self.is_available() {
            self.editor_focus(app).focus(window);
        }
    }

    /// Возвращает цель редактора при обходе областей окна.
    pub(super) fn editor_focus(&self, app: &App) -> FocusHandle {
        self.editor.read(app).focus_handle(app)
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
        let edit = cx.subscribe(&editor, |composer, _, _: &EditPreviousRequested, cx| {
            if (composer.can_edit_protected() || composer.fragment_edit_topic().is_some())
                && composer.edit.is_none()
            {
                cx.emit(ComposerEvent::EditPrevious);
            }
        });
        Self {
            address,
            editor,
            replies: Replies::default(),
            draft_runtime: None,
            draft_session: None,
            draft_topic: None,
            plain_target: None,
            destination: Destination::Unchecked,
            edit: None,
            fragment_new_key: false,
            toolbar: Toolbar::new(cx),
            writing: None,
            write_failure: None,
            _editor_observer: observer,
            _editor_submit: submit,
            _editor_edit: edit,
            #[cfg(all(test, feature = "gpui-test"))]
            renders: 0,
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

    /// Проверяет, осталась ли правка открыта после подтверждения или смены версии.
    #[cfg(all(test, feature = "gpui-test"))]
    pub fn tested_edit_active(&self) -> bool {
        self.edit.is_some()
    }

    /// Возвращает причину завершённой проверки, если отправка заблокирована.
    pub(super) fn destination_error(&self) -> Option<DestinationError> {
        match self.destination {
            Destination::Unavailable(error) => Some(error),
            _ => None,
        }
    }

    /// Проверяет доступность редактора в обычном или защищённом режиме.
    pub(super) fn is_available(&self) -> bool {
        matches!(self.destination, Destination::Ready { .. })
            && (!matches!(
                self.destination,
                Destination::Ready {
                    encrypted: true,
                    ..
                }
            ) || self.replies.protected_current())
    }

    /// Возвращает точную тему, которой уже разрешена защищённая отправка.
    pub(super) fn protected_send_topic(&self) -> Option<Uuid> {
        match self.destination {
            Destination::Ready {
                topic,
                encrypted: true,
                ..
            } => Some(topic),
            _ => None,
        }
    }

    /// Возвращает точную тему, которой уже разрешена обычная отправка.
    pub(super) fn plain_send_topic(&self) -> Option<Uuid> {
        match self.destination {
            Destination::Ready {
                topic,
                encrypted: false,
                ..
            } => Some(topic),
            _ => None,
        }
    }

    /// Помнит подтверждённый защищённый режим и тогда, когда не найден ключ для новой отправки.
    pub(super) fn known_protected_destination(&self) -> bool {
        self.protected_send_topic().is_some()
            || matches!(
                self.destination,
                Destination::Unavailable(DestinationError::KeyUnavailable)
            )
    }

    /// Проверяет, что экран разрешил защищённую запись в этой теме.
    pub(super) fn can_edit_protected(&self) -> bool {
        self.writing.is_none()
            && matches!(
                self.destination,
                Destination::Ready {
                    encrypted: true,
                    ..
                }
            )
    }

    /// Начинает правку с точного снимка строки и сохраняет незаконченный ввод до возврата.
    pub(super) fn open_protected_edit(
        &mut self,
        expected: HistoryMessage,
        access: std::sync::Arc<ProtectedMessage>,
        cx: &mut Context<Self>,
    ) {
        if !self.can_edit_protected() || self.edit.is_some() {
            return;
        }
        let Some(text) = access.with_body(|text| zeroize::Zeroizing::new(text.to_owned())) else {
            return;
        };
        let draft = zeroize::Zeroizing::new(self.editor.read(cx).text().to_owned());
        self.edit = Some(ProtectedEdit {
            expected,
            access: EditAccess::Whole(access),
            draft,
            draft_replies: None,
            _watch: None,
        });
        self.editor
            .update(cx, |editor, cx| editor.show_reply(&text, cx));
        cx.notify();
    }

    /// Возвращает исходный ввод и закрывает правку при отмене или отзыве.
    pub(super) fn cancel_protected_edit(&mut self, message: Option<Uuid>, cx: &mut Context<Self>) {
        if message.is_some_and(|id| self.edit.as_ref().is_none_or(|edit| edit.expected.id != id)) {
            return;
        }
        if let Some(edit) = self.edit.take() {
            if let Some(replies) = edit.draft_replies {
                self.replies = replies;
            }
            self.editor.update(cx, |editor, cx| {
                editor.finish_fragment_edit();
                editor.show_reply(&edit.draft, cx);
            });
            self.write_failure = None;
            cx.notify();
        }
    }

    /// Перепроверяет только редактор защищённого режима после смены локального ключа.
    pub(super) fn refresh_encrypted_destination(
        &mut self,
        source: &SendSource,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            self.destination,
            Destination::Ready {
                encrypted: true,
                ..
            } | Destination::Checking(_)
                | Destination::Unavailable(DestinationError::KeyUnavailable)
        ) {
            if self
                .edit
                .as_ref()
                .is_some_and(|edit| !edit.access.current())
            {
                self.cancel_protected_edit(None, cx);
            }
            self.invalidate_destination(cx);
            self.check_destination(source, cx);
        }
    }

    /// Убирает разрешение на отправку, пока история проходит повторную проверку.
    pub fn invalidate_destination(&mut self, cx: &mut Context<Self>) {
        // Сохраняем текущий снимок, чтобы вернуть ввод после подтверждения области.
        self.persist_draft(cx);
        self.plain_target = None;
        self.destination = Destination::Unchecked;
        cx.emit(ComposerEvent::DestinationChanged);
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
                self.plain_target = resolved.plain_target;
                if resolved.encrypted && self.draft_topic.is_some() {
                    self.revoke_draft(cx);
                }
                if let Some((access, saved)) = resolved.draft {
                    self.draft_topic = Some(topic);
                    self.hydrate_draft(access, saved, resolved.restored_protected, stamp, cx);
                }
                if resolved.closed {
                    Destination::Unavailable(DestinationError::TopicClosed)
                } else {
                    Destination::Ready {
                        topic,
                        encrypted: resolved.encrypted,
                        check_id: op_id,
                    }
                }
            }
            Err(error) => {
                self.plain_target = None;
                if matches!(
                    error,
                    DestinationError::EncryptionUnavailable
                        | DestinationError::AccessLost
                        | DestinationError::KeyUnavailable
                ) && self.draft_topic.is_some()
                {
                    self.revoke_draft(cx);
                }
                Destination::Unavailable(error)
            }
        };
        cx.emit(ComposerEvent::DestinationChanged);
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
        let markdown = self.replies.markdown(editor.text());
        let marked =
            workspace_core::messenger::encryption::fragments::has_protected_draft(&markdown);
        let markers_supported = !marked
            || self.protected_send_topic().is_some()
            || self.plain_target.as_ref().is_some_and(|target| {
                target.fragments_allowed()
                    || (target.literal_markers_allowed()
                        && self
                            .draft_session
                            .as_ref()
                            .is_none_or(|session| !session.protected))
            });
        matches!(self.destination, Destination::Ready { .. })
            && markers_supported
            && self.edit.as_ref().is_none_or(|edit| edit.access.current())
            && self.writing.is_none()
            && !editor.composing()
            && self.replies.has_answer(editor.text())
            && !over_limit(&markdown)
    }

    /// Блокирует повторное нажатие и передаёт экрану прежний текст и адрес.
    fn submit(&mut self, cx: &mut Context<Self>) {
        let Destination::Ready {
            topic,
            check_id,
            encrypted,
        } = self.destination
        else {
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
        let draft = if encrypted || self.edit.is_some() {
            None
        } else {
            self.prepare_draft_send(attempt.op_id, cx)
        };
        let editor = self.editor.read(cx);
        let markdown = self.replies.markdown(editor.text()).into_owned();
        let submission = Submission {
            draft,
            address: self.address.clone(),
            attempt,
            topic,
            encrypted,
            fragment_new_key: (self
                .plain_target
                .as_ref()
                .is_some_and(PlainSendTarget::fragments_allowed)
                && workspace_core::messenger::encryption::fragments::has_protected_draft(
                    &markdown,
                ))
            .then_some(self.fragment_new_key),
            edit: self.edit.as_ref().map(|edit| edit.expected.clone()),
            fragment_edit: self.edit.as_ref().and_then(|edit| edit.access.fragments()),
            markdown,
            quote_sources: self.replies.sources(),
            protected_sources: self.replies.protected_sources(),
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
        cx.emit(ComposerEvent::Send(Box::new(submission)));
        cx.notify();
    }

    /// Проверяет, что отложенное действие редактора ещё может поставить текст в очередь.
    pub fn accepts(&self, submission: &Submission) -> bool {
        self.address == submission.address
            && self.writing == Some(submission.attempt)
            && matches!(
                self.destination,
                Destination::Ready { topic, check_id, encrypted }
                    if topic == submission.topic && encrypted == submission.encrypted && check_id == submission.attempt.destination_check
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
            QueueWriteOutcome::Edited => self.cancel_protected_edit(None, cx),
            QueueWriteOutcome::StaleEdit => self.cancel_protected_edit(None, cx),
            QueueWriteOutcome::EditFailed if same_version => {
                self.write_failure =
                    Some((attempt.version, attempt.replies_version, WriteFailure::Edit));
            }
            QueueWriteOutcome::Saved(_) if same_version => {
                self.fragment_new_key = false;
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
                cx.emit(ComposerEvent::DestinationChanged);
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
        #[cfg(all(test, feature = "gpui-test"))]
        {
            self.renders += 1;
        }

        let palette = Palette::for_window(window, cx);
        let locale = self.language(cx);
        let editor = self.editor.read(cx);
        let expanded = ToolbarPreferences::expanded(cx);
        let can_format = editor.can_format();
        let too_long = over_limit(&self.replies.markdown(editor.text()));
        let write_failure = self.current_write_failure(cx);
        let explanation = if too_long {
            Some(locale.tr("composer.messageTooLong"))
        } else {
            write_failure.map(|failure| failure.text(locale))
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
        let edit_target = target.clone();
        let toolbar = expanded.then(|| {
            div()
                .flex()
                .items_center()
                .child(self.toolbar.row(palette, can_format, locale, window, cx))
                .children(self.fragment_toolbar(palette, locale, can_format, cx))
        });
        // Сетка сначала задаёт общую ширину поля и пояснения. От неё зависит высота переносов.
        // Явная ширина нужна и при отдельной раскладке композера из кэша GPUI.
        div()
            .grid()
            .grid_cols(1)
            .flex_none()
            .w_full()
            .min_w_0()
            .gap(px(4.))
            .p(COMPOSER_INSET)
            .rounded(SURFACE_RADIUS)
            .bg(palette.surface)
            .debug_selector(|| "conversation-composer".into())
            .children(self.edit.as_ref().map(|_| {
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(label(
                        palette,
                        TextRole::Caption,
                        locale.tr("desktop.messenger.composer.editingMessage"),
                    ))
                    .child(
                        Button::new("cancel-protected-edit", locale.tr("message.cancelEdit"))
                            .render(palette, None, move |_, cx| {
                                let _ = edit_target.update(cx, |composer, cx| {
                                    composer.cancel_protected_edit(None, cx)
                                });
                            })
                            .debug_selector(|| "cancel-protected-edit".into()),
                    )
            }))
            .children(self.render_replies(palette, window, cx))
            .children(self.fragment_key_option(palette, locale, cx))
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
                                        .toggle(false, palette, locale, cx)
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
                                    locale.tr("desktop.messenger.composer.savingMessage")
                                } else {
                                    locale.tr("desktop.messenger.composer.sendWithEnter")
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
            .when_some(explanation, |view, text| {
                view.child(
                    label(palette, TextRole::Caption, text)
                        .text_color(palette.danger)
                        .debug_selector(|| "conversation-send-explanation".into()),
                )
            })
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
