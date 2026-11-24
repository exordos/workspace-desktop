//! Открывает нативный выбор фотографии и готовит её предпросмотр перед сохранением профиля.

use std::{fs::File, io::Read, path::PathBuf, sync::Arc};

use gpui::{App, Context, PathPromptOptions, RenderImage, WeakEntity, Window, div, prelude::*, px};
use workspace_core::{
    directory::{AvatarFormat, AvatarImage},
    messenger::own_profile::MAX_OWN_AVATAR_BYTES,
};

use super::Personal;
use crate::gpui_client::{
    avatars,
    preferences::Preferences,
    right_panel::RightPanel,
    ui::{Button, ButtonAppearance, Icon, Palette, TextRole, activate, label},
};

/// Действие, подтверждаемое кнопкой сохранения на странице профиля.
#[derive(Clone)]
pub(super) enum AvatarChoice {
    Remove,                                 // Вернуть стандартное фото.
    Upload(Arc<Vec<u8>>, Arc<RenderImage>), // Исходный файл и ограниченный по размеру предпросмотр.
}

/// Компактный оверлей с двумя источниками изменения и отменой.
pub(super) struct AvatarDialog {
    host: WeakEntity<RightPanel>, // Оверлей окна, который вернёт клавиатурный фокус.
    profile: WeakEntity<Personal>, // Владелец подготовленных правок.
    can_remove: bool,             // Фото есть или было выбрано ранее.
    choosing: bool,               // Окно ОС ещё удерживает выбор файла.
    error: Option<&'static str>,  // Безопасная подпись ошибки при открытии или декодировании.
}

impl AvatarDialog {
    /// Создаёт новую форму поверх редактируемой карточки.
    pub(super) fn new(
        host: WeakEntity<RightPanel>,
        profile: WeakEntity<Personal>,
        can_remove: bool,
    ) -> Self {
        Self {
            host,
            profile,
            can_remove,
            choosing: false,
            error: None,
        }
    }

    /// Закрывает форму и возвращает фокус к фотографии.
    fn cancel(&self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self
            .host
            .update(cx, |host, cx| host.close_modal(window, cx));
    }

    /// Отдаёт предпросмотр редактируемой странице. Запись произойдёт при подтверждении.
    fn selected(&self, choice: AvatarChoice, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self
            .profile
            .update(cx, |profile, cx| profile.select_photo(choice, cx));
        self.cancel(window, cx);
    }

    /// Просит выбрать один файл, читает его вне потока окна и оставляет форму при ошибке.
    fn choose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.choosing {
            return;
        }
        let language = Preferences::read(cx).language;
        self.choosing = true;
        self.error = None;
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(language.tr("settings.chooseFromGallery").into()),
        });
        let background = cx.background_executor().clone();
        cx.spawn_in(window, async move |view, cx| {
            let result = match dialog.await {
                Ok(Ok(Some(paths))) => match paths.into_iter().next() {
                    Some(path) => background
                        .spawn(async move { load_preview(path) })
                        .await
                        .map(Some),
                    None => Ok(None),
                },
                Ok(Ok(None)) => Ok(None),
                _ => Err("desktop.avatar.chooseFailed"),
            };
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.choosing = false;
                    match result {
                        Ok(Some(choice)) => view.selected(choice, window, cx),
                        Ok(None) => {}
                        Err(error) => view.error = Some(error),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// Передаёт действие кнопки живому экземпляру диалога.
    fn handler(
        &self,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl Fn(&mut Window, &mut App) + 'static {
        let weak = cx.weak_entity();
        move |window, app| {
            let _ = weak.update(app, |view, cx| action(view, window, cx));
        }
    }
}

impl Render for AvatarDialog {
    /// Показывает доступность галереи, удаления и отмены как в Electron.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read(cx).language;
        let choose = div()
            .id("profile-avatar-gallery")
            .debug_selector(|| "profile-avatar-gallery".into())
            .flex()
            .items_center()
            .h(px(40.))
            .gap_4()
            .px_1()
            .rounded(px(6.))
            .hover(|style| style.bg(palette.card_hover))
            .child(Icon::Image.element(px(24.), palette.icon))
            .child(label(
                palette,
                TextRole::Body,
                language.tr("settings.chooseFromGallery"),
            ));
        let choose = if self.choosing {
            choose
        } else {
            activate(choose, None, palette, self.handler(cx, Self::choose))
        };
        let remove = div()
            .id("profile-avatar-remove")
            .debug_selector(|| "profile-avatar-remove".into())
            .flex()
            .items_center()
            .h(px(40.))
            .gap_4()
            .px_1()
            .rounded(px(6.))
            .child(Icon::Delete.element(px(24.), palette.danger))
            .child(
                label(
                    palette,
                    TextRole::Body,
                    language.tr("settings.removeCurrentPhoto"),
                )
                .text_color(palette.danger),
            );
        let remove = if self.can_remove && !self.choosing {
            activate(
                remove.hover(|style| style.bg(palette.card_hover)),
                None,
                palette,
                self.handler(cx, |view, window, cx| {
                    view.selected(AvatarChoice::Remove, window, cx)
                }),
            )
        } else {
            remove.opacity(0.55)
        };
        let mut cancel = Button::new("profile-avatar-cancel", language.tr("common.cancel"));
        cancel.appearance = ButtonAppearance::Neutral;
        cancel.disabled = self.choosing;
        div()
            .id("profile-avatar-dialog")
            .debug_selector(|| "profile-avatar-dialog".into())
            .w(px(322.).min((window.viewport_size().width - px(24.)).max(px(0.))))
            .flex()
            .flex_col()
            .p_3()
            .py_4()
            .rounded(px(12.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .shadow_md()
            .child(choose)
            .child(remove)
            .children(self.error.map(|error| {
                label(palette, TextRole::Caption, language.tr(error)).text_color(palette.danger)
            }))
            .child(
                cancel
                    .render(
                        palette,
                        None,
                        self.handler(cx, |view, window, cx| view.cancel(window, cx)),
                    )
                    .mt_4()
                    .h(px(40.))
                    .bg(palette.card_selected)
                    .text_color(palette.accent),
            )
    }
}

/// Читает только допустимый объём байтов и использует декодер общего кэша аватаров.
fn load_preview(path: PathBuf) -> Result<AvatarChoice, &'static str> {
    let invalid = "desktop.avatar.invalidFormat";
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| invalid)?
        .take(MAX_OWN_AVATAR_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid)?;
    if bytes.len() > MAX_OWN_AVATAR_BYTES {
        return Err("desktop.avatar.fileTooLarge");
    }
    let format = AvatarFormat::sniff(&bytes).ok_or(invalid)?;
    let image = AvatarImage { bytes, format };
    let preview = avatars::decode::decode(&image, false).ok_or(invalid)?;
    Ok(AvatarChoice::Upload(
        Arc::new(image.bytes),
        Arc::new(preview),
    ))
}
