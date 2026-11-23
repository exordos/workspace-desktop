//! Декодирует загруженный аватар в ограниченный по размеру набор кадров GPUI.

use std::io::Cursor;

use gpui::RenderImage;
use image::{
    AnimationDecoder, Frame, ImageDecoder, ImageFormat, ImageReader, Limits, RgbaImage,
    codecs::gif::GifDecoder, imageops,
};
use workspace_core::directory::{AvatarFormat, AvatarImage};

const MAX_SOURCE_SIDE: u32 = 4096;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RENDER_SIDE: u32 = 256;
pub(super) const MAX_ANIMATED_BYTES: u32 = 4 * 1024 * 1024;
pub(super) const MAX_STILL_BYTES: u32 = MAX_RENDER_SIDE * MAX_RENDER_SIDE * 4;
const MAX_ANIMATION_FRAMES: usize = 96;

/// Готовит BGRA-кадры и оставляет заглушку при повреждённом или слишком большом исходнике.
pub(in crate::gpui_client) fn decode(avatar: &AvatarImage, animated: bool) -> Option<RenderImage> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(MAX_SOURCE_BYTES);
    let reader = Cursor::new(avatar.bytes.as_slice());
    let format = match avatar.format {
        AvatarFormat::Gif if animated => return decode_gif(reader, limits),
        AvatarFormat::Gif => ImageFormat::Gif,
        AvatarFormat::Png => ImageFormat::Png,
        AvatarFormat::Jpeg => ImageFormat::Jpeg,
        AvatarFormat::Webp => ImageFormat::WebP,
    };
    let mut reader = ImageReader::with_format(reader, format);
    reader.limits(limits);
    let buffer = thumbnail(reader.decode().ok()?.into_rgba8());
    Some(RenderImage::new(vec![Frame::new(buffer)]))
}

/// Сохраняет короткую GIF-анимацию, а при превышении бюджета оставляет первый кадр.
fn decode_gif(reader: Cursor<&[u8]>, limits: Limits) -> Option<RenderImage> {
    let mut decoder = GifDecoder::new(reader).ok()?;
    decoder.set_limits(limits).ok()?;
    let mut frames = Vec::new();
    let mut bytes = 0;
    for frame in decoder.into_frames() {
        let frame = frame.ok()?;
        let delay = frame.delay();
        let buffer = thumbnail(frame.into_buffer());
        bytes += buffer.as_raw().len();
        if bytes > MAX_ANIMATED_BYTES as usize || frames.len() == MAX_ANIMATION_FRAMES {
            frames.truncate(1);
            break;
        }
        frames.push(Frame::from_parts(buffer, 0, 0, delay));
    }
    (!frames.is_empty()).then(|| RenderImage::new(frames))
}

/// Готовит квадратный срез из центра без растягивания и меняет порядок каналов для GPUI.
fn thumbnail(mut buffer: RgbaImage) -> RgbaImage {
    let (width, height) = buffer.dimensions();
    let side = width.min(height);
    if width != height || side > MAX_RENDER_SIDE {
        // GPUI скругляет границы картинки после Cover. Квадрат заполняет аватар без выступающих углов.
        let square =
            imageops::crop_imm(&buffer, (width - side) / 2, (height - side) / 2, side, side);
        buffer = if side > MAX_RENDER_SIDE {
            imageops::thumbnail(&*square, MAX_RENDER_SIDE, MAX_RENDER_SIDE)
        } else {
            square.to_image()
        };
    }
    for color in buffer.pixels_mut() {
        color.0.swap(0, 2);
    }
    buffer
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, ObjectFit, point, px, size};
    use image::{ColorType, ImageEncoder, Rgba, codecs::png::PngEncoder};

    use super::*;

    /// Кодирует тестовое изображение без потери цветов.
    fn png(pixels: &RgbaImage) -> AvatarImage {
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
                ColorType::Rgba8.into(),
            )
            .unwrap();
        AvatarImage {
            format: AvatarFormat::Png,
            bytes,
        }
    }

    /// Иконка получает уменьшенные BGRA-пиксели, а чрезмерные размеры отклоняются.
    #[test]
    fn avatar_decode_bounds_dimensions_and_keeps_color() {
        let color = Rgba([11, 22, 33, 255]);
        let output = decode(&png(&RgbaImage::from_pixel(512, 256, color)), false).unwrap();
        assert_eq!(output.frame_count(), 1);
        let pixels = output.as_bytes(0).unwrap();
        assert_eq!(pixels.len(), 256 * 256 * 4);
        assert_eq!(&pixels[..4], &[33, 22, 11, 255]);
        assert!(
            decode(
                &png(&RgbaImage::from_pixel(MAX_SOURCE_SIDE + 1, 1, color)),
                false
            )
            .is_none()
        );
    }

    /// Проверяет центральный срез без растягивания горизонтального и вертикального фото.
    #[test]
    fn avatar_decode_crops_landscape_and_portrait_to_center_square() {
        for (width, height) in [(8, 4), (4, 8)] {
            let source =
                RgbaImage::from_fn(width, height, |x, y| Rgba([x as u8, y as u8, 42, 255]));
            let output = decode(&png(&source), false).unwrap();
            let slot = Bounds::new(point(px(3.), px(7.)), size(px(40.), px(40.)));
            assert_eq!(ObjectFit::Cover.get_bounds(slot, output.size(0)), slot);
            let pixels = output.as_bytes(0).unwrap();
            assert_eq!(pixels.len(), 4 * 4 * 4);
            let left = (width - 4) / 2;
            let top = (height - 4) / 2;
            for (index, pixel) in pixels.as_chunks::<4>().0.iter().enumerate() {
                let x = left + (index % 4) as u32;
                let y = top + (index / 4) as u32;
                assert_eq!(*pixel, [42, y as u8, x as u8, 255]);
            }
        }
    }
}
