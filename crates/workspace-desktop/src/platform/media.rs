//! Определяет форматы вложений и подключает доступные декодеры видео и изображений.

use std::io::{self, Read, Write};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::{VideoWindow, heif_preview, video_dimensions, video_poster};

#[cfg(not(target_os = "macos"))]
mod portable;
#[cfg(not(target_os = "macos"))]
pub(crate) use portable::{VideoWindow, heif_preview, video_dimensions, video_poster};

/// Копирует проверяемые байты в закрытый файл только на время обработки видео.
pub(crate) fn stage_video(mut source: impl Read) -> io::Result<tempfile::NamedTempFile> {
    let mut prefix = [0; 12];
    source.read_exact(&mut prefix)?;
    // Тип контейнера определяют байты. Имя из сообщения не становится путём на диске.
    let suffix = match &prefix[4..8] {
        b"ftyp" if &prefix[8..12] == b"qt  " => ".mov",
        b"ftyp" => ".mp4",
        b"moov" | b"mdat" | b"wide" => ".mov",
        _ if prefix.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) => ".webm",
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported video container",
            ));
        }
    };
    let mut clip = tempfile::Builder::new().suffix(suffix).tempfile()?;
    clip.write_all(&prefix)?;
    io::copy(&mut source, &mut clip)?;
    Ok(clip)
}

/// Распознаёт по марке контейнера изображения семейства HEIF.
pub(crate) fn heif_mime_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes.get(4..12)? {
        b"ftypheic" | b"ftypheix" | b"ftyphevc" | b"ftyphevx" => Some("image/heic"),
        b"ftypmif1" | b"ftypmsf1" => Some("image/heif"),
        b"ftypavif" | b"ftypavis" => Some("image/avif"),
        _ => None,
    }
}

/// Содержит уменьшенное изображение для последующего кадрирования в GPUI.
pub(crate) struct ImagePreview {
    pub(crate) width: u32,      // Ширина после исправления ориентации, в пикселях.
    pub(crate) height: u32,     // Высота после исправления ориентации, в пикселях.
    pub(crate) pixels: Vec<u8>, // Последовательность RGBA, начиная с верхней строки.
}
