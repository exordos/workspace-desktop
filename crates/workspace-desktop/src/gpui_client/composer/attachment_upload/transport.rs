//! Читает выбранный файл вне UI, передаёт его в Core и связывает ответ с попыткой карточки.

use futures::{StreamExt, stream};
use gpui::Context;
use tokio::sync::watch;
use uuid::Uuid;
use workspace_core::{
    messenger::files::{
        AttachmentUploadControl, AttachmentUploadProgress, UploadAttachmentError,
        UploadedAttachment,
    },
    storage::HistoryScope,
};

use super::{Composer, MAX_CONCURRENT_UPLOADS, Source, Status, UploadTarget, presentation};
use crate::gpui_client::{
    contracts::{AccountSession, ConversationAddress},
    runtime::RuntimeClient,
};

/// Адрес и параметры одной операции чтения и передачи.
pub(super) struct UploadAttempt {
    pub(super) id: Uuid,             // Карточка, которая ждёт исходный файл.
    pub(super) attempt: Uuid,        // Текущая попытка передачи этого файла.
    pub(super) target: UploadTarget, // Подтверждённая тема и режим редактора.
    pub(super) source: Source,       // Локальный источник байтов.
    pub(super) canceled: watch::Receiver<bool>, // Отменяет операцию при удалении карточки.
    pub(super) runtime: RuntimeClient, // Исполняет запросы в фоне.
    pub(super) address: ConversationAddress, // Исходный вход и область истории.
}

impl UploadAttempt {
    /// Запускает передачу и передаёт квитанцию только живой карточке исходного композера.
    pub(super) fn start(self, cx: &Context<Composer>) {
        let Self {
            id,
            attempt,
            target,
            source,
            canceled,
            runtime,
            address,
        } = self;
        let background = cx.background_executor().clone();
        cx.spawn(async move |entity, cx| {
        let loaded = background.spawn(async move {
            let upload = source.load()?;
            let preview = if upload.content_type.starts_with("image/") {
                presentation::decode_thumbnail(&upload.bytes)
            } else {
                None
            };
            Ok::<_, &'static str>((upload, preview))
        }).await;
        let result = match loaded {
            Ok((upload, preview)) => {
                let content_type = upload.content_type.clone();
                let size = upload.bytes.len() as u64;
                let live = entity.update(cx, |composer, cx| {
                    if composer.attachment_target() != Some(target) {
                        composer.validate_attachment_target(cx);
                        return false;
                    }
                    let Some(slot) = composer.attachment_upload.working(id, attempt) else { return false };
                    slot.release_preview(cx);
                    slot.preview = preview;
                    slot.content_type = Some(content_type);
                    slot.size = Some(size);
                    if let Status::Working { progress, .. } = &mut slot.status {
                        *progress = Some(0);
                    }
                    cx.notify();
                    true
                }).unwrap_or(false);
                if !live {
                    return;
                }
                let (progress_sender, mut progress) = watch::channel(AttachmentUploadProgress::default());
                let control = AttachmentUploadControl {
                    cancel: Some(canceled),
                    progress: Some(progress_sender),
                };
                let account = AccountSession { owner: address.route.owner.clone(), generation: address.generation };
                let activation = runtime.message_encryption(&account).map(|(_, activation)| activation);
                let scope = HistoryScope {
                    stream_id: address.route.scope.stream_id,
                    topic_id: Some(target.topic),
                };
                let call = runtime.conversation(address, move |services, address| async move {
                    if target.encrypted {
                        let activation = activation.ok_or(workspace_core::auth::AuthError::SessionUnavailable)?;
                        services.messenger.upload_encrypted_attachment(
                            &services.sync, &activation, scope, upload, control,
                        ).await
                    } else {
                        services.messenger.upload_attachment_with_control(
                            &services.sync, &address.route.owner, address.generation, scope, upload, control,
                        ).await
                    }
                });
                tokio::pin!(call);
                loop {
                    tokio::select! {
                        reply = &mut call => break reply.value
                            .map_err(|_| ("desktop.messenger.composer.attachmentFailed", true))
                            .and_then(|result| result.map_err(upload_error)),
                        Ok(()) = progress.changed() => {
                            let percent = progress_percent(*progress.borrow_and_update());
                            let _ = entity.update(cx, |composer, cx| {
                                let Some(slot) = composer.attachment_upload.working(id, attempt) else { return };
                                if let Status::Working { progress, .. } = &mut slot.status
                                    && *progress != Some(percent)
                                {
                                    *progress = Some(percent);
                                    cx.notify();
                                }
                            });
                        }
                    }
                }
            }
            Err(key) => Err((key, false)),
        };
        let mut result = Some(result);
        let _ = entity.update(cx, |composer, cx| {
            if composer.attachment_target() != Some(target) {
                composer.validate_attachment_target(cx);
                return;
            }
            let Some(slot) = composer.attachment_upload.working(id, attempt) else { return };
            slot.status = match result.take().expect("результат одной попытки") {
                Ok(receipt) => Status::Ready {
                    file: receipt.id(),
                    markdown: receipt.markdown.clone(),
                    cleanup: Some(receipt),
                },
                Err((key, retryable)) => Status::Error { key, retryable },
            };
            composer.pump_attachments(cx);
            cx.notify();
        });
        if let Some(Ok(receipt)) = result {
            discard_uploaded(&runtime, vec![receipt]).await;
        }
    }).detach();
    }
}

/// Пробует удалить каждый черновой файл в его исходной сессии, не отменяя остальные при отказе.
pub(super) async fn discard_uploaded(runtime: &RuntimeClient, receipts: Vec<UploadedAttachment>) {
    let _ = runtime
        .request(move |services| async move {
            stream::iter(receipts)
                .map(|receipt| {
                    services
                        .messenger
                        .discard_uploaded_attachment(&services.sync, receipt)
                })
                .buffer_unordered(MAX_CONCURRENT_UPLOADS)
                .for_each(|_| std::future::ready(()))
                .await;
        })
        .await;
}

/// Возвращает процент отправленных байтов для одной карточки.
fn progress_percent(progress: AttachmentUploadProgress) -> u8 {
    if progress.total_bytes == 0 {
        return 0;
    }
    ((f64::from(100) * progress.loaded_bytes as f64 / progress.total_bytes as f64)
        .round()
        .clamp(0.0, 100.0)) as u8
}

/// Разделяет ошибку валидации и сбой передачи, который разрешено повторить вручную.
fn upload_error(error: UploadAttachmentError) -> (&'static str, bool) {
    match error {
        UploadAttachmentError::TooLarge => ("desktop.messenger.composer.attachmentTooLarge", false),
        UploadAttachmentError::Empty => ("desktop.messenger.composer.attachmentEmpty", false),
        UploadAttachmentError::InvalidInput => {
            ("desktop.messenger.composer.attachmentInvalidImage", false)
        }
        UploadAttachmentError::Unknown => ("desktop.messenger.composer.attachmentUnknown", true),
        _ => ("desktop.messenger.composer.attachmentFailed", true),
    }
}
