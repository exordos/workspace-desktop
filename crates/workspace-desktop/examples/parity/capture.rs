//! Ждёт отрисовку окна, сохраняет PNG и параметры, проверяет повтор и контрольный сдвиг.

use std::{
    cell::Cell,
    fs::File,
    io::{BufReader, BufWriter},
    path::Path,
    process::Command,
    rc::Rc,
    time::Duration,
};

use futures::{
    channel::oneshot,
    future::{Either, select},
};
use gpui::{AsyncApp, Bounds, Pixels, Window, WindowHandle};
use image::{
    ExtendedColorType, ImageDecoder as _, ImageEncoder as _, Rgba, RgbaImage,
    codecs::png::{PngDecoder, PngEncoder},
};
use objc2::{msg_send, runtime::AnyObject};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde_json::{Value, json};

use super::{CHECK_SHIFT, Config, ID, INSET, Result, View};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
}

/// Проверяет текущее разрешение macOS без открытия системного диалога.
pub fn ensure_screen_access() -> Result<()> {
    if unsafe { CGPreflightScreenCaptureAccess() } {
        Ok(())
    } else {
        Err(
            "Захват окна недоступен: текущей графической сессии macOS нужен доступ к записи экрана"
                .into(),
        )
    }
}

/// Вызывает AppKit только для NSView, принадлежащего окну сцены.
fn with_native_window<T>(window: &Window, read: impl FnOnce(&AnyObject) -> T) -> Result<T> {
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|error| format!("Не удалось получить дескриптор окна: {error:?}"))?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return Err("Окно сцены должно принадлежать AppKit".into());
    };
    // GPUI гарантирует жизнь NSView на время заимствования window_handle.
    unsafe {
        let view = handle.ns_view.cast::<AnyObject>().as_ref();
        let window: *mut AnyObject = msg_send![view, window];
        let window = window.as_ref().ok_or("У сцены нет нативного окна")?;
        Ok(read(window))
    }
}

/// Отключает переход появления до первого показа окна сцены.
pub fn disable_open_animation(window: &Window) -> Result<()> {
    const NS_WINDOW_ANIMATION_BEHAVIOR_NONE: isize = 2;
    with_native_window(window, |native| {
        // NSWindow принимает NSInteger из NSWindowAnimationBehavior.
        let _: () =
            unsafe { msg_send![native, setAnimationBehavior: NS_WINDOW_ANIMATION_BEHAVIOR_NONE] };
    })
}

/// Получает номер окна, который нужен screencapture.
fn window_number(window: &Window) -> Result<isize> {
    let number = with_native_window(window, |native| unsafe { msg_send![native, windowNumber] })?;
    if number > 0 {
        Ok(number)
    } else {
        Err("У сцены нет номера окна WindowServer".into())
    }
}

/// Ждёт, пока AppKit активирует окно и WindowServer закончит оформление первого кадра.
async fn wait_for_activation(window: WindowHandle<View>, cx: &mut AsyncApp) -> Result<()> {
    for _ in 0..100 {
        if window.update(cx, |_, window, _| window.is_window_active())? {
            // На macOS 27 первые кадры после isKeyWindow ещё могут содержать переход.
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            return Ok(());
        }
        cx.background_executor()
            .timer(Duration::from_millis(25))
            .await;
    }
    Err(format!("Сцена {ID}: за 2,5 секунды не дождались активации окна").into())
}

/// Сдвигает только контейнер сцены и ждёт кадр, в котором GPUI его нарисовала.
async fn prepare(
    window: WindowHandle<View>,
    cx: &mut AsyncApp,
    painted: &Cell<Option<Bounds<Pixels>>>,
    inset: f32,
    pass: &str,
) -> Result<Bounds<Pixels>> {
    painted.set(None);
    let (send, recv) = oneshot::channel();
    window.update(cx, |view, window, cx| {
        view.inset = inset;
        cx.notify();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |_, _| {
                let _ = send.send(());
            });
            window.refresh();
        });
        window.refresh();
    })?;
    let timeout = cx.background_executor().timer(Duration::from_secs(5));
    if let Either::Left((Ok(()), _)) = select(recv, timeout).await
        && let Some(bounds) = painted.get()
    {
        return Ok(bounds);
    }
    Err(format!("Сцена {ID}, прогон {pass}: за 5 секунд не дождались отрисовки ChatRow").into())
}

/// Кадр области содержимого после проверки системного канта.
struct CapturedPixels {
    image: RgbaImage,        // Пиксели области содержимого.
    source_size: (u32, u32), // Размер PNG, который вернул screencapture.
    crop_inset: u32,         // Сколько физических пикселей снято с каждой стороны.
}

/// Сохраняет кадр окна и при необходимости удаляет системный кант.
fn save_frame(
    png: &Path,
    number: isize,
    expected: (u32, u32),
    scale: f32,
) -> Result<CapturedPixels> {
    let output = Command::new("/usr/sbin/screencapture")
        .args(["-x", "-o", "-l", &number.to_string()])
        .arg(png)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Не удалось сохранить окно {number}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let mut decoder = PngDecoder::new(BufReader::new(File::open(png)?))?;
    let profile = decoder.icc_profile()?;
    let image = image::DynamicImage::from_decoder(decoder)?.into_rgba8();
    let source_size = image.dimensions();
    let crop_inset = capture_inset(&image, expected, scale)?;
    let image = if crop_inset == 0 {
        image
    } else {
        let cropped =
            image::imageops::crop_imm(&image, crop_inset, crop_inset, expected.0, expected.1)
                .to_image();
        let mut encoder = PngEncoder::new(BufWriter::new(File::create(png)?));
        if let Some(profile) = profile {
            encoder.set_icc_profile(profile)?;
        }
        encoder.write_image(
            cropped.as_raw(),
            expected.0,
            expected.1,
            ExtendedColorType::Rgba8,
        )?;
        cropped
    };
    Ok(CapturedPixels {
        image,
        source_size,
        crop_inset,
    })
}

/// Допускает только симметричный кант macOS шириной в один логический пиксель.
fn capture_inset(image: &RgbaImage, expected: (u32, u32), scale: f32) -> Result<u32> {
    let (width, height) = image.dimensions();
    if (width, height) == expected {
        return Ok(0);
    }
    let inset = scale.round() as u32;
    let transparent_edges = || {
        (0..width).all(|x| image[(x, 0)].0[3] == 0 && image[(x, height - 1)].0[3] == 0)
            && (0..height).all(|y| image[(0, y)].0[3] == 0 && image[(width - 1, y)].0[3] == 0)
    };
    if inset > 0
        && (width, height) == (expected.0 + inset * 2, expected.1 + inset * 2)
        && transparent_edges()
    {
        return Ok(inset);
    }
    Err(format!("Получен кадр {:?}, ожидался размер {expected:?} или симметричный системный кант с прозрачной кромкой", image.dimensions()).into())
}

/// Границы и количество пикселей, которые изменились между кадрами.
struct Difference {
    pixels: usize,          // Число различий RGBA.
    rect: Option<[u32; 4]>, // Прямоугольник различий в физических пикселях, правая и нижняя границы исключены.
}

/// Считает точные различия RGBA и сохраняет выделенные пиксели по запросу.
fn compare(first: &RgbaImage, second: &RgbaImage, output: Option<&Path>) -> Result<Difference> {
    if first.dimensions() != second.dimensions() {
        return Err("Нельзя сравнить кадры разного размера".into());
    }
    let mut diff = Difference {
        pixels: 0,
        rect: None,
    };
    let mut overlay = output.map(|_| first.clone());
    for (x, y, pixel) in first.enumerate_pixels() {
        if pixel != second.get_pixel(x, y) {
            diff.pixels += 1;
            let rect = diff.rect.get_or_insert([x, y, x + 1, y + 1]);
            *rect = [
                rect[0].min(x),
                rect[1].min(y),
                rect[2].max(x + 1),
                rect[3].max(y + 1),
            ];
            if let Some(overlay) = &mut overlay {
                overlay.put_pixel(x, y, Rgba([224, 0, 67, 255]));
            }
        }
    }
    if let Some((overlay, path)) = overlay.zip(output) {
        overlay.save(path)?;
    }
    Ok(diff)
}

/// Данные одного кадра для сравнения в том же запуске.
struct Frame {
    image: RgbaImage,       // Пиксели сохранённого PNG.
    bounds: Bounds<Pixels>, // Область строки, которую измерила GPUI.
    scale: f32,             // Сколько физических пикселей в одном логическом.
}

/// Снимает одну фазу после подтверждения размера окна и отрисовки строки.
async fn capture(
    config: &Config,
    window: WindowHandle<View>,
    painted: &Cell<Option<Bounds<Pixels>>>,
    cx: &mut AsyncApp,
    pass: &str,
    inset: f32,
) -> Result<Frame> {
    let bounds = prepare(window, cx, painted, inset, pass).await?;
    let (viewport, scale, number) = window.update(cx, |_, window, _| {
        (
            window.viewport_size(),
            window.scale_factor(),
            window_number(window),
        )
    })?;
    let number = number?;
    if viewport != config.viewport {
        return Err(format!(
            "Сцена {ID}, прогон {pass}: окно имеет размер {viewport:?}, запрошен {:?}",
            config.viewport
        )
        .into());
    }
    let expected = (
        (f32::from(viewport.width) * scale).round() as u32,
        (f32::from(viewport.height) * scale).round() as u32,
    );
    let dir = config.artifact_dir(pass);
    std::fs::create_dir(&dir)?;
    let png = dir.join("frame.png");
    let pixels = cx
        .background_executor()
        .spawn(async move {
            save_frame(&png, number, expected, scale).map_err(|error| error.to_string())
        })
        .await?;
    let data = json!({
        "scene": ID,
        "sha": config.sha,
        "dirty": config.dirty,
        "pass": pass,
        "theme": config.theme_name(),
        "palette": "blue-mist",
        "fixture": "UI-02",
        "locale": "ru",
        "content_size_px": [f32::from(viewport.width), f32::from(viewport.height)],
        "scale": scale,
        "png_size_px": expected,
        "native_capture_size_px": pixels.source_size,
        "crop_inset_px": pixels.crop_inset,
        "window_number": number,
        "row": row_geometry(bounds),
    });
    std::fs::write(dir.join("run.json"), serde_json::to_vec_pretty(&data)?)?;
    println!("{}", dir.display());
    Ok(Frame {
        image: pixels.image,
        bounds,
        scale,
    })
}

/// Формирует JSON с расположением строки в логических пикселях.
fn row_geometry(bounds: Bounds<Pixels>) -> Value {
    json!({
        "id": ID,
        "x": f32::from(bounds.origin.x),
        "y": f32::from(bounds.origin.y),
        "width": f32::from(bounds.size.width),
        "height": f32::from(bounds.size.height),
    })
}

/// Сохраняет кадр и при проверке требует совпадения повтора и видимого сдвига.
pub async fn run(
    config: &Config,
    window: WindowHandle<View>,
    painted: Rc<Cell<Option<Bounds<Pixels>>>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    wait_for_activation(window, cx).await?;
    let first = capture(config, window, &painted, cx, "baseline", INSET).await?;
    if !config.verify {
        return Ok(());
    }
    let repeat = capture(config, window, &painted, cx, "repeat", INSET).await?;
    let shifted = capture(config, window, &painted, cx, "shift", INSET + CHECK_SHIFT).await?;
    let repeated = compare(&first.image, &repeat.image, None)?;
    let changed = compare(
        &first.image,
        &shifted.image,
        Some(&config.out.join("diff.png")),
    )?;
    let delta = f32::from(shifted.bounds.origin.x - first.bounds.origin.x);
    let report = json!({
        "scene": ID,
        "sha": config.sha,
        "repeat_changed_pixels": repeated.pixels,
        "shift_changed_pixels": changed.pixels,
        "shift_changed_bounds_physical_px": changed.rect,
        "shift_dx_logical_px": delta,
        "baseline_row": row_geometry(first.bounds),
        "repeat_row": row_geometry(repeat.bounds),
        "shift_row": row_geometry(shifted.bounds),
    });
    std::fs::write(
        config.out.join("diff.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    if repeated.pixels != 0 || first.bounds != repeat.bounds {
        return Err("Повтор сцены дал другой кадр или геометрию. См. diff.json".into());
    }
    let row_rect = [
        (f32::from(first.bounds.left()) * first.scale).floor() as u32,
        (f32::from(first.bounds.top()) * first.scale).floor() as u32,
        (f32::from(shifted.bounds.right().max(first.bounds.right())) * first.scale).ceil() as u32,
        (f32::from(first.bounds.bottom()) * first.scale).ceil() as u32,
    ];
    let inside_row = changed.rect.is_some_and(|[x0, y0, x1, y1]| {
        x0 >= row_rect[0] && y0 >= row_rect[1] && x1 <= row_rect[2] && y1 <= row_rect[3]
    });
    if !inside_row || delta != CHECK_SHIFT || shifted.bounds.top() != first.bounds.top() {
        return Err(
            "Контрольный сдвиг дал разницу вне строки или неверную геометрию. См. diff.json".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Принимает точный размер и симметричный системный кант, защищает от обрезки другого кадра.
    #[test]
    fn only_crops_the_window_server_border() {
        let expected = (8, 10);
        assert_eq!(
            capture_inset(&RgbaImage::new(8, 10), expected, 2.).unwrap(),
            0
        );
        let mut retina = RgbaImage::from_fn(12, 14, |x, y| {
            Rgba([
                255,
                255,
                255,
                if x == 0 || y == 0 || x == 11 || y == 13 {
                    0
                } else {
                    255
                },
            ])
        });
        assert_eq!(capture_inset(&retina, expected, 2.).unwrap(), 2);
        assert!(capture_inset(&retina, expected, 1.).is_err());
        retina.put_pixel(0, 5, Rgba([255, 255, 255, 255]));
        assert!(capture_inset(&retina, expected, 2.).is_err());
    }
}
