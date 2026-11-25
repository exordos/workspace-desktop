//! Ведёт карточки композера от выбора до отправки, отмены или удаления.
//! Состояние хранит порядок добавления, а фоновая работа сверяет попытку и цель перед записью.

mod edit;
mod markup;
mod presentation;
mod source;
mod transport;

use std::{borrow::Cow, path::PathBuf, sync::Arc};

use gpui::{
    App, Context, Div, FocusHandle, Image, PathPromptOptions, Pixels, Point, RenderImage, Stateful,
    Task, Window,
};
use tokio::sync::watch;
use uuid::Uuid;
use workspace_core::messenger::files::UploadedAttachment;

use super::{Composer, Destination, toolbar::icon_button};
use crate::gpui_client::{
    preferences::Language,
    runtime::RuntimeClient,
    ui::{Icon, Palette},
};

pub(super) use presentation::AttachmentDrag;
use presentation::{Card, CardId, CardPreview, CardStatus};
use source::Source;
use transport::{UploadAttempt, discard_uploaded};

const MAX_CONCURRENT_UPLOADS: usize = 5;

/// Версия подтверждённой темы и режима редактора. Вкладки ответа разделяют список файлов.
#[derive(Clone, Copy, PartialEq, Eq)]
struct UploadTarget {
    topic: Uuid,           // Точная серверная тема.
    check: Uuid,           // Повторная проверка не наследует запросы предыдущего показа.
    encrypted: bool,       // Какое разрешение Core использует для передачи файла.
    edit_id: Option<Uuid>, // None для нового сообщения, иначе точная исходная запись.
}

/// Открытый системный диалог, который ещё может вернуть список файлов.
struct Picker {
    target: UploadTarget, // Цель до открытия диалога.
    _task: Task<()>,      // Результат диалога игнорируется после смены назначения.
}

/// Один файл и его текущая попытка, независимо от соседних карточек.
enum Status {
    Queued, // Файл ждёт свободное место в очереди.
    Working {
        attempt: Uuid,               // Меняется при повторе того же вложения.
        cancel: watch::Sender<bool>, // Отмена сетевого запроса при удалении карточки.
        progress: Option<u8>,        // None, пока идёт локальное чтение и подготовка.
    },
    Ready {
        file: Uuid,                          // Подтверждённый UUID для сопоставления с текстом.
        markdown: String,                    // Точное тело ссылки для итогового сообщения.
        cleanup: Option<UploadedAttachment>, // После возможной передачи серверу удалять опасно.
    },
    Error {
        key: &'static str, // Ключ пояснения на языке аккаунта.
        retryable: bool,   // Только ошибка передачи допускает повтор с тем же файлом.
    },
}

/// Упорядоченная позиция карточки, сохраняющая идентичность во время повтора.
struct Slot {
    id: Uuid,                          // Локальный идентификатор для кнопок и перетаскивания.
    source: Source,                    // Выбранный путь или снимок буфера.
    content_type: Option<String>,      // MIME после чтения файла.
    size: Option<u64>,                 // Длина исходного файла в байтах.
    preview: Option<Arc<RenderImage>>, // Уменьшенный кадр после проверки файла.
    status: Status,                    // Видимое состояние и данные для следующего действия.
}

/// Снимок переданной попытки, который ещё можно вернуть после отказа.
struct Submission {
    operation: Uuid,              // Операция записи, которой передали ссылки.
    target: Option<UploadTarget>, // Цель для восстановления после отказа.
    slots: Vec<Slot>,             // Только готовые карточки конкретной отправки.
}

/// Вложения одного открытого композера.
#[derive(Default)]
pub(super) struct AttachmentState {
    target: Option<UploadTarget>, // Версия темы, которой принадлежат новые карточки.
    picker: Option<Picker>,       // Системный выбор; файлы пока ещё не добавлены.
    picker_failed: bool,          // Сбой системного окна можно скрыть и открыть выбор вновь.
    slots: Vec<Slot>,             // Порядок исходного выбора, а не окончания запросов.
    submission: Option<Submission>, // Ссылки, которыми временно владеет операция записи.
    edit: edit::Existing,         // Вложения исходного сообщения, которые можно отвязать.
    strip: presentation::AttachmentStrip, // Положение прокрутки карточек и видимость фокуса.
}

impl AttachmentState {
    /// Блокирует отправку, пока в списке остаётся неподтверждённый файл.
    pub(super) fn pending(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| !matches!(slot.status, Status::Ready { .. }))
    }

    /// Возвращает карточку только для ещё живой попытки передачи.
    fn working(&mut self, id: Uuid, attempt: Uuid) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|slot| {
            slot.id == id
                && matches!(slot.status, Status::Working { attempt: active, .. } if active == attempt)
        })
    }

    /// Собирает ссылки всех ещё временных файлов для очистки снимка черновика.
    fn temporary_links(&self) -> impl Iterator<Item = (Uuid, &str)> + '_ {
        self.slots
            .iter()
            .chain(self.submission.iter().flat_map(|sending| &sending.slots))
            .filter_map(Slot::link)
    }
}

impl Slot {
    /// Возвращает серверный UUID готового файла.
    fn file(&self) -> Option<Uuid> {
        self.link().map(|(id, _)| id)
    }

    /// Возвращает точную временную ссылку даже после её переноса в обычный текст или блок кода.
    fn link(&self) -> Option<(Uuid, &str)> {
        match &self.status {
            Status::Ready { file, markdown, .. } => Some((*file, markdown.as_str())),
            _ => None,
        }
    }

    /// Освобождает текстуру после снятия карточки с экрана.
    fn release_preview(&mut self, cx: &mut App) {
        if let Some(image) = self.preview.take() {
            cx.defer(move |app| app.drop_image(image, None));
        }
    }

    /// Отменяет текущую попытку или отдаёт квитанцию для отдельной попытки удаления.
    fn retire(mut self, cx: &mut App) -> Option<UploadedAttachment> {
        self.release_preview(cx);
        match self.status {
            Status::Working { cancel, .. } => {
                let _ = cancel.send(true);
                None
            }
            Status::Ready { cleanup, .. } => cleanup,
            _ => None,
        }
    }
}

/// Освобождает карточки одним заданием, даже если исходный список длиннее очереди команд.
fn discard_slots(
    slots: impl IntoIterator<Item = Slot>,
    runtime: Option<&RuntimeClient>,
    cx: &mut App,
) {
    let receipts = slots
        .into_iter()
        .filter_map(|slot| slot.retire(cx))
        .collect::<Vec<_>>();
    if let Some(runtime) = runtime.cloned().filter(|_| !receipts.is_empty()) {
        cx.background_executor()
            .spawn(async move { discard_uploaded(&runtime, receipts).await })
            .detach();
    }
}

impl Composer {
    /// Даёт сквозным тестам показать вложение, восстановленное из существующего черновика.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_complete_attachment(
        &mut self,
        markdown: &str,
        cx: &mut Context<Self>,
    ) {
        self.editor
            .update(cx, |editor, cx| editor.insert_attachment(markdown, cx));
    }

    /// Проверяет доступ и режим перед использованием экрана для фоновой передачи.
    fn attachment_target(&self) -> Option<UploadTarget> {
        let Destination::Ready {
            topic,
            encrypted,
            check_id,
        } = self.destination
        else {
            return None;
        };
        if self.draft_runtime.is_none()
            || (!encrypted && self.plain_target.is_none())
            || self
                .edit
                .as_ref()
                .is_some_and(|edit| !edit.access.current())
        {
            return None;
        }
        Some(UploadTarget {
            topic,
            check: check_id,
            encrypted,
            edit_id: self.edit.as_ref().map(|edit| edit.expected.id),
        })
    }

    /// Снимает запросы прежней темы и не трогает переключение вкладок ответа.
    pub(super) fn validate_attachment_target(&mut self, cx: &mut Context<Self>) {
        let current = self.attachment_target();
        if self.attachment_upload.target.is_some() && self.attachment_upload.target != current {
            self.discard_new_attachments(cx);
        }
    }

    /// Проверяет возможность добавить файлы поверх уже показанных карточек.
    pub(super) fn can_drop_attachments(&self, _: &App) -> bool {
        self.writing.is_none() && self.attachment_target().is_some()
    }

    /// Добавляет перетащенные пути в очередь в том порядке, который передала система.
    pub(super) fn drop_attachments(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        self.add_attachments(paths.iter().cloned().map(Source::path), cx);
    }

    /// Добавляет снимок буфера в ту же очередь, что системный выбор и перетаскивание.
    pub(super) fn paste_attachment(&mut self, image: Arc<Image>, cx: &mut Context<Self>) {
        self.add_attachments([Source::clipboard(image)], cx);
    }

    /// Создаёт карточки до начала чтения, чтобы очередь оставалась видимой и упорядоченной.
    fn add_attachments(
        &mut self,
        sources: impl IntoIterator<Item = Source>,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.attachment_target().filter(|_| self.writing.is_none()) else {
            return;
        };
        self.validate_attachment_target(cx);
        self.attachment_upload.target = Some(target);
        self.attachment_upload.picker_failed = false;
        self.attachment_upload
            .slots
            .extend(sources.into_iter().map(|source| Slot {
                id: Uuid::new_v4(),
                source,
                content_type: None,
                size: None,
                preview: None,
                status: Status::Queued,
            }));
        self.pump_attachments(cx);
        cx.notify();
    }

    /// Показывает кнопку выбора, пока подтверждён текущий разговор.
    pub(super) fn attachment_button(
        &self,
        palette: Palette,
        language: Language,
        focus: &FocusHandle,
        compact: bool,
        cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        self.attachment_target()?;
        let composer = cx.entity().downgrade();
        let glyph = if compact {
            (Icon::ComposerAttachCompact, [14., 24.])
        } else {
            (Icon::ComposerAttach, [14.35, 24.])
        };
        Some(icon_button(
            "composer-attach-file",
            language.tr("desktop.messenger.composer.attachFile"),
            glyph,
            palette,
            focus,
            self.writing.is_some() || self.attachment_upload.picker.is_some(),
            move |_, app| {
                let _ = composer.update(app, |composer, cx| composer.choose_attachments(cx));
            },
        ))
    }

    /// Открывает один системный диалог без остановки загрузок, которые уже начались.
    fn choose_attachments(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self
            .attachment_target()
            .filter(|_| self.attachment_upload.picker.is_none() && self.writing.is_none())
        else {
            return;
        };
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(
                self.language(cx)
                    .tr("desktop.messenger.composer.attachFile")
                    .into(),
            ),
        });
        self.attachment_upload.picker_failed = false;
        let task = cx.spawn(async move |entity, cx| {
            let result = dialog.await;
            let _ = entity.update(cx, |composer, cx| {
                if composer
                    .attachment_upload
                    .picker
                    .take()
                    .map(|picker| picker.target)
                    != Some(target)
                    || composer.attachment_target() != Some(target)
                    || composer.writing.is_some()
                {
                    return;
                }
                match result {
                    Ok(Ok(Some(paths))) => {
                        composer.add_attachments(paths.into_iter().map(Source::path), cx)
                    }
                    Ok(Ok(None)) => {}
                    _ => composer.attachment_upload.picker_failed = true,
                }
                cx.notify();
            });
        });
        self.attachment_upload.picker = Some(Picker {
            target,
            _task: task,
        });
        cx.notify();
    }

    /// Даёт каждому свободному месту первый ожидающий файл.
    fn pump_attachments(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self
            .attachment_upload
            .target
            .filter(|target| self.attachment_target() == Some(*target))
        else {
            return;
        };
        let active = self
            .attachment_upload
            .slots
            .iter()
            .filter(|slot| matches!(slot.status, Status::Working { .. }))
            .count();
        let queued = self
            .attachment_upload
            .slots
            .iter_mut()
            .filter(|slot| matches!(slot.status, Status::Queued))
            .take(MAX_CONCURRENT_UPLOADS.saturating_sub(active));
        let runtime = self
            .draft_runtime
            .clone()
            .expect("цель проверена с рабочим потоком");
        for slot in queued {
            let attempt = Uuid::new_v4();
            let (cancel, canceled) = watch::channel(false);
            slot.status = Status::Working {
                attempt,
                cancel,
                progress: None,
            };
            UploadAttempt {
                id: slot.id,
                attempt,
                target,
                source: slot.source.clone(),
                canceled,
                runtime: runtime.clone(),
                address: self.address.clone(),
            }
            .start(cx);
        }
    }

    /// Повторяет передачу только для карточки с сетевой ошибкой в её исходной области.
    fn retry_attachment_card(&mut self, id: CardId, cx: &mut Context<Self>) {
        let CardId::Upload(id) = id else { return };
        if !self.can_drop_attachments(cx)
            || self.attachment_upload.target != self.attachment_target()
        {
            return;
        }
        let Some(slot) = self
            .attachment_upload
            .slots
            .iter_mut()
            .find(|slot| slot.id == id)
        else {
            return;
        };
        if matches!(
            slot.status,
            Status::Error {
                retryable: true,
                ..
            }
        ) {
            slot.status = Status::Queued;
            self.pump_attachments(cx);
            cx.notify();
        }
    }

    /// Скрывает карточку сразу, отменяя незаконченный запрос или планируя удаление нового файла.
    fn remove_attachment_card(&mut self, id: CardId, window: &mut Window, cx: &mut Context<Self>) {
        if self.writing.is_some() || self.editor.read(cx).composing() {
            return;
        }
        match id {
            CardId::Upload(id) if id.is_nil() => self.attachment_upload.picker_failed = false,
            CardId::Upload(id) => {
                let Some(index) = self
                    .attachment_upload
                    .slots
                    .iter()
                    .position(|slot| slot.id == id)
                else {
                    return;
                };
                let slot = self.attachment_upload.slots.remove(index);
                if let Some(link) = slot.link() {
                    self.remove_inline_files(&[link], cx);
                }
                discard_slots([slot], self.draft_runtime.as_ref(), cx);
                self.pump_attachments(cx);
            }
            card @ CardId::Reference { id, offset } => {
                if let Some(markdown) = self.remove_existing_attachment(card, cx) {
                    self.editor.update(cx, |editor, cx| {
                        editor.remove_attachment_link(&markdown, cx)
                    });
                } else if self.edit.is_none() {
                    self.editor
                        .update(cx, |editor, cx| editor.remove_attachment(id, offset, cx));
                }
            }
        }
        self.focus_editor(window, cx);
        cx.notify();
    }

    /// Соединяет карточки исходного сообщения и новые файлы в порядке добавления.
    pub(super) fn attachments_panel(
        &self,
        palette: Palette,
        language: Language,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        let editor = self.editor.read(cx);
        let editable = self.writing.is_none() && !editor.composing();
        let protect_fragments = self
            .plain_target
            .as_ref()
            .is_some_and(|target| target.fragments_allowed());
        let active_files = markup::public_edit_media(editor.text(), protect_fragments);
        let shown_inline = |link: &str| {
            active_files
                .iter()
                .any(|file| &editor.text()[file.range.clone()] == link)
        };
        let mut cards = self.existing_attachment_cards(shown_inline, editable);
        let temporary: Vec<_> = self
            .attachment_upload
            .temporary_links()
            .map(|(id, _)| id)
            .collect();
        let legacy_cards = self.edit.is_none()
            && !(protect_fragments
                && workspace_core::messenger::encryption::fragments::has_protected_draft(
                    editor.text(),
                ));
        cards.extend(editor.attachments().filter_map(|(id, offset, name)| {
            if !legacy_cards || temporary.contains(&id) {
                return None;
            }
            let reference = active_files.iter().find(|file| file.file_id == id);
            Some(Card {
                id: CardId::Reference { id, offset },
                name: name.to_owned(),
                content_type: reference.and_then(|file| file.metadata.content_type.clone()),
                size: reference.and_then(|file| file.metadata.size_bytes),
                preview: None,
                status: CardStatus::Ready,
                can_remove: editable,
                can_inline: false,
                in_text: false,
            })
        }));
        cards.extend(self.attachment_upload.slots.iter().map(|slot| {
            Card {
                id: CardId::Upload(slot.id),
                name: slot.source.name(),
                content_type: slot.content_type.clone(),
                size: slot.size,
                preview: slot.preview.clone().map(CardPreview::Plain),
                status: match slot.status {
                    Status::Queued => CardStatus::Queued,
                    Status::Working { progress: None, .. } => CardStatus::Validating,
                    Status::Working {
                        progress: Some(progress),
                        ..
                    } => CardStatus::Uploading(progress),
                    Status::Ready { .. } => CardStatus::Ready,
                    Status::Error { key, retryable } => CardStatus::Error { key, retryable },
                },
                can_remove: editable,
                can_inline: editable && slot.file().is_some(),
                in_text: slot
                    .link()
                    .is_some_and(|(_, link)| editor.text().contains(link)),
            }
        }));
        if self.attachment_upload.picker_failed {
            cards.push(Card {
                id: CardId::Upload(Uuid::nil()),
                name: language
                    .tr("desktop.messenger.composer.attachFile")
                    .to_owned(),
                content_type: None,
                size: None,
                preview: None,
                status: CardStatus::Error {
                    key: "desktop.messenger.composer.attachmentOpenFailed",
                    retryable: false,
                },
                can_remove: true,
                can_inline: false,
                in_text: false,
            });
        }
        self.attachment_upload.strip.render(
            cards,
            palette,
            language,
            cx.entity().downgrade(),
            window,
            cx,
        )
    }

    /// Вставляет готовое изображение в позицию, указанную перетаскиванием его карточки.
    pub(super) fn insert_inline_attachment(
        &mut self,
        id: CardId,
        point: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.writing.is_some()
            || self.editor.read(cx).composing()
            || self.attachment_target().is_none()
        {
            return;
        }
        let attachment = match id {
            CardId::Upload(id) => self
                .attachment_upload
                .slots
                .iter()
                .find_map(|slot| match &slot.status {
                    Status::Ready { file, markdown, .. }
                        if slot.id == id && slot.preview.is_some() =>
                    {
                        Some((*file, markdown.as_str()))
                    }
                    _ => None,
                }),
            reference => self.attachment_upload.edit.for_inline(reference),
        };
        let Some((file, markdown)) = attachment else {
            return;
        };
        let editor = self.editor.read(cx);
        let current = self.replies.markdown(editor.text());
        if current.contains(markdown)
            || matches!(id, CardId::Upload(_)) && markup::contains_file(&current, file)
        {
            return;
        }
        let markdown = markdown.to_owned();
        let temporary = matches!(id, CardId::Upload(_));
        self.editor.update(cx, |editor, cx| {
            editor.insert_inline_attachment(&markdown, point, temporary, cx);
        });
    }

    /// Убирает изображение из текста, сохраняя его готовую карточку и файл.
    fn remove_inline_attachment(
        &mut self,
        id: CardId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.writing.is_some() || self.editor.read(cx).composing() {
            return;
        }
        match id {
            CardId::Upload(id) => {
                let link = self
                    .attachment_upload
                    .slots
                    .iter()
                    .find(|slot| slot.id == id)
                    .and_then(Slot::link)
                    .map(|(file, markdown)| (file, markdown.to_owned()));
                if let Some((file, markdown)) = link {
                    self.remove_inline_files(&[(file, &markdown)], cx);
                }
            }
            reference => {
                if let Some(markdown) = self.attachment_upload.edit.link_for(reference) {
                    self.editor
                        .update(cx, |editor, cx| editor.remove_attachment_link(markdown, cx));
                }
            }
        }
        self.focus_editor(window, cx);
    }

    /// Очищает видимое поле и скрытые вкладки от указанных внутритекстовых ссылок.
    fn remove_inline_files(&mut self, links: &[(Uuid, &str)], cx: &mut Context<Self>) {
        if links.is_empty() {
            return;
        }
        self.replies
            .rewrite_attachment_text(|markdown| markup::without_files(markdown, links));
        let files = links.iter().map(|(file, _)| *file).collect::<Vec<_>>();
        self.editor
            .update(cx, |editor, cx| editor.remove_attachment_files(&files, cx));
        // Просим сохранить и скрытую вкладку после того, как завершится текущее действие.
        // При принятой отправке редактор к тому моменту уже успеет очистить старый снимок.
        let composer = cx.entity().downgrade();
        cx.defer(move |app| {
            let _ = composer.update(app, |composer, cx| composer.persist_draft(cx));
        });
    }

    /// Исключает временные ссылки из черновика, который можно открыть после закрытия композера.
    pub(super) fn persistent_attachment_markdown<'a>(&self, markdown: &'a str) -> Cow<'a, str> {
        let links = self.attachment_upload.temporary_links().collect::<Vec<_>>();
        markup::without_files(markdown, &links).map_or(Cow::Borrowed(markdown), Cow::Owned)
    }

    /// Возвращает наличие отдельного файла для отправки даже с пустым полем сообщения.
    pub(super) fn has_ready_attachments(&self) -> bool {
        self.attachment_upload
            .slots
            .iter()
            .any(|slot| slot.file().is_some())
            || !self.attachment_upload.edit.is_empty()
    }

    /// Добавляет подтверждённые ссылки к снимку сообщения, исключая уже вставленные в текст.
    pub(super) fn attachment_markdown(&self, markdown: &str) -> String {
        markup::with_attachments(
            markdown,
            self.attachment_upload.edit.links(),
            self.attachment_upload.slots.iter().filter_map(Slot::link),
            self.edit.is_some(),
            self.plain_target
                .as_ref()
                .is_some_and(|target| target.fragments_allowed()),
        )
    }

    /// Передаёт черновые карточки во владение одной операции записи.
    pub(super) fn begin_attachment_submission(&mut self, operation: Uuid, cx: &mut Context<Self>) {
        self.attachment_upload.picker.take();
        self.attachment_upload.picker_failed = false;
        if !self.attachment_upload.slots.is_empty() {
            self.attachment_upload.submission = Some(Submission {
                operation,
                target: self.attachment_upload.target,
                slots: std::mem::take(&mut self.attachment_upload.slots),
            });
        }
        cx.notify();
    }

    /// Завершает передачу карточек или возвращает их после сбоя записи в той же области.
    pub(super) fn finish_attachment_submission(
        &mut self,
        operation: Uuid,
        accepted: bool,
        uncertain: bool,
        cx: &mut Context<Self>,
    ) {
        if self
            .attachment_upload
            .submission
            .as_ref()
            .is_none_or(|sending| sending.operation != operation)
        {
            return;
        }
        let mut sending = self
            .attachment_upload
            .submission
            .take()
            .expect("попытка сверена");
        if accepted || uncertain {
            for slot in &mut sending.slots {
                if let Status::Ready { cleanup, .. } = &mut slot.status {
                    cleanup.take();
                }
            }
        }
        if !accepted && self.attachment_target() == sending.target {
            self.attachment_upload.slots = sending.slots;
            return;
        }
        let files = sending
            .slots
            .iter()
            .filter_map(Slot::link)
            .collect::<Vec<_>>();
        self.remove_inline_files(&files, cx);
        discard_slots(sending.slots, self.draft_runtime.as_ref(), cx);
    }

    /// Закрывает текущий набор после смены режима или потери разрешения.
    pub(super) fn discard_attachments(&mut self, cx: &mut Context<Self>) {
        self.discard_new_attachments(cx);
        self.attachment_upload.edit.release(cx);
    }

    /// Закрывает новые карточки и убирает вставленные временные ссылки.
    pub(super) fn discard_new_attachments(&mut self, cx: &mut Context<Self>) {
        let slots = std::mem::take(&mut self.attachment_upload.slots);
        let files = slots.iter().filter_map(Slot::link).collect::<Vec<_>>();
        self.remove_inline_files(&files, cx);
        self.attachment_upload.target = None;
        self.attachment_upload.picker.take();
        self.attachment_upload.picker_failed = false;
        discard_slots(slots, self.draft_runtime.as_ref(), cx);
    }

    /// Освобождает карточки закрытого редактора, сохраняя файлы уже переданной операции.
    pub(super) fn release_attachments(&mut self, cx: &mut App) {
        self.attachment_upload.edit.release(cx);
        discard_slots(
            self.attachment_upload.slots.drain(..),
            self.draft_runtime.as_ref(),
            cx,
        );
        if let Some(sending) = self.attachment_upload.submission.take() {
            for mut slot in sending.slots {
                slot.release_preview(cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Оставляет ошибки в исходном порядке и принимает только ответ действующей попытки.
    #[test]
    fn failed_card_blocks_until_removed_and_old_attempt_is_ignored() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let attempt = Uuid::from_u128(3);
        let (cancel, _) = watch::channel(false);
        let slot = |id, source: &str, status| Slot {
            id,
            source: Source::path(source.into()),
            content_type: None,
            size: None,
            preview: None,
            status,
        };
        let mut state = AttachmentState {
            slots: vec![
                slot(
                    first,
                    "one.txt",
                    Status::Error {
                        key: "test",
                        retryable: false,
                    },
                ),
                slot(
                    second,
                    "two.txt",
                    Status::Working {
                        attempt,
                        cancel,
                        progress: Some(50),
                    },
                ),
            ],
            ..AttachmentState::default()
        };
        assert!(state.working(second, Uuid::nil()).is_none());
        assert!(state.pending());
        let current = state.working(second, attempt).unwrap();
        current.status = Status::Ready {
            file: second,
            markdown: String::new(),
            cleanup: None,
        };
        assert!(state.working(second, attempt).is_none());
        assert!(state.pending());
        assert_eq!(state.slots.remove(0).id, first);
        assert!(!state.pending());
        assert_eq!(state.temporary_links().collect::<Vec<_>>(), [(second, "")]);
    }
}
