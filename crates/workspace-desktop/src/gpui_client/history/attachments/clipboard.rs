//! Копирует фото из подтверждённой строки через тот же допуск и декодер, что и просмотр.

use std::io::Cursor;

use gpui::{ClipboardItem, Context, Div, Image, ImageFormat, Task};

use super::{AttachmentKey, Conversation, PictureError, decode_original};
use crate::gpui_client::{
    preferences::Language,
    ui::{Button, ConversationNotice, Palette},
};

/// Удерживает последний запрос и его видимую ошибку в одном разговоре.
#[derive(Default)]
pub(super) struct CopyState {
    task: Option<(AttachmentKey, Task<()>)>, // Источник и ожидание последнего копирования.
    error: Option<(AttachmentKey, PictureError)>, // Ошибка видна, пока исходная карточка в истории.
}

impl CopyState {
    /// Отменяет ожидание и очищает ошибку, если исходное вложение потеряло доступ.
    pub(super) fn invalidate(&mut self, affected: impl Fn(AttachmentKey) -> bool) {
        if self.task.as_ref().is_some_and(|(key, _)| affected(*key)) {
            self.task = None;
        }
        if self.error.is_some_and(|(key, _)| affected(key)) {
            self.error = None;
        }
    }
}

impl Conversation {
    /// Проверяет, что исходная карточка ещё доступна в открытой истории.
    pub(in crate::gpui_client::conversation) fn can_copy_attachment(
        &self,
        key: AttachmentKey,
    ) -> bool {
        self.has_access()
            && self.attachments.runtime.is_some()
            && self.attachments.access.is_some()
            && self.viewport.has_attachment(key)
    }

    /// Получает фото Core и пишет PNG в системный буфер под проверкой доступа к файлу.
    pub(in crate::gpui_client::conversation) fn copy_attachment(
        &mut self,
        key: AttachmentKey,
        cx: &mut Context<Self>,
    ) {
        if !self.can_copy_attachment(key) {
            return;
        }
        let (Some(runtime), Some(access)) = (
            self.attachments.runtime.clone(),
            self.attachments.access.clone(),
        ) else {
            return;
        };
        let address = self.address.clone();
        self.attachments.clipboard.error = None;
        let op_id = uuid::Uuid::new_v4();
        log::debug!("gpui_media op_id={op_id} event=clipboard_image_requested");
        let task = cx.spawn(async move |entity, cx| {
            let reply = runtime
                .conversation(address, move |services, _| async move {
                    let mut file = services
                        .messenger
                        .open_media(&services.sync, &access, key.message, key.file)
                        .await
                        .map_err(|_| PictureError::Unavailable)?;
                    tokio::task::spawn_blocking(move || {
                        let photo = decode_original(&mut file)?;
                        let mut output = Cursor::new(Vec::new());
                        photo
                            .write_to(&mut output, image::ImageFormat::Png)
                            .map_err(|_| PictureError::Unsupported)?;
                        let image = Image::from_bytes(ImageFormat::Png, output.into_inner());
                        Ok((file, ClipboardItem::new_image(&image)))
                    })
                    .await
                    .map_err(|_| PictureError::Unavailable)?
                })
                .await;
            let _ = entity.update(cx, |view, cx| {
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                if !view.can_copy_attachment(key) {
                    return;
                }
                let copied = match result {
                    Ok(Ok((file, image))) => file
                        .with_valid_access(|| cx.write_to_clipboard(image))
                        .map_err(|_| PictureError::Unavailable),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err(PictureError::Unavailable),
                };
                log::debug!(
                    "gpui_media op_id={op_id} event=clipboard_image_completed success={}",
                    copied.is_ok()
                );
                view.attachments.clipboard.error = copied.err().map(|error| (key, error));
                cx.notify();
            });
        });
        self.attachments.clipboard.task = Some((key, task));
        cx.notify();
    }

    /// Показывает ошибку копирования рядом с историей, пока доступен её источник.
    pub(in crate::gpui_client::conversation) fn render_attachment_copy_error(
        &self,
        palette: Palette,
        language: Language,
        cx: &Context<Self>,
    ) -> Option<Div> {
        let (key, error) = self.attachments.clipboard.error?;
        if !self.can_copy_attachment(key) {
            return None;
        }
        let view = cx.entity().downgrade();
        Some(
            ConversationNotice {
                text: error.message(language).into(),
                action: Some(Button::new(
                    "history-copy-image-error-close",
                    language.tr("common.close"),
                )),
            }
            .render(palette, move |_, cx| {
                let _ = view.update(cx, |view, cx| {
                    view.attachments.clipboard.error = None;
                    cx.notify();
                });
            }),
        )
    }
}
