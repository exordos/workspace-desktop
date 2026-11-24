//! Запускает нативный проигрыватель из действующего сообщения и закрывает его при отзыве файла.

use gpui::{Context, SharedString, Task};
use uuid::Uuid;
use workspace_core::{messenger::files::MediaFile, storage::media::MediaKind};

use super::{
    AttachmentKey, CardAddress, Conversation, PictureError, ProtectedAttachment, inline::copy_video,
};
use crate::platform::media::VideoWindow;

/// Статус команды воспроизведения, видимый на карточке.
#[derive(Clone, Copy)]
pub(super) enum CardState {
    Loading, // Оригинал читается и готовится для AVKit.
    Failed,  // Проигрыватель не удалось запустить.
}

/// Одно ожидающее или открытое видео на разговор.
#[derive(Default)]
pub(super) struct Playback(Option<Entry>); // Закрытие разговора завершает загрузку или воспроизведение.

/// Точный источник текущего клика и его состояние.
struct Entry {
    attempt: Uuid,                       // Отличает ответы последовательных кликов.
    card: CardAddress,                   // Различает исходное сообщение и цитаты.
    holder: Option<ProtectedAttachment>, // Проверка ключей и источника защищённой ссылки.
    status: Status,                      // Запрос, отказ или открытое окно.
}

/// Ресурсы, принадлежащие текущему клику.
enum Status {
    Loading {
        _task: Task<()>, // Удаление задачи отменяет ожидание и сеть.
    },
    Failed, // Карточка показывает ошибку до следующего нажатия.
    Playing {
        access: Box<MediaFile>, // Действующая версия файла и сессия.
        _window: VideoWindow,   // Закрывает окно и останавливает воспроизведение при удалении.
    },
}

impl Playback {
    /// Возвращает подсказку для карточки, открывшей проигрыватель.
    pub(super) fn card_state(&self) -> Option<(CardAddress, CardState)> {
        let entry = self.0.as_ref()?;
        let state = match &entry.status {
            Status::Loading { .. } => CardState::Loading,
            Status::Failed => CardState::Failed,
            Status::Playing { .. } => return None,
        };
        Some((entry.card, state))
    }

    /// Отменяет команды и закрывает окно отозванного или изменённого файла.
    pub(super) fn invalidate(
        &mut self,
        keep: impl Fn(AttachmentKey) -> bool,
        cx: &mut Context<Conversation>,
    ) {
        self.retain(|entry| keep(entry.card.0), cx);
    }

    /// Сверяет обычный источник по открытому окну и защищённый по исходному конверту.
    pub(super) fn retain_sources(
        &mut self,
        ordinary: impl Fn(AttachmentKey) -> bool,
        protected: impl Fn(&ProtectedAttachment) -> bool,
        cx: &mut Context<Conversation>,
    ) {
        self.retain(
            |entry| match &entry.holder {
                Some(holder) => protected(holder),
                None => ordinary(entry.card.0),
            },
            cx,
        );
    }

    /// Закрывает проигрыватель, если отозван ключ, версия файла или сессия.
    pub(super) fn invalidate_revoked(&mut self, cx: &mut Context<Conversation>) {
        self.retain(
            |entry| {
                entry
                    .holder
                    .as_ref()
                    .is_none_or(|holder| holder.with_current(|| ()).is_some())
                    && match &entry.status {
                        Status::Playing { access, .. } => access.with_valid_access(|| ()).is_ok(),
                        _ => true,
                    }
            },
            cx,
        );
    }

    /// Снимает устаревшее состояние и обновляет подсказку на карточке.
    fn retain(&mut self, keep: impl FnOnce(&Entry) -> bool, cx: &mut Context<Conversation>) {
        if self.0.as_ref().is_some_and(|entry| !keep(entry)) {
            self.0.take();
            cx.notify();
        }
    }
}

impl Conversation {
    /// Проверяет, что после нажатия файл передан проигрывателю.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_video_open(&self) -> bool {
        self.attachments.video.0.as_ref().is_some_and(|entry| {
            matches!(&entry.status, Status::Playing { _window: window, .. } if window.is_open())
        })
    }

    /// Проверяет источник в актуальном окне и готовит авторизованный оригинал на рабочем потоке.
    pub(super) fn open_history_video(
        &mut self,
        key: AttachmentKey,
        name: SharedString,
        holder: Option<ProtectedAttachment>,
        cx: &mut Context<Self>,
    ) {
        let activation = if let Some(holder) = &holder {
            let Some(encryption) = self.encryption.as_ref().filter(|_| holder.is_mounted(self))
            else {
                return;
            };
            Some(encryption.activation.clone())
        } else {
            if !self.has_access()
                || self
                    .viewport
                    .attachment(key)
                    .is_none_or(|reference| reference.kind != MediaKind::Video)
            {
                return;
            }
            None
        };
        let (Some(runtime), Some(access)) = (
            self.attachments.runtime.clone(),
            self.attachments.access.clone(),
        ) else {
            return;
        };
        let playback = &mut self.attachments.video;
        let card = holder
            .as_ref()
            .map_or((key, None), ProtectedAttachment::card_address);
        if matches!(playback.card_state(), Some((pending, CardState::Loading)) if pending == card) {
            return;
        }
        let address = self.address.clone();
        let attempt = Uuid::new_v4();
        let source = holder.clone().zip(activation);
        let task = cx.spawn(async move |target, cx| {
            let reply = runtime
                .conversation(address, move |services, _| async move {
                    let mut file = match source {
                        Some((holder, activation)) => {
                            holder.open(&services, &access, &activation).await
                        }
                        None => {
                            services
                                .messenger
                                .open_media(&services.sync, &access, key.message, key.file)
                                .await
                        }
                    }
                    .map_err(|_| PictureError::Unavailable)?;
                    tokio::task::spawn_blocking(move || {
                        copy_video(&mut file).map(|clip| (file, clip))
                    })
                    .await
                    .map_err(|_| PictureError::Unavailable)?
                })
                .await;
            let _ = target.update(cx, |view, cx| {
                let Some(entry) = view
                    .attachments
                    .video
                    .0
                    .as_ref()
                    .filter(|entry| entry.attempt == attempt)
                else {
                    return;
                };
                let mounted = match &entry.holder {
                    Some(holder) => holder.is_mounted(view),
                    None => view.has_access() && view.viewport.has_attachment(key),
                };
                if !mounted {
                    view.attachments.video.0.take();
                    cx.notify();
                    return;
                }
                let status = reply
                    .for_current(Some(&view.address))
                    .and_then(Result::ok)
                    .and_then(Result::ok)
                    .and_then(|(file, clip)| {
                        file.with_valid_access(|| ()).ok()?;
                        let window = VideoWindow::open(clip, name.as_ref())?;
                        Some(Status::Playing {
                            access: Box::new(file),
                            _window: window,
                        })
                    })
                    .unwrap_or(Status::Failed);
                if let Some(entry) = &mut view.attachments.video.0 {
                    entry.status = status;
                }
                cx.notify();
            });
        });
        playback.0 = Some(Entry {
            attempt,
            card,
            holder,
            status: Status::Loading { _task: task },
        });
        cx.notify();
    }
}
