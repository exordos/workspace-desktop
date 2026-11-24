//! Получает первый кадр локального видео и открывает проигрыватель macOS.

use std::{
    cell::RefCell,
    ffi::c_void,
    io::{self, Read, Write},
    path::Path,
    ptr::NonNull,
};

use block2::RcBlock;
use objc2::{
    Encode, Encoding, RefEncode, class, msg_send, msg_send_id,
    rc::{Retained, autoreleasepool},
    runtime::{AnyObject, Bool},
};
use objc2_app_kit::{
    NSBackingStoreType, NSView, NSWindow, NSWindowStyleMask, NSWindowWillCloseNotification,
};
use objc2_foundation::{
    MainThreadMarker, NSData, NSDictionary, NSError, NSNotification, NSNotificationCenter,
    NSNumber, NSObject, NSPoint, NSRect, NSSize, NSString, NSURL,
};

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    static AVURLAssetReferenceRestrictionsKey: &'static NSString;
}

#[link(name = "AVKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageRelease(image: *mut CGImage);
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    static kCGImageSourceCreateThumbnailFromImageAlways: &'static NSString;
    static kCGImageSourceCreateThumbnailWithTransform: &'static NSString;
    static kCGImageSourceThumbnailMaxPixelSize: &'static NSString;
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

/// Уменьшает HEIC или HEIF с учётом ориентации до выделения пикселей исходного размера.
pub(crate) fn heif_preview(bytes: &[u8], max_side: u32) -> Option<Vec<u8>> {
    autoreleasepool(|_| {
        let data = NSData::with_bytes(bytes);
        // NSData и NSDictionary совместимы с одноимёнными объектами Core Foundation.
        unsafe {
            let options = NSDictionary::from_id_slice(
                &[
                    kCGImageSourceCreateThumbnailFromImageAlways,
                    kCGImageSourceCreateThumbnailWithTransform,
                    kCGImageSourceThumbnailMaxPixelSize,
                ],
                &[
                    NSNumber::new_bool(true),
                    NSNumber::new_bool(true),
                    NSNumber::new_u32(max_side),
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
            encode_png(NonNull::new(image)?)
        }
    })
}

/// Забирает владение кадром Core Graphics и возвращает его PNG.
unsafe fn encode_png(image: NonNull<CGImage>) -> Option<Vec<u8>> {
    // Оба вызывающих API передают кадр с правилом Create или Copy.
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
