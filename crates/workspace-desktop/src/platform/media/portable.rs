//! Возвращает отсутствие декодера для форматов, которые пока обрабатывает AVFoundation.

use std::{io::Read, path::Path};

use super::ImagePreview;

/// Сообщает, что размер видеотрека пока недоступен на этой платформе.
pub(crate) fn video_dimensions(_: impl Read) -> Option<(u32, u32)> {
    None
}

/// Сообщает, что первый кадр пока недоступен на этой платформе.
pub(crate) fn video_poster(_: &Path, _: f64) -> Option<Vec<u8>> {
    None
}

/// Сообщает, что системный декодер HEIF пока не подключён.
pub(crate) fn heif_preview(_: &[u8], _: u32) -> Option<ImagePreview> {
    None
}

/// Место проигрывателя в общем состоянии карточки сообщения.
pub(crate) enum VideoWindow {}

impl VideoWindow {
    /// Возвращает отказ и освобождает временный файл видео.
    pub(crate) fn open(_: tempfile::NamedTempFile, _: &str) -> Option<Self> {
        None
    }

    /// Подтверждает, что в тестовом клиенте проигрыватель недоступен.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(crate) fn is_open(&self) -> bool {
        match *self {}
    }
}
