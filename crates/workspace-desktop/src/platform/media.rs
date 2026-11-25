//! Готовит уменьшенные изображения и первые кадры видео, открывает проигрыватель macOS.

use std::{
    cell::RefCell,
    ffi::c_void,
    io::{self, Read, Write},
    path::Path,
    ptr::NonNull,
};

use block2::RcBlock;
use objc2::{
    ClassType, Encode, Encoding, RefEncode, class, msg_send, msg_send_id,
    rc::{Retained, autoreleasepool},
    runtime::{AnyObject, Bool},
};
use objc2_app_kit::{
    NSBackingStoreType, NSView, NSWindow, NSWindowStyleMask, NSWindowWillCloseNotification,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSData, NSDictionary, NSError, NSNotification, NSNotificationCenter,
    NSNumber, NSObject, NSPoint, NSRect, NSSize, NSString, NSURL,
};

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    static AVURLAssetReferenceRestrictionsKey: &'static NSString;
    static AVMediaTypeVideo: &'static NSString;
}

#[link(name = "AVKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGColorSpaceSRGB: &'static NSString;
    fn CGImageRelease(image: *mut CGImage);
    fn CGImageGetWidth(image: *const CGImage) -> usize;
    fn CGImageGetHeight(image: *const CGImage) -> usize;
    fn CGColorSpaceCreateWithName(name: *const NSString) -> *mut c_void;
    fn CGColorSpaceRelease(space: *mut c_void);
    fn CGBitmapContextCreate(
        pixels: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        color_space: *mut c_void,
        bitmap_info: u32,
    ) -> *mut c_void;
    fn CGContextDrawImage(context: *mut c_void, rect: NSRect, image: *mut CGImage);
    fn CGContextRelease(context: *mut c_void);
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    static kCGImageSourceCreateThumbnailFromImageAlways: &'static NSString;
    static kCGImageSourceCreateThumbnailWithTransform: &'static NSString;
    static kCGImageSourceThumbnailMaxPixelSize: &'static NSString;
    static kCGImageSourceShouldCacheImmediately: &'static NSString;
    fn CGImageSourceCreateWithData(data: *const c_void, options: *const c_void) -> *mut c_void;
    fn CGImageSourceCreateThumbnailAtIndex(
        source: *mut c_void,
        index: usize,
        options: *const c_void,
    ) -> *mut CGImage;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *mut c_void);
}

/// Время CoreMedia. Порядок полей совпадает с CMTime в SDK.
#[derive(Clone, Copy)]
#[repr(C)]
struct MediaTime {
    value: i64,     // Числитель времени.
    timescale: i32, // Число единиц в секунде.
    flags: u32,     // Флаги корректности значения.
    epoch: i64,     // Ноль для начала шкалы файла.
}

// Objective-C передаёт структуру по значению в copyCGImageAtTime.
unsafe impl Encode for MediaTime {
    const ENCODING: Encoding = Encoding::Struct(
        "?",
        &[i64::ENCODING, i32::ENCODING, u32::ENCODING, i64::ENCODING],
    );
}

// Для optional-выхода actualTime передаётся указатель на тот же тип.
unsafe impl RefEncode for MediaTime {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

/// Преобразование кадра из CGAffineTransform, в том же порядке полей, что и в SDK.
#[repr(C)]
struct VideoTransform {
    a: f64,   // Вклад исходной ширины в ширину отображения.
    b: f64,   // Вклад исходной ширины в высоту отображения.
    c: f64,   // Вклад исходной высоты в ширину отображения.
    d: f64,   // Вклад исходной высоты в высоту отображения.
    _tx: f64, // Сдвиг по горизонтали не меняет размер.
    _ty: f64, // Сдвиг по вертикали не меняет размер.
}

// CGFloat на поддерживаемой 64-разрядной macOS имеет тот же вид, что и f64.
unsafe impl Encode for VideoTransform {
    const ENCODING: Encoding = Encoding::Struct("CGAffineTransform", &[f64::ENCODING; 6]);
}

/// Непрозрачный кадр Core Graphics. Используется только через указатель SDK.
#[repr(C)]
struct CGImage {
    _private: [u8; 0], // Содержимое читает и освобождает Core Graphics.
}

// Objective-C проверяет имя непрозрачного типа при вызове AVFoundation и AppKit.
unsafe impl RefEncode for CGImage {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGImage", &[]));
}

/// Открывает только локальный контейнер без ссылок на другие файлы или сеть.
fn local_asset(path: &Path) -> Option<Retained<AnyObject>> {
    let path = NSString::from_str(path.to_str()?);
    // URL строится из закрытой временной копии, созданной приложением.
    unsafe {
        let url = NSURL::fileURLWithPath(&path);
        let options = NSDictionary::from_id_slice(
            &[AVURLAssetReferenceRestrictionsKey],
            &[NSNumber::new_usize(0xffff)],
        );
        msg_send_id![class!(AVURLAsset), URLAssetWithURL: &*url, options: &*options]
    }
}

/// Копирует проверяемые байты в закрытый файл только на время работы AVFoundation.
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

/// Читает ширину и высоту первого видеотрека в пикселях с учётом поворота кадра.
/// Вызывается на рабочем потоке с теми же байтами, которые будут отправлены.
pub(crate) fn video_dimensions(source: impl Read) -> Option<(u32, u32)> {
    let file = stage_video(source).ok()?;
    autoreleasepool(|_| {
        let asset = local_asset(file.path())?;
        // Синхронный интерфейс чтения дорожки доступен и в macOS 11.
        unsafe {
            let tracks: Retained<NSArray<AnyObject>> =
                msg_send_id![&*asset, tracksWithMediaType: AVMediaTypeVideo];
            let track = tracks.firstObject()?;
            let size: NSSize = msg_send![&*track, naturalSize];
            let transform: VideoTransform = msg_send![&*track, preferredTransform];
            displayed_video_dimensions(size, transform)
        }
    })
}

/// Возвращает размер прямоугольника, содержащего весь кадр после поворота.
fn displayed_video_dimensions(size: NSSize, transform: VideoTransform) -> Option<(u32, u32)> {
    let width = (transform.a.abs() * size.width + transform.c.abs() * size.height).round();
    let height = (transform.b.abs() * size.width + transform.d.abs() * size.height).round();
    let valid = 1.0..=f64::from(u32::MAX);
    (valid.contains(&width) && valid.contains(&height)).then_some((width as u32, height as u32))
}

/// Возвращает уменьшенный PNG первого кадра. Вызывается на рабочем потоке.
pub(crate) fn video_poster(path: &Path, max_side: f64) -> Option<Vec<u8>> {
    autoreleasepool(|_| {
        let asset = local_asset(path)?;
        // У AVFoundation нет используемой здесь версии типизированных обёрток.
        // Сигнатуры, владение результатами и типы взяты из заголовков macOS SDK.
        unsafe {
            let generator: Retained<AnyObject> =
                msg_send_id![class!(AVAssetImageGenerator), assetImageGeneratorWithAsset: &*asset];
            let _: () = msg_send![&*generator, setAppliesPreferredTrackTransform: Bool::YES];
            let _: () = msg_send![&*generator, setMaximumSize: NSSize::new(max_side, max_side)];
            let start = MediaTime {
                value: 0,
                timescale: 1,
                flags: 1,
                epoch: 0,
            };
            let image: *mut CGImage = msg_send![
                &*generator,
                copyCGImageAtTime: start,
                actualTime: std::ptr::null_mut::<MediaTime>(),
                error: std::ptr::null_mut::<*mut NSError>()
            ];
            encode_png(NonNull::new(image)?)
        }
    })
}

/// Распознаёт по марке контейнера изображения, которые читает ImageIO.
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

/// Уменьшает HEIF-совместимый файл с учётом ориентации до выделения пикселей исходного размера.
pub(crate) fn heif_preview(bytes: &[u8], max_side: u32) -> Option<ImagePreview> {
    autoreleasepool(|_| {
        // NSData и NSDictionary совместимы с одноимёнными объектами Core Foundation.
        unsafe {
            // NSData читает заимствованные байты до конца синхронного вызова и не освобождает их.
            let data = NSData::initWithBytesNoCopy_length_freeWhenDone(
                NSData::alloc(),
                NonNull::from(bytes.first()?).cast(),
                bytes.len(),
                false,
            );
            let options = NSDictionary::from_id_slice(
                &[
                    kCGImageSourceCreateThumbnailFromImageAlways,
                    kCGImageSourceCreateThumbnailWithTransform,
                    kCGImageSourceThumbnailMaxPixelSize,
                    kCGImageSourceShouldCacheImmediately,
                ],
                &[
                    NSNumber::new_bool(true),
                    NSNumber::new_bool(true),
                    NSNumber::new_u32(max_side),
                    NSNumber::new_bool(true),
                ],
            );
            let source = NonNull::new(CGImageSourceCreateWithData(
                Retained::as_ptr(&data).cast(),
                std::ptr::null(),
            ))?;
            let image = CGImageSourceCreateThumbnailAtIndex(
                source.as_ptr(),
                0,
                Retained::as_ptr(&options).cast(),
            );
            CFRelease(source.as_ptr());
            let image = NonNull::new(image)?;
            let preview = image_preview(image, max_side);
            CGImageRelease(image.as_ptr());
            preview
        }
    })
}

/// Копирует только уменьшенные пиксели Core Graphics, сохраняя цвет и прозрачность.
unsafe fn image_preview(image: NonNull<CGImage>, max_side: u32) -> Option<ImagePreview> {
    // Эти флаги из CGBitmapContext задают порядок байтов RGBA с умножением цвета на альфу.
    const RGBA_PREMULTIPLIED: u32 = (4 << 12) | 1;
    unsafe {
        let width = CGImageGetWidth(image.as_ptr());
        let height = CGImageGetHeight(image.as_ptr());
        if width == 0 || height == 0 || width.max(height) > max_side as usize {
            return None;
        }
        let stride = width.checked_mul(4)?;
        let mut pixels = vec![0_u8; stride.checked_mul(height)?];
        let space = NonNull::new(CGColorSpaceCreateWithName(kCGColorSpaceSRGB))?;
        // Буфер остаётся на месте до освобождения CGContext.
        let context = CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            width,
            height,
            8,
            stride,
            space.as_ptr(),
            RGBA_PREMULTIPLIED,
        );
        CGColorSpaceRelease(space.as_ptr());
        let context = NonNull::new(context)?;
        let bounds = NSRect::new(
            NSPoint::new(0., 0.),
            NSSize::new(width as f64, height as f64),
        );
        CGContextDrawImage(context.as_ptr(), bounds, image.as_ptr());
        CGContextRelease(context.as_ptr());
        for pixel in pixels.as_chunks_mut::<4>().0 {
            let alpha = u16::from(pixel[3]);
            if (1..255).contains(&alpha) {
                for channel in &mut pixel[..3] {
                    let original = (u16::from(*channel) * 255 + alpha / 2) / alpha;
                    *channel = original.min(255) as u8;
                }
            }
        }
        Some(ImagePreview {
            width: width as u32,
            height: height as u32,
            pixels,
        })
    }
}

/// Забирает владение кадром Core Graphics и возвращает его PNG.
unsafe fn encode_png(image: NonNull<CGImage>) -> Option<Vec<u8>> {
    // AVFoundation передаёт кадр по правилу Copy.
    unsafe {
        let bitmap: Option<Retained<AnyObject>> = msg_send_id![msg_send_id![class!(NSBitmapImageRep), alloc], initWithCGImage: image.as_ptr()];
        CGImageRelease(image.as_ptr());
        let bitmap = bitmap?;
        let properties: Retained<NSDictionary> = NSDictionary::new();
        let png: Option<Retained<NSData>> =
            msg_send_id![&*bitmap, representationUsingType: 4_usize, properties: &*properties];
        png.map(|png| png.bytes().to_vec())
    }
}

/// Окно воспроизведения и закрытая копия, которую AVKit читает до конца просмотра.
pub(crate) struct VideoWindow {
    native: Option<NativeVideoWindow>, // Отсутствует только в эмулируемом окне GPUI.
    #[cfg(all(test, feature = "gpui-test"))]
    _test_file: Option<tempfile::NamedTempFile>, // Тест держит копию до конца просмотра.
}

/// Ресурсы настоящего проигрывателя на главном потоке.
struct NativeVideoWindow {
    window: Retained<NSWindow>, // Нативное окно с кнопками, звуком и перемоткой.
    center: Retained<NSNotificationCenter>, // Источник события закрытия окна.
    observer: Retained<NSObject>, // Подписка останавливает воспроизведение и удаляет копию.
}

impl VideoWindow {
    /// Показывает проигрыватель Workspace только после явного нажатия.
    pub(crate) fn open(file: tempfile::NamedTempFile, title: &str) -> Option<Self> {
        #[cfg(all(test, feature = "gpui-test"))]
        if MainThreadMarker::new().is_none() {
            return Some(Self {
                native: None,
                _test_file: Some(file),
            });
        }
        Some(Self {
            native: Some(NativeVideoWindow::open(file, title)?),
            #[cfg(all(test, feature = "gpui-test"))]
            _test_file: None,
        })
    }

    /// Проверяет состояние проигрывателя при работе через тестовое окно GPUI.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(crate) fn is_open(&self) -> bool {
        self.native
            .as_ref()
            .is_none_or(|native| native.window.isVisible())
    }
}

impl Drop for VideoWindow {
    /// Закрывает AppKit-окно до освобождения действующего просмотра.
    fn drop(&mut self) {
        self.native.take();
    }
}

impl NativeVideoWindow {
    /// Создаёт окно со стандартными контролами AVKit на главном потоке macOS.
    fn open(file: tempfile::NamedTempFile, title: &str) -> Option<Self> {
        let main = MainThreadMarker::new()?;
        let asset = local_asset(file.path())?;
        let frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(720., 480.));
        // AVPlayerView наследуется от NSView. Все вызовы для окна идут с главного потока.
        unsafe {
            let item: Retained<AnyObject> =
                msg_send_id![class!(AVPlayerItem), playerItemWithAsset: &*asset];
            let player: Retained<AnyObject> =
                msg_send_id![class!(AVPlayer), playerWithPlayerItem: &*item];
            let view: Retained<NSView> =
                msg_send_id![msg_send_id![class!(AVPlayerView), alloc], initWithFrame: frame];
            let _: () = msg_send![&*view, setPlayer: &*player];
            let window = NSWindow::initWithContentRect_styleMask_backing_defer(
                main.alloc(),
                frame,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Resizable
                    | NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::NSBackingStoreBuffered,
                false,
            );
            window.setReleasedWhenClosed(false);
            window.setTitle(&NSString::from_str(title));
            window.setContentView(Some(&view));
            let center = NSNotificationCenter::defaultCenter();
            let file = RefCell::new(Some(file));
            let callback = RcBlock::new({
                let player = player.clone();
                move |_: NonNull<NSNotification>| {
                    // NSWindow публикует событие закрытия в главном потоке.
                    let _: () = msg_send![&*player, pause];
                    let _: () = msg_send![&*view, setPlayer: None::<&AnyObject>];
                    let _: () =
                        msg_send![&*player, replaceCurrentItemWithPlayerItem: None::<&AnyObject>];
                    file.borrow_mut().take();
                }
            });
            let observer = center.addObserverForName_object_queue_usingBlock(
                Some(NSWindowWillCloseNotification),
                Some(&window),
                None,
                &callback,
            );
            window.center();
            window.makeKeyAndOrderFront(None);
            let _: () = msg_send![&*player, play];
            Some(Self {
                window,
                center,
                observer,
            })
        }
    }
}

impl Drop for NativeVideoWindow {
    /// Останавливает звук и отдаёт AVKit последний дескриптор перед удалением файла.
    fn drop(&mut self) {
        self.window.close();
        unsafe { self.center.removeObserver(&self.observer) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Читает реальные метаданные короткого локального видео через AVFoundation.
    #[test]
    fn video_dimensions_reads_first_local_track() {
        let bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/media/synthetic-video.mov"
        ));
        assert_eq!(video_dimensions(bytes.as_slice()), Some((32, 16)));
    }

    /// При повороте на 90 градусов ширина и высота кадра меняются местами.
    #[test]
    fn video_dimensions_respect_track_rotation() {
        let rotation = VideoTransform {
            a: 0.,
            b: 1.,
            c: -1.,
            d: 0.,
            _tx: 16.,
            _ty: 0.,
        };
        assert_eq!(
            displayed_video_dimensions(NSSize::new(32., 16.), rotation),
            Some((16, 32))
        );
    }
}
