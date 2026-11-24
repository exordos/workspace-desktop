//! Рисует выбор фона и подтверждение области действия. Читает изображение в
//! системном диалоге, проверяет размер и публикует только готовый JPEG.

use std::{
    fs,
    io::{Cursor, Read},
    path::Path,
    sync::Arc,
};

use gpui::{
    App, AppContext, Context, Div, FocusHandle, FontWeight, Hsla, Image, ImageFormat, ImageSource,
    ObjectFit, PathBuilder, PathPromptOptions, Pixels, Render, Stateful, StyledImage, Task,
    WeakEntity, Window, canvas, div, img, point, prelude::*, px,
};
use workspace_core::storage::HistoryScope;

use crate::gpui_client::{
    contracts::{AccountSession, ConversationAddress},
    preferences::{
        Language, Preferences,
        appearance::{Background, BackgroundPreset},
    },
    right_panel::RightPanel,
    ui::{Button, ButtonAppearance, Icon, Palette, SURFACE_RADIUS, TextRole, activate, label},
};

use super::Appearance;

const PRESETS: [BackgroundPreset; 3] = [
    BackgroundPreset::Mountains,
    BackgroundPreset::Sea,
    BackgroundPreset::Forest,
];
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FILE_PIXELS: u64 = 24_000_000;
const MAX_STORED_IMAGE_BYTES: usize = 1_499_900;

/// Фокус кнопок позволяет вернуться к выбранной картинке после закрытия диалога.
pub(super) struct BackgroundSection {
    choices: [FocusHandle; 3],  // В том же порядке, что пресеты.
    upload: FocusHandle,        // Открывает системный выбор файла.
    reset: FocusHandle,         // Открывает подтверждение стандартного фона.
    pending: Option<Task<()>>,  // Чтение файла, которое отменяется при закрытии страницы.
    error: Option<UploadError>, // Сообщение рядом с кнопкой загрузки.
}

impl BackgroundSection {
    /// Готовит фокус кнопок в пределах открытого экрана.
    pub(super) fn new(cx: &mut Context<Appearance>) -> Self {
        Self {
            choices: std::array::from_fn(|_| cx.focus_handle()),
            upload: cx.focus_handle(),
            reset: cx.focus_handle(),
            pending: None,
            error: None,
        }
    }
}

impl Appearance {
    /// Возвращает адрес, для которого можно применить фон только к одному чату.
    fn background_address(&self, cx: &App) -> Option<ConversationAddress> {
        self.host
            .upgrade()
            .and_then(|host| current_chat(host.read(cx), &self.session))
    }

    /// Показывает пресеты, выбор изображения, действующий фон и сброс.
    pub(super) fn background_options(
        &self,
        palette: Palette,
        language: Language,
        cx: &Context<Self>,
    ) -> Div {
        let address = self.background_address(cx);
        let scope = address.as_ref().map(|address| address.route.scope);
        let current = Preferences::read(cx).appearance.background.for_chat(scope);
        let working = self.background.pending.is_some();
        let mut choices = Vec::new();
        for (index, preset) in PRESETS.into_iter().enumerate() {
            let focus = self.background.choices[index].clone();
            let id = format!("settings-background-{}", preset.id());
            let title = match preset {
                BackgroundPreset::Mountains => language.tr("desktop.background.mountains"),
                BackgroundPreset::Sea => language.tr("desktop.background.sea"),
                BackgroundPreset::Forest => language.tr("desktop.background.forest"),
            };
            let value = Background::Preset(preset);
            let active = current == Some(&value);
            let card = preset_card(id, title, Some(preset.source()), active, palette);
            let view = cx.weak_entity();
            let opener = focus.clone();
            choices.push(activate(card, Some(&focus), palette, move |window, app| {
                let _ = view.update(app, |view, cx| {
                    view.choose_background(Some(value.clone()), opener.clone(), window, cx);
                });
            }));
        }
        let upload = preset_card(
            "settings-background-upload".into(),
            if working {
                language.tr("desktop.background.processing")
            } else {
                language.tr("desktop.background.addCustom")
            },
            None,
            false,
            palette,
        );
        let upload = if working {
            upload.opacity(0.5)
        } else {
            let view = cx.weak_entity();
            activate(
                upload,
                Some(&self.background.upload),
                palette,
                move |window, app| {
                    let _ = view.update(app, |view, cx| view.pick_background(window, cx));
                },
            )
        };
        let reset_focus = self.background.reset.clone();
        let view = cx.weak_entity();
        let reset = div()
            .id("settings-background-reset")
            .debug_selector(|| "settings-background-reset".into())
            .h_8()
            .px_3()
            .flex()
            .items_center()
            .rounded(px(8.))
            .hover(|style| style.bg(palette.card_hover))
            .child(
                label(
                    palette,
                    TextRole::Caption,
                    language.tr("desktop.background.defaultImage"),
                )
                .text_color(palette.text)
                .font_weight(FontWeight::MEDIUM),
            );
        let reset = activate(
            reset,
            Some(&self.background.reset),
            palette,
            move |window, app| {
                let _ = view.update(app, |view, cx| {
                    view.choose_background(None, reset_focus.clone(), window, cx);
                });
            },
        );
        div()
            .debug_selector(|| "chat-background-options".into())
            .p(px(16.))
            .pb(px(28.))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                label(
                    palette,
                    TextRole::Caption,
                    language.tr("desktop.background.fileRequirements"),
                )
                .line_height(px(20.)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().flex().gap_2().children(choices.drain(..2)))
                    .child(div().flex().gap_2().children(choices).child(upload)),
            )
            .children(current.and_then(|background| {
                match background {
                    Background::Preset(_) => None,
                    Background::Custom(_) => Some(
                        div()
                            .p_2()
                            .flex()
                            .items_center()
                            .gap_3()
                            .rounded(px(8.))
                            .border_1()
                            .border_color(palette.border)
                            .child(
                                img(background.source())
                                    .w_16()
                                    .h_10()
                                    .object_fit(ObjectFit::Cover)
                                    .rounded(px(4.)),
                            )
                            .child(
                                label(
                                    palette,
                                    TextRole::Caption,
                                    language.tr("desktop.background.customImage"),
                                )
                                .text_color(palette.text),
                            ),
                    ),
                }
            }))
            .child(div().flex().child(reset))
            .children(self.background.error.map(|error| {
                label(palette, TextRole::Body, error.label(language)).text_color(palette.danger)
            }))
    }

    /// Отменяет предыдущую загрузку и открывает подтверждение выбранного фона.
    fn choose_background(
        &mut self,
        value: Option<Background>,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.background.pending = None;
        self.background.error = None;
        let dialog = BackgroundDialog {
            host: self.host.clone(),
            session: self.session.clone(),
            address: self.background_address(cx),
            value,
            error: false,
        };
        let _ = self.host.update(cx, |host, cx| {
            if host.is_active(&Self::key(&self.session)) {
                host.open_modal(&self.session, opener, window, cx, move |_, cx| {
                    cx.new(|_| dialog)
                });
            }
        });
        cx.notify();
    }

    /// Загружает выбранный файл вне UI и показывает результат только в исходном контексте.
    fn pick_background(&mut self, window: &Window, cx: &mut Context<Self>) {
        let language = Preferences::read(cx).language;
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(language.tr("desktop.background.chooseFile").into()),
        });
        let opener = self.background.upload.clone();
        let address = self.background_address(cx);
        let worker = cx.background_executor().clone();
        self.background.error = None;
        self.background.pending = Some(cx.spawn_in(window, async move |view, cx| {
            let result = match picker.await {
                Ok(Ok(Some(mut paths))) => match paths.pop() {
                    Some(path) => worker
                        .spawn(async move { prepare_file(&path) })
                        .await
                        .map(Some),
                    None => Ok(None),
                },
                Ok(Ok(None)) => Ok(None),
                _ => Err(UploadError::Decode),
            };
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.background.pending = None;
                    if view.background_address(cx) == address {
                        match result {
                            Ok(Some(image)) => {
                                view.choose_background(Some(image), opener, window, cx)
                            }
                            Ok(None) => {}
                            Err(error) => view.background.error = Some(error),
                        }
                    }
                    cx.notify();
                });
            });
        }));
        cx.notify();
    }
}

/// Рисует плитку с изображением 8:5 или кнопкой выбора файла и подписью под ней.
fn preset_card(
    id: String,
    title: &'static str,
    source: Option<ImageSource>,
    selected: bool,
    palette: Palette,
) -> Stateful<Div> {
    let mut aspect = div()
        .relative()
        .w_full()
        .overflow_hidden()
        .flex()
        .items_center()
        .justify_center();
    aspect.style().aspect_ratio = Some(8. / 5.);
    let aspect = match source {
        Some(source) => aspect.child(
            img(source)
                .absolute()
                .inset_0()
                .size_full()
                .object_fit(ObjectFit::Cover),
        ),
        None => aspect.child(Icon::BackgroundAdd.element(px(28.), palette.text)),
    };
    div()
        .id(gpui::SharedString::from(id.clone()))
        .debug_selector(move || id.clone())
        .flex_1()
        .min_w_0()
        .rounded(px(8.))
        .border_1()
        .border_color(if selected {
            palette.accent
        } else {
            palette.border
        })
        .bg(palette.background)
        .hover(|style| style.border_color(palette.accent))
        .overflow_hidden()
        .child(aspect)
        .child(
            label(palette, TextRole::Caption, title)
                .text_color(palette.text)
                .line_height(px(16.))
                .whitespace_nowrap()
                .overflow_hidden()
                .text_center()
                .px_2()
                .py(px(6.)),
        )
        .when(selected, |card| {
            card.relative().child(
                div()
                    .absolute()
                    .top(px(6.))
                    .right(px(6.))
                    .p(px(2.))
                    .rounded_full()
                    .bg(palette.accent)
                    .child(Icon::Check.element(px(14.), palette.on_accent)),
            )
        })
}

/// Защищает настройку чата от перехода на другую организацию или вход.
fn current_chat(host: &RightPanel, session: &AccountSession) -> Option<ConversationAddress> {
    host.current_conversation()
        .filter(|address| {
            address.route.owner == session.owner && address.generation == session.generation
        })
        .cloned()
}

/// Готовый выбор и контекст исходного чата до подтверждения.
struct BackgroundDialog {
    host: WeakEntity<RightPanel>, // Панель удерживает модальный фокус.
    session: AccountSession,      // Чей профиль следует обновить.
    address: Option<ConversationAddress>, // Чат на момент выбора, либо ни одного.
    value: Option<Background>,    // None сбрасывает фон.
    error: bool,                  // Не удалось записать профиль; прежний фон сохранён.
}

impl BackgroundDialog {
    /// Закрывает подтверждение и возвращает фокус к его кнопке в сайдбаре.
    fn close(&self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self
            .host
            .update(cx, |host, cx| host.close_modal(window, cx));
    }

    /// Применяет выбор к исходному чату или ко всем чатам активного входа.
    fn save(&mut self, single_chat: bool, window: &mut Window, cx: &mut Context<Self>) {
        let valid = self.host.upgrade().is_some_and(|host| {
            let host = host.read(cx);
            host.is_active(&Appearance::key(&self.session))
                && current_chat(host, &self.session) == self.address
        });
        if !valid {
            self.close(window, cx);
            return;
        }
        let scope: Option<HistoryScope> = if single_chat {
            let Some(address) = self.address.as_ref() else {
                return;
            };
            Some(address.route.scope)
        } else {
            None
        };
        let next = Preferences::read(cx)
            .appearance
            .background
            .with_background(scope, self.value.clone());
        self.error = next.is_none_or(|next| {
            Preferences::update(cx, |preferences| preferences.appearance.background = next).is_err()
        });
        if self.error {
            cx.notify();
        } else {
            self.close(window, cx);
            window.refresh();
        }
    }
}

impl Render for BackgroundDialog {
    /// Рисует окно с предпросмотром и тремя действиями в нижней части.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read(cx).language;
        let viewport = window.viewport_size();
        let width = px(448.).min(viewport.width - px(32.)).max(px(0.));
        let compact = viewport.width < px(640.);
        let view = cx.weak_entity();
        let close = div()
            .id("settings-background-close")
            .size(px(26.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .hover(|style| style.bg(palette.card_hover))
            .child(Icon::Close.element(px(18.), palette.text_secondary));
        let mut cancel = Button::new("settings-background-cancel", language.tr("common.cancel"));
        cancel.appearance = ButtonAppearance::Ghost;
        let mut chat = Button::new(
            "settings-background-apply-chat",
            language.tr("desktop.background.onlyThisChat"),
        );
        chat.appearance = ButtonAppearance::Ghost;
        chat.disabled = self.address.is_none();
        let mut all = Button::new(
            "settings-background-apply-all",
            language.tr("desktop.background.allChats"),
        );
        all.appearance = ButtonAppearance::Primary;
        div()
            .id("settings-background-dialog")
            .debug_selector(|| "settings-background-dialog".into())
            .w(width)
            .max_h((viewport.height - px(32.)).max(px(0.)))
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .rounded(SURFACE_RADIUS)
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(label(
                        palette,
                        TextRole::Heading,
                        language.tr("desktop.background.apply"),
                    ))
                    .child(activate(close, None, palette, {
                        let view = view.clone();
                        move |window, app| {
                            let _ = view.update(app, |view, cx| view.close(window, cx));
                        }
                    })),
            )
            .child(
                label(
                    palette,
                    TextRole::Body,
                    language.tr("desktop.background.applyHint"),
                )
                .text_color(palette.text_secondary),
            )
            .map(|dialog| match &self.value {
                Some(value) => dialog.child(
                    div()
                        .relative()
                        .w_full()
                        .h(px(192.))
                        .overflow_hidden()
                        .child(
                            img(value.source())
                                .absolute()
                                .inset_0()
                                .size_full()
                                .object_fit(ObjectFit::Cover),
                        )
                        .child(preview_corners(px(6.), palette.surface)),
                ),
                None => dialog.child(
                    label(
                        palette,
                        TextRole::Body,
                        language.tr("desktop.background.resetHint"),
                    )
                    .text_color(palette.text_secondary),
                ),
            })
            .when(self.address.is_none(), |dialog| {
                dialog.child(
                    label(
                        palette,
                        TextRole::Body,
                        language.tr("desktop.background.openChat"),
                    )
                    .text_color(palette.text_secondary),
                )
            })
            .when(self.error, |dialog| {
                dialog.child(
                    label(
                        palette,
                        TextRole::Body,
                        language.tr("desktop.background.saveFailed"),
                    )
                    .text_color(palette.danger),
                )
            })
            .child(
                div()
                    .flex()
                    .gap_2()
                    .when(compact, |footer| footer.flex_col())
                    .child(
                        cancel
                            .render(palette, None, {
                                let view = view.clone();
                                move |window, app| {
                                    let _ = view.update(app, |view, cx| view.close(window, cx));
                                }
                            })
                            .h_9()
                            .px_4(),
                    )
                    .child(
                        chat.render(palette, None, {
                            let view = view.clone();
                            move |window, app| {
                                let _ = view.update(app, |view, cx| view.save(true, window, cx));
                            }
                        })
                        .h_9()
                        .flex_1()
                        .border_color(palette.border)
                        .debug_selector(|| "settings-background-apply-chat".into()),
                    )
                    .child(
                        all.render(palette, None, move |window, app| {
                            let _ = view.update(app, |view, cx| view.save(false, window, cx));
                        })
                        .h_9()
                        .flex_1()
                        .debug_selector(|| "settings-background-apply-all".into()),
                    ),
            )
    }
}

/// Скругляет видимые углы после центральной обрезки картинки.
fn preview_corners(radius: Pixels, surface: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            // Cover скругляет саму картинку, до обрезки. Закрываем углы рамки цветом диалога.
            let control = radius * 0.447_715_25;
            let mut shape = PathBuilder::fill();
            for (corner, dx, dy) in [
                (bounds.origin, 1., 1.),
                (bounds.top_right(), -1., 1.),
                (bounds.bottom_left(), 1., -1.),
                (bounds.bottom_right(), -1., -1.),
            ] {
                let at = |x: Pixels, y: Pixels| point(corner.x + x * dx, corner.y + y * dy);
                shape.move_to(corner);
                shape.line_to(at(radius, px(0.)));
                shape.cubic_bezier_to(at(px(0.), radius), at(control, px(0.)), at(px(0.), control));
                shape.close();
            }
            if let Ok(path) = shape.build() {
                window.paint_path(path, surface);
            }
        },
    )
    .absolute()
    .inset_0()
    .size_full()
}

/// Причина отказа от файла, которую можно показать рядом с выбором.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UploadError {
    Format, // Выбранный файл имеет другой формат.
    Size,   // Превышен допустимый размер файла или число пикселей.
    Decode, // Файл не читается или повреждён.
}

impl UploadError {
    /// Возвращает локализованный текст ошибки выбора изображения.
    fn label(self, language: Language) -> &'static str {
        match self {
            Self::Format => language.tr("desktop.background.invalidFormat"),
            Self::Size => language.tr("desktop.background.invalidSize"),
            Self::Decode => language.tr("desktop.background.openFailed"),
        }
    }
}

/// Читает только обычный файл в пределах лимита 8 МБ.
fn prepare_file(path: &Path) -> Result<Background, UploadError> {
    let metadata = fs::metadata(path).map_err(|_| UploadError::Decode)?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err(UploadError::Size);
    }
    let file = fs::File::open(path).map_err(|_| UploadError::Decode)?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| UploadError::Decode)?;
    prepare_bytes(&bytes)
}

/// Проверяет формат, ограничения пикселей и записывает статичный JPEG без исходных метаданных.
fn prepare_bytes(bytes: &[u8]) -> Result<Background, UploadError> {
    use image::{
        DynamicImage, ImageDecoder, ImageFormat as SourceFormat, ImageReader,
        codecs::jpeg::JpegEncoder,
    };

    if bytes.is_empty() || bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(UploadError::Size);
    }
    let format = image::guess_format(bytes).map_err(|_| UploadError::Format)?;
    if !matches!(
        format,
        SourceFormat::Jpeg | SourceFormat::Png | SourceFormat::WebP
    ) {
        return Err(UploadError::Format);
    }
    let reader = || ImageReader::with_format(Cursor::new(bytes), format);
    let (width, height) = reader()
        .into_dimensions()
        .map_err(|_| UploadError::Decode)?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_FILE_PIXELS {
        return Err(UploadError::Size);
    }
    let mut input = reader();
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(24_000_000 * 8);
    input.limits(limits);
    let mut decoder = input.into_decoder().map_err(|_| UploadError::Decode)?;
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| UploadError::Decode)?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    if width > 1600 || height > 1600 {
        image = image.thumbnail(1600, 1600);
    }
    // JPEG не хранит прозрачность. Смешиваем цвет с белым, как делает Electron.
    let rgb = match image {
        DynamicImage::ImageRgb8(rgb) => rgb,
        image => {
            let rgba = image.into_rgba8();
            image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
                let [r, g, b, a] = rgba.get_pixel(x, y).0;
                let alpha = u16::from(a);
                image::Rgb([r, g, b].map(|channel| {
                    ((u16::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255) as u8
                }))
            })
        }
    };
    let mut output = Vec::new();
    JpegEncoder::new_with_quality(&mut output, 80)
        .encode_image(&rgb)
        .map_err(|_| UploadError::Decode)?;
    if output.len() > MAX_STORED_IMAGE_BYTES {
        return Err(UploadError::Size);
    }
    Ok(Background::Custom(Arc::new(Image::from_bytes(
        ImageFormat::Jpeg,
        output,
    ))))
}

#[cfg(test)]
mod tests {
    use image::{ColorType, ImageEncoder, codecs::png::PngEncoder};

    use super::*;

    /// Проверяет ограничения формата, нормализацию изображения и прозрачного фона.
    #[test]
    fn upload_normalizes_safe_raster_to_bounded_jpeg() {
        assert_eq!(prepare_bytes(&[]), Err(UploadError::Size));
        assert_eq!(prepare_bytes(b"<svg></svg>"), Err(UploadError::Format));
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(&[0, 0, 0, 0], 1, 1, ColorType::Rgba8.into())
            .unwrap();
        let Background::Custom(image) = prepare_bytes(&png).unwrap() else {
            panic!("ожидался JPEG")
        };
        let decoded = image::load_from_memory(&image.bytes).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (1, 1));
        assert_eq!(decoded.get_pixel(0, 0).0, [255, 255, 255]);
    }
}
