//! Готовит квадратное превью вложения из локального или загруженного изображения.

use std::{io::Cursor, sync::Arc};

use gpui::RenderImage;
use image::{DynamicImage, Frame, ImageDecoder, ImageReader, Limits, RgbaImage, imageops};

use crate::platform::media;

/// Читает изображение с лимитом памяти и уменьшает до 80 × 80 для экрана Retina.
pub(in crate::gpui_client::composer::attachment_upload) fn decode_thumbnail(
    bytes: &[u8],
) -> Option<Arc<RenderImage>> {
    let image = if media::heif_mime_type(bytes).is_some() {
        // ImageIO уменьшает контейнер до декодирования в GPUI и уже применяет ориентацию.
        let preview = media::heif_preview(bytes, 320)?;
        DynamicImage::ImageRgba8(RgbaImage::from_raw(
            preview.width,
            preview.height,
            preview.pixels,
        )?)
    } else {
        decode_raster(bytes)?
    };
    let (width, height) = (image.width(), image.height());
    let side = width.min(height);
    // Сначала берём квадрат. У очень широкого фото промежуточный кадр иначе разрастётся.
    let crop = imageops::crop_imm(&image, (width - side) / 2, (height - side) / 2, side, side);
    let mut square = imageops::resize(&*crop, 80, 80, imageops::FilterType::Triangle);
    // GPUI ожидает порядок BGRA, тогда как декодер отдаёт RGBA.
    for pixel in square.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![Frame::new(square)])))
}

/// Декодирует небольшой растр и применяет ориентацию из метаданных исходника.
fn decode_raster(bytes: &[u8]) -> Option<DynamicImage> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder().ok()?;
    limits.reserve(decoder.total_bytes()).ok()?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > 16 * 1024 * 1024 {
        return None;
    }
    decoder.set_limits(limits).ok()?;
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    Some(image)
}

#[cfg(test)]
mod tests {
    use image::{ExtendedColorType, ImageEncoder, Rgba, RgbaImage, codecs::png::PngEncoder};

    use super::*;

    /// Широкое изображение обрезается по центру, а ошибочные данные оставляют заглушку.
    #[test]
    fn attachment_thumbnail_crops_without_changing_colors() {
        let source = RgbaImage::from_fn(160, 80, |x, _| {
            if (40..120).contains(&x) {
                Rgba([10, 30, 60, 255])
            } else {
                Rgba([0, 0, 0, 255])
            }
        });
        let mut bytes = vec![];
        PngEncoder::new(&mut bytes)
            .write_image(&source, 160, 80, ExtendedColorType::Rgba8)
            .unwrap();
        let decoded = decode_thumbnail(&bytes).unwrap();
        let pixels = decoded.as_bytes(0).unwrap();
        assert_eq!(pixels.len(), 80 * 80 * 4);
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == [60, 30, 10, 255])
        );
        assert!(decode_thumbnail(b"invalid image").is_none());
    }

    /// Использует ImageIO для тестового HEIC, который не читается растровым декодером Rust.
    #[test]
    fn attachment_thumbnail_reads_heic_via_image_io() {
        let bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/media/synthetic-photo.heic"
        ));
        let decoded = decode_thumbnail(bytes).unwrap();
        assert_eq!(decoded.as_bytes(0).unwrap().len(), 80 * 80 * 4);
    }
}
