//! Запрашивает файл у macOS, загружает его в подтверждённый чат и добавляет ссылку в черновик.

use std::{
    fs::File,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{
    App, Context, Div, FocusHandle, Image, PathPromptOptions, Stateful, Task, div, prelude::*, px,
};
use tokio::sync::watch;
use uuid::Uuid;
use workspace_core::{
    messenger::{
        encryption::fragments::{fragment_display_spans, has_protected_draft},
        files::{AttachmentUpload, MAX_ATTACHMENT_UPLOAD_BYTES, UploadAttachmentError},
    },
    storage::HistoryScope,
};

use super::{Composer, Destination};
use crate::gpui_client::{
    preferences::Language,
    ui::{Button, ButtonAppearance, Icon, Palette, TextRole, label, popup},
};

/// Версия темы и выбранной вкладки, которая получит результат загрузки.
#[derive(Clone, Copy, PartialEq, Eq)]
struct UploadTarget {
    topic: Uuid,           // Точная серверная тема вместо общей области разговора.
    check: Uuid,           // Подтверждение той же цели на момент выбора файла.
    reply_revision: u64,   // Версия вкладок, защищающая соседние черновики.
    edit_id: Option<Uuid>, // Проверенная исходная запись, если это обычная правка.
}

/// Ошибка, после которой можно выбрать файл заново или явно повторить отправку.
struct Failure {
    path: Option<PathBuf>, // Локальный путь повторной попытки. В журналы не передаётся.
    key: &'static str,     // Перевод причины на языке текущего аккаунта.
    target: UploadTarget,  // Повтор разрешён только в исходной теме и вкладке.
}

/// Текущая операция выбора или передачи файла.
struct Pending {
    id: Uuid,                            // Отличает даже две попытки в той же теме и вкладке.
    target: UploadTarget,                // Проверенная тема, актуальная до выдачи результата.
    cancel: Option<watch::Sender<bool>>, // Есть после открытия файла, прерывает передачу в Core.
    _task: Task<()>,                     // Отменяет ожидание окна при смене разговора.
}

/// Одна операция на редактор. Готовые ссылки хранятся в самом черновике.
#[derive(Default)]
pub(super) struct AttachmentState {
    pending: Option<Pending>, // Отсутствует после отмены, подтверждения или ошибки.
    failures: Vec<Failure>,   // Ошибки файлов, с которыми закончились отдельные попытки.
}

/// Файл диска или снимок формата изображения, который macOS передала из буфера.
enum AttachmentSource {
    Path(PathBuf),         // Путь существует только на стороне Desktop.
    Clipboard(Arc<Image>), // Байты были подготовлены системой ввода.
}

impl AttachmentState {
    /// Предупреждает отправку, пока не закончился выбор или загрузка.
    pub(super) fn pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Прерывает фоновую передачу и закрывает состояние текущего запроса.
    pub(super) fn cancel(&mut self) {
        if let Some(pending) = self.pending.take()
            && let Some(cancel) = pending.cancel.as_ref()
        {
            let _ = cancel.send(true);
        }
        self.failures.clear();
    }
}

impl Drop for AttachmentState {
    /// Уведомляет Core об отмене, если окно исчезло до ответа.
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Composer {
    /// Передаёт штатному редактору результат подготовленного вложения для сквозной проверки.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_complete_attachment(
        &mut self,
        markdown: &str,
        cx: &mut Context<Self>,
    ) {
        self.editor
            .update(cx, |editor, cx| editor.insert_attachment(markdown, cx));
    }

    /// Разрешает передачу файла только при подтверждённой обычной отправке.
    fn attachment_target(&self, cx: &App) -> Option<UploadTarget> {
        let Destination::Ready {
            topic,
            encrypted: false,
            check_id,
        } = self.destination
        else {
            return None;
        };
        if self.plain_target.is_none()
            || self.writing.is_some()
            || self.draft_runtime.is_none()
            || has_protected_draft(&self.replies.markdown(self.editor.read(cx).text()))
        {
            return None;
        }
        let edit_id = if let Some(edit) = &self.edit {
            if !edit.access.current()
                || !matches!(&edit.access, super::EditAccess::Fragments(_))
                || edit.expected.body.markdown().is_none_or(|body| {
                    !fragment_display_spans(body).is_empty() || has_protected_draft(body)
                })
            {
                return None;
            }
            Some(edit.expected.id)
        } else {
            None
        };
        Some(UploadTarget {
            topic,
            check: check_id,
            reply_revision: self.replies.revision,
            edit_id,
        })
    }

    /// Отменяет загрузку при смене вкладки, цели, режима шифрования или доступности.
    pub(super) fn validate_attachment_target(&mut self, cx: &Context<Self>) {
        let current = self.attachment_target(cx);
        if let Some(pending) = &self.attachment_upload.pending
            && current != Some(pending.target)
        {
            self.attachment_upload.cancel();
        }
        self.attachment_upload
            .failures
            .retain(|failure| current == Some(failure.target));
    }

    /// Проверяет, может ли окно принять файлы с диска в этот редактор.
    pub(super) fn can_drop_attachments(&self, cx: &App) -> bool {
        !self.attachment_upload.pending() && self.attachment_target(cx).is_some()
    }

    /// Передаёт перетащенные файлы в тот же путь, который использует диалог выбора.
    pub(super) fn drop_attachments(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        if self.can_drop_attachments(cx)
            && let Some(target) = self.attachment_target(cx)
        {
            self.start_attachment_uploads(
                paths.iter().cloned().map(AttachmentSource::Path).collect(),
                target,
                cx,
            );
        }
    }

    /// Принимает вставленное изображение только после обычных проверок цели.
    pub(super) fn paste_attachment(&mut self, image: Arc<Image>, cx: &mut Context<Self>) {
        if self.can_drop_attachments(cx)
            && let Some(target) = self.attachment_target(cx)
        {
            self.start_attachment_uploads(vec![AttachmentSource::Clipboard(image)], target, cx);
        }
    }

    /// Показывает кнопку файлов, если Core подтвердил обычную отправку.
    pub(super) fn attachment_button(
        &self,
        palette: Palette,
        language: Language,
        focus: &FocusHandle,
        cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        self.attachment_target(cx)?;
        let composer = cx.entity().downgrade();
        let caption = language.tr("desktop.messenger.composer.attachFile");
        let mut button = Button::new("composer-attach-file", "");
        button.icon = Some(Icon::Attach);
        button.appearance = ButtonAppearance::Ghost;
        button.disabled = self.attachment_upload.pending();
        Some(
            button
                .render(palette, Some(focus), move |_, app| {
                    let _ = composer.update(app, |composer, cx| composer.choose_attachment(cx));
                })
                .size(px(32.))
                .p_0()
                .flex_none()
                .debug_selector(|| "composer-attach-file".into())
                .tooltip(move |_, app| popup::tooltip(palette, caption.into(), app)),
        )
    }

    /// Показывает со стороны редактора факт передачи, повтор и удаление готовых ссылок.
    pub(super) fn attachments_panel(
        &self,
        palette: Palette,
        language: Language,
        cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        let editor = self.editor.read(cx);
        let mut attachments = editor.attachments().peekable();
        if attachments.peek().is_none()
            && !self.attachment_upload.pending()
            && self.attachment_upload.failures.is_empty()
        {
            return None;
        }
        let cancel = cx.entity().downgrade();
        let mut panel = div()
            .id("composer-attachments")
            .flex()
            .flex_col()
            .gap_1()
            .max_h(px(132.))
            .overflow_y_scroll();
        if let Some(pending) = &self.attachment_upload.pending {
            let status = if pending.cancel.is_some() {
                "desktop.messenger.composer.attachmentUploading"
            } else {
                "desktop.messenger.composer.attachmentChoosing"
            };
            panel = panel.child(
                attachment_row(palette, language.tr(status)).child(
                    Button::new(
                        "composer-attachment-cancel",
                        language.tr("desktop.messenger.composer.timeCancel"),
                    )
                    .render(palette, None, move |_, app| {
                        let _ = cancel.update(app, |composer, cx| {
                            composer.attachment_upload.cancel();
                            cx.notify();
                        });
                    }),
                ),
            );
        }
        for (index, failure) in self.attachment_upload.failures.iter().enumerate() {
            let target = failure.target;
            let retry = failure
                .path
                .clone()
                .filter(|_| {
                    self.can_drop_attachments(cx) && self.attachment_target(cx) == Some(target)
                })
                .map(|path| {
                    let composer = cx.entity().downgrade();
                    Button::new(
                        ("composer-attachment-retry", index),
                        language.tr("desktop.messenger.composer.attachmentRetry"),
                    )
                    .render(palette, None, move |_, app| {
                        let _ = composer.update(app, |composer, cx| {
                            if composer.can_drop_attachments(cx)
                                && composer.attachment_target(cx) == Some(target)
                            {
                                composer
                                    .attachment_upload
                                    .failures
                                    .retain(|failure| failure.path.as_ref() != Some(&path));
                                composer.start_attachment_uploads(
                                    vec![AttachmentSource::Path(path.clone())],
                                    target,
                                    cx,
                                );
                            }
                        });
                    })
                });
            let reason = language.tr(failure.key);
            let caption = failure
                .path
                .as_ref()
                .and_then(|path| path.file_name())
                .map_or_else(
                    || reason.to_owned(),
                    |name| format!("{} · {reason}", name.to_string_lossy()),
                );
            panel = panel.child(attachment_row(palette, caption).children(retry));
        }
        for (index, (id, offset, name)) in attachments.enumerate() {
            let composer = cx.entity().downgrade();
            let button = Button::new(
                ("composer-attachment-remove", offset),
                language.tr("desktop.messenger.composer.attachmentRemove"),
            )
            .render(palette, None, move |window, app| {
                let _ = composer.update(app, |composer, cx| {
                    if !composer.editor.read(cx).composing() {
                        composer
                            .editor
                            .update(cx, |editor, cx| editor.remove_attachment(id, offset, cx));
                        composer.focus_editor(window, cx);
                    }
                });
            })
            .debug_selector(move || format!("composer-attachment-remove-{index}").into());
            panel = panel.child(attachment_row(palette, name.to_owned()).child(button));
        }
        Some(panel)
    }

    /// Открывает штатный выбор файла без передачи пути в сеть.
    fn choose_attachment(&mut self, cx: &mut Context<Self>) {
        if self.attachment_upload.pending() {
            return;
        }
        let Some(target) = self.attachment_target(cx) else {
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
        self.attachment_upload.failures.clear();
        let id = Uuid::new_v4();
        let task = cx.spawn(async move |entity, cx| {
            let result = dialog.await;
            let _ = entity.update(cx, |composer, cx| {
                if composer
                    .attachment_upload
                    .pending
                    .as_ref()
                    .map(|pending| pending.id)
                    != Some(id)
                {
                    return;
                }
                composer.attachment_upload.pending.take();
                if composer.attachment_target(cx) != Some(target) {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(Ok(Some(paths))) => {
                        composer.start_attachment_uploads(
                            paths.into_iter().map(AttachmentSource::Path).collect(),
                            target,
                            cx,
                        );
                    }
                    Ok(Ok(None)) => {}
                    _ => composer.attachment_upload.failures.push(Failure {
                        path: None,
                        key: "desktop.messenger.composer.attachmentOpenFailed",
                        target,
                    }),
                }
                cx.notify();
            });
        });
        self.attachment_upload.pending = Some(Pending {
            id,
            target,
            cancel: None,
            _task: task,
        });
        cx.notify();
    }

    /// Читает и загружает файлы по одному, чтобы набор файлов не занял память всего окна.
    fn start_attachment_uploads(
        &mut self,
        sources: Vec<AttachmentSource>,
        target: UploadTarget,
        cx: &mut Context<Self>,
    ) {
        if self.attachment_upload.pending() || sources.is_empty() {
            return;
        }
        let Some(runtime) = self.draft_runtime.clone() else {
            return;
        };
        let address = self.address.clone();
        let scope = HistoryScope {
            stream_id: address.route.scope.stream_id,
            topic_id: Some(target.topic),
        };
        let (cancel, canceled) = watch::channel(false);
        let background = cx.background_executor().clone();
        let id = Uuid::new_v4();
        let task = cx.spawn(async move |entity, cx| {
            for source in sources {
                if *canceled.borrow() {
                    return;
                }
                let path = source.path().map(Path::to_owned);
                let read = background.spawn(async move { source.load() });
                let upload_cancel = canceled.clone();
                let result = match read.await {
                    Ok(upload) => runtime
                        .conversation(address.clone(), move |services, address| async move {
                            services
                                .messenger
                                .upload_attachment(
                                    &services.sync,
                                    &address.route.owner,
                                    address.generation,
                                    scope,
                                    upload,
                                    Some(upload_cancel),
                                )
                                .await
                        })
                        .await
                        .value
                        .map_err(|_| "desktop.messenger.composer.attachmentFailed")
                        .and_then(|result| result.map_err(upload_error)),
                    Err(error) => Err(error),
                };
                let current = entity.update(cx, |composer, cx| {
                    if composer
                        .attachment_upload
                        .pending
                        .as_ref()
                        .map(|pending| pending.id)
                        != Some(id)
                    {
                        return false;
                    }
                    if composer.attachment_target(cx) != Some(target) {
                        composer.attachment_upload.cancel();
                        cx.notify();
                        return false;
                    }
                    match result {
                        Ok(uploaded) => composer.editor.update(cx, |editor, cx| {
                            editor.insert_attachment(&uploaded.markdown, cx)
                        }),
                        Err(key) => {
                            composer
                                .attachment_upload
                                .failures
                                .push(Failure { path, key, target })
                        }
                    }
                    cx.notify();
                    true
                });
                if !current.unwrap_or(false) {
                    return;
                }
            }
            let _ = entity.update(cx, |composer, cx| {
                if composer
                    .attachment_upload
                    .pending
                    .as_ref()
                    .map(|pending| pending.id)
                    == Some(id)
                {
                    composer.attachment_upload.pending.take();
                    cx.notify();
                }
            });
        });
        self.attachment_upload.pending = Some(Pending {
            id,
            target,
            cancel: Some(cancel),
            _task: task,
        });
        cx.notify();
    }
}

impl AttachmentSource {
    /// Возвращает путь, который можно повторно открыть при явном повторе.
    fn path(&self) -> Option<&Path> {
        match self {
            Self::Path(path) => Some(path),
            Self::Clipboard(_) => None,
        }
    }

    /// Превращает один выбранный источник в ограниченный по размеру снимок файла.
    fn load(self) -> Result<AttachmentUpload, &'static str> {
        match self {
            Self::Path(path) => read_attachment(&path),
            Self::Clipboard(image) => read_clipboard_image(&image),
        }
    }
}

/// Ставит состояние или имя рядом с кнопкой действия над файлом.
fn attachment_row(palette: Palette, title: impl Into<gpui::SharedString>) -> Div {
    div()
        .flex()
        .min_w_0()
        .items_center()
        .gap_2()
        .child(Icon::Attach.element(px(16.), palette.icon))
        .child(
            label(palette, TextRole::Caption, title)
                .flex_1()
                .min_w_0()
                .truncate(),
        )
}

/// Читает обычный непустой файл и не выделяет память сверх лимита загрузки.
fn read_attachment(path: &Path) -> Result<AttachmentUpload, &'static str> {
    const READ_FAILED: &str = "desktop.messenger.composer.attachmentOpenFailed";
    const TOO_LARGE: &str = "desktop.messenger.composer.attachmentTooLarge";
    let name = path
        .file_name()
        .ok_or(READ_FAILED)?
        .to_string_lossy()
        .into_owned();
    let file = File::open(path).map_err(|_| READ_FAILED)?;
    let metadata = file.metadata().map_err(|_| READ_FAILED)?;
    if !metadata.is_file() {
        return Err(READ_FAILED);
    }
    let max = MAX_ATTACHMENT_UPLOAD_BYTES as u64;
    if metadata.len() > max {
        return Err(TOO_LARGE);
    }
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| READ_FAILED)?;
    if bytes.len() as u64 > max {
        return Err(TOO_LARGE);
    }
    if bytes.is_empty() {
        return Err("desktop.messenger.composer.attachmentEmpty");
    }
    let (content_type, dimensions) = detect_attachment(path, &bytes);
    Ok(AttachmentUpload {
        name,
        content_type: content_type.to_owned(),
        bytes,
        dimensions,
    })
}

/// Даёт вставленному изображению имя и сохраняет MIME системного буфера.
fn read_clipboard_image(image: &Image) -> Result<AttachmentUpload, &'static str> {
    let bytes = image.bytes();
    if bytes.len() > MAX_ATTACHMENT_UPLOAD_BYTES {
        return Err("desktop.messenger.composer.attachmentTooLarge");
    }
    if bytes.is_empty() {
        return Err("desktop.messenger.composer.attachmentEmpty");
    }
    let content_type = image.format().mime_type();
    let extension = match content_type {
        "image/jpeg" => "jpg",
        "image/svg+xml" => "svg",
        _ => content_type.strip_prefix("image/").unwrap_or("png"),
    };
    let fingerprint = Uuid::new_v4().simple().to_string();
    let name = format!("pasted-image-{}.{}", &fingerprint[..8], extension);
    let (_, dimensions) = detect_attachment(Path::new(&name), bytes);
    Ok(AttachmentUpload {
        name,
        content_type: content_type.to_owned(),
        bytes: bytes.to_vec(),
        dimensions,
    })
}

/// Получает подсказку MIME, а у поддерживаемого изображения читает только размеры.
fn detect_attachment(path: &Path, bytes: &[u8]) -> (&'static str, Option<(u32, u32)>) {
    if let Ok(format) = image::guess_format(bytes) {
        let dimensions = image::ImageReader::with_format(Cursor::new(bytes), format)
            .into_dimensions()
            .ok();
        return (format.to_mime_type(), dimensions);
    }
    if bytes.starts_with(b"%PDF-") {
        return ("application/pdf", None);
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    let mime = match extension.as_deref() {
        Some("heic") => "image/heic",
        Some("heif") => "image/heif",
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("txt" | "md") => "text/plain",
        _ => "application/octet-stream",
    };
    (mime, None)
}

/// Отличает неполученную квитанцию от явного отказа и локального сбоя.
fn upload_error(error: UploadAttachmentError) -> &'static str {
    match error {
        UploadAttachmentError::TooLarge => "desktop.messenger.composer.attachmentTooLarge",
        UploadAttachmentError::Empty => "desktop.messenger.composer.attachmentEmpty",
        UploadAttachmentError::Unknown => "desktop.messenger.composer.attachmentUnknown",
        _ => "desktop.messenger.composer.attachmentFailed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет, что диск и буфер дают одинаковые байты, MIME и размеры изображения.
    #[test]
    fn composer_attachment_sources_keep_image_bytes() {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 3)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let image = Image::from_bytes(gpui::ImageFormat::Png, bytes.into_inner());
        let pasted = read_clipboard_image(&image).unwrap();
        assert_eq!(pasted.bytes, image.bytes());
        assert_eq!(pasted.content_type, "image/png");
        assert_eq!(pasted.dimensions, Some((2, 3)));
        assert!(pasted.name.ends_with(".png"));

        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("кадр");
        std::fs::write(&file, image.bytes()).unwrap();
        let file = read_attachment(&file).unwrap();
        assert_eq!(file.name, "кадр");
        assert_eq!(file.bytes, pasted.bytes);
        assert_eq!(file.content_type, pasted.content_type);
        assert_eq!(file.dimensions, pasted.dimensions);
    }
}
