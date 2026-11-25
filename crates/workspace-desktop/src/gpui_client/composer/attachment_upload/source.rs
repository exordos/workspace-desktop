//! Читает выбранный файл или изображение буфера за пределами потока интерфейса.

use std::{
    fs::{Metadata, OpenOptions},
    io::{Cursor, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use gpui::Image;
use uuid::Uuid;
use workspace_core::messenger::files::{AttachmentUpload, MAX_ATTACHMENT_UPLOAD_BYTES};

use crate::platform::media;

/// Источник файла для первой попытки и повтора после сетевой ошибки.
#[derive(Clone)]
pub(super) enum Source {
    Path {
        path: PathBuf, // Пользовательский путь остаётся в Desktop.
        fingerprint: Arc<OnceLock<FileFingerprint>>, // Первая версия файла для всех попыток карточки.
    },
    Clipboard {
        image: Arc<Image>, // Снимок изображения, чтобы повтор не читал новый буфер обмена.
        name: String,      // Одинаковое имя у всех попыток одной карточки.
    },
}

/// Версия локального файла, которая обнаруживает замену и изменение байтов во время чтения.
#[derive(PartialEq, Eq)]
pub(super) struct FileFingerprint {
    device: u64,         // Диск, на котором находится файл.
    inode: u64,          // Идентификатор записи на диске.
    bytes: u64,          // Размер файла до чтения.
    edited: (i64, i64),  // Время записи содержимого в секундах и наносекундах.
    changed: (i64, i64), // Время изменения записи файловой системы в секундах и наносекундах.
}

impl FileFingerprint {
    /// Снимает признаки версии с пути или открытого дескриптора.
    fn new(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            edited: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

impl Source {
    /// Заводит первое чтение и все повторы одного выбранного пути.
    pub(super) fn path(path: PathBuf) -> Self {
        Self::Path {
            path,
            fingerprint: Arc::new(OnceLock::new()),
        }
    }

    /// Присваивает изображению из буфера имя при добавлении карточки.
    pub(super) fn clipboard(image: Arc<Image>) -> Self {
        let name = clipboard_name(&image);
        Self::Clipboard { image, name }
    }

    /// Возвращает имя карточки до чтения байтов с диска.
    pub(super) fn name(&self) -> String {
        match self {
            Self::Path { path, .. } => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Self::Clipboard { name, .. } => name.clone(),
        }
    }

    /// Читает снимок файла в рабочем потоке.
    pub(super) fn load(self) -> Result<AttachmentUpload, &'static str> {
        match self {
            Self::Path { path, fingerprint } => read_attachment(&path, &fingerprint),
            Self::Clipboard { image, name } => read_clipboard_image(&image, name),
        }
    }
}

/// Читает обычный непустой файл и не выделяет память сверх лимита загрузки.
fn read_attachment(
    path: &Path,
    fingerprint: &OnceLock<FileFingerprint>,
) -> Result<AttachmentUpload, &'static str> {
    const CHANGED: &str = "desktop.messenger.composer.attachmentChanged";
    const READ_FAILED: &str = "desktop.messenger.composer.attachmentOpenFailed";
    const TOO_LARGE: &str = "desktop.messenger.composer.attachmentTooLarge";
    let name = path
        .file_name()
        .ok_or(READ_FAILED)?
        .to_string_lossy()
        .into_owned();
    let metadata = std::fs::metadata(path).map_err(|_| READ_FAILED)?;
    if !metadata.is_file() {
        return Err(READ_FAILED);
    }
    let max = MAX_ATTACHMENT_UPLOAD_BYTES as u64;
    if metadata.len() > max {
        return Err(TOO_LARGE);
    }
    // Подмена пути между проверкой и open не должна остановить поток на FIFO.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| READ_FAILED)?;
    let opened = file.metadata().map_err(|_| READ_FAILED)?;
    let current = FileFingerprint::new(&metadata);
    if !opened.is_file() || FileFingerprint::new(&opened) != current {
        return Err(CHANGED);
    }
    let expected = fingerprint.get_or_init(|| current);
    if *expected != FileFingerprint::new(&opened) {
        return Err(CHANGED);
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| READ_FAILED)?;
    if bytes.len() as u64 > max {
        return Err(TOO_LARGE);
    }
    let read = file.metadata().map_err(|_| READ_FAILED)?;
    if bytes.len() as u64 != expected.bytes || *expected != FileFingerprint::new(&read) {
        return Err(CHANGED);
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

/// Даёт вставленному изображению имя с расширением формата системного буфера.
fn clipboard_name(image: &Image) -> String {
    let content_type = image.format().mime_type();
    let extension = match content_type {
        "image/jpeg" => "jpg",
        "image/svg+xml" => "svg",
        _ => content_type.strip_prefix("image/").unwrap_or("png"),
    };
    let fingerprint = Uuid::new_v4().simple().to_string();
    format!("pasted-image-{}.{}", &fingerprint[..8], extension)
}

/// Сохраняет байты и MIME снимка буфера обмена.
fn read_clipboard_image(image: &Image, name: String) -> Result<AttachmentUpload, &'static str> {
    let bytes = image.bytes();
    if bytes.len() > MAX_ATTACHMENT_UPLOAD_BYTES {
        return Err("desktop.messenger.composer.attachmentTooLarge");
    }
    if bytes.is_empty() {
        return Err("desktop.messenger.composer.attachmentEmpty");
    }
    let content_type = image.format().mime_type();
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
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    let declared_image = match extension.as_deref() {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    };
    if let Ok(format) = image::guess_format(bytes) {
        let dimensions = image::ImageReader::with_format(Cursor::new(bytes), format)
            .into_dimensions()
            .ok();
        return (declared_image.unwrap_or(format.to_mime_type()), dimensions);
    }
    if let Some(declared_image) = declared_image {
        // Core проверит сигнатуру по объявленному типу так же, как браузерный редактор.
        return (declared_image, None);
    }
    if let Some(mime) = media::heif_mime_type(bytes) {
        return (mime, None);
    }
    if bytes.starts_with(b"%PDF-") {
        return ("application/pdf", None);
    }
    let mime = match extension.as_deref() {
        Some("heic") => "image/heic",
        Some("heif") => "image/heif",
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("avi") => "video/x-msvideo",
        Some("mkv") => "video/x-matroska",
        Some("mp3") => "audio/mpeg",
        Some("m4a") => "audio/mp4",
        Some("ogg" | "oga") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("txt" | "md") => "text/plain",
        Some("svg") => "image/svg+xml",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("zip") => "application/zip",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    };
    let dimensions = if mime.starts_with("video/") {
        media::video_dimensions(bytes)
    } else {
        None
    };
    (mime, dimensions)
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
        let pasted = read_clipboard_image(&image, clipboard_name(&image)).unwrap();
        assert_eq!(pasted.bytes, image.bytes());
        assert_eq!(pasted.content_type, "image/png");
        assert_eq!(pasted.dimensions, Some((2, 3)));
        assert!(pasted.name.ends_with(".png"));

        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("кадр");
        std::fs::write(&file, image.bytes()).unwrap();
        let file = Source::path(file).load().unwrap();
        assert_eq!(file.name, "кадр");
        assert_eq!(file.bytes, pasted.bytes);
        assert_eq!(file.content_type, pasted.content_type);
        assert_eq!(file.dimensions, pasted.dimensions);
    }

    /// Повтор загружает тот же файл, пока его версия совпадает с первым чтением.
    #[test]
    fn retry_rejects_replaced_source_file() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("file.txt");
        std::fs::write(&path, b"first").unwrap();
        let source = Source::path(path.clone());
        assert_eq!(source.clone().load().unwrap().bytes, b"first");
        assert_eq!(source.clone().load().unwrap().bytes, b"first");

        let replacement = folder.path().join("replacement.txt");
        std::fs::write(&replacement, b"other").unwrap();
        std::fs::rename(replacement, path).unwrap();
        assert_eq!(
            source.load().err(),
            Some("desktop.messenger.composer.attachmentChanged")
        );
    }
}
