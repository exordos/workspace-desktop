//! Рисует перекрытие, форму цвета и подтверждение удаления.
//! Кнопки направляют действия обратно в состояние диалога.

use gpui::{
    AnyElement, Context, Div, Render, Stateful, Window, anchored, deferred, div, point, prelude::*,
    px, rgb, rgba,
};

use crate::gpui_client::ui::{
    Button, ButtonAppearance, Icon, Palette, TextRole, ThemeMode, label, popup,
};

use super::{
    COLORS, Dismiss, FolderCommandError, FolderDialog, KEY_CONTEXT, MAX_TITLE_LENGTH, Mode, Next,
    OpenDialog, Previous, Status,
};

impl Render for FolderDialog {
    /// Кладёт диалог над окном, оставляя каталог на прежнем месте под затемнением.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(dialog) = self.open.as_ref() else {
            if let Some(focus) = self.return_focus.take() {
                window.focus(&focus);
            } else if self.region.contains_focused(window, cx) {
                // После смены входа старый триггер не подходит. Корень сам поставит новый фокус.
                window.blur();
            }
            return div().absolute().size(px(0.));
        };
        let pending = matches!(dialog.status, Status::Pending);
        if pending != self.region.is_focused(window) {
            self.focus_initial(window, cx);
        }

        let viewport = window.viewport_size();
        let palette = Palette::for_mode(ThemeMode::from(window.appearance()));
        let backdrop = div()
            .id("folder-dialog-backdrop")
            .debug_selector(|| "folder-dialog-backdrop".into())
            .absolute()
            .inset_0()
            .bg(rgba(0x00000066))
            .occlude()
            .on_click(cx.listener(|view, _, _, cx| view.dismiss(cx)));
        let panel = div()
            .id("folder-dialog")
            .debug_selector(|| "folder-dialog".into())
            .relative()
            .w(px(420.).min((viewport.width - px(24.)).max(px(0.))))
            .max_h((viewport.height - px(24.)).max(px(0.)))
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(20.))
            .rounded(px(12.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .shadow_md()
            .overflow_y_scroll()
            .occlude()
            .track_focus(&self.region)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|view, _: &Dismiss, _, cx| {
                cx.stop_propagation();
                view.dismiss(cx);
            }))
            .on_action(
                cx.listener(|view, _: &Next, window, cx| view.cycle_focus(false, window, cx)),
            )
            .on_action(
                cx.listener(|view, _: &Previous, window, cx| view.cycle_focus(true, window, cx)),
            )
            .child(self.header(dialog, palette, cx))
            .child(self.content(dialog, palette, cx))
            .children(self.notice(dialog, palette, cx))
            .child(self.footer(dialog, palette, cx));
        let layer = div()
            .relative()
            .w(viewport.width)
            .h(viewport.height)
            .flex()
            .items_center()
            .justify_center()
            .child(backdrop)
            .child(panel);
        // Меню рендерятся с приоритетом 2. Диалог поглощает клики и поверх них.
        div().absolute().size(px(0.)).child(
            deferred(anchored().position(point(px(0.), px(0.))).child(layer)).with_priority(4),
        )
    }
}

impl FolderDialog {
    /// Показывает заголовок сценария и крестик пока запрос не выполняется.
    fn header(&self, dialog: &OpenDialog, palette: Palette, cx: &Context<Self>) -> Div {
        let title = match dialog.mode {
            Mode::Create(_) => "Создать папку",
            Mode::Edit { .. } => "Редактировать папку",
            Mode::Delete { .. } => "Удалить папку?",
        };
        let mut close = Button::new("folder-dialog-close", "");
        close.appearance = ButtonAppearance::Ghost;
        close.icon = Some(Icon::Close);
        close.disabled = matches!(dialog.status, Status::Pending);
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .child(label(palette, TextRole::Heading, title))
            .child(
                self.button(close, palette, &self.close, Self::dismiss, cx)
                    .tooltip(move |_, app| popup::tooltip(palette, "Закрыть".into(), app)),
            )
    }

    /// Показывает поля цветной формы или текст явного подтверждения удаления.
    fn content(&self, dialog: &OpenDialog, palette: Palette, cx: &Context<Self>) -> AnyElement {
        if let Mode::Delete { title, .. } = &dialog.mode {
            return label(
                palette,
                TextRole::Body,
                format!("Папка «{title}» будет удалена. Чаты останутся в мессенджере."),
            )
            .into_any_element();
        }
        let title = self.name.read(cx).text().trim();
        let preview = if title.is_empty() {
            "Пример"
        } else {
            title
        }
        .to_owned();
        let color = dialog
            .color
            .map(|color| rgb(color & 0x00ffffff).into())
            .unwrap_or(palette.icon);
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(self.name.clone())
            .child(label(palette, TextRole::Caption, "Цвет"))
            .child(div().flex().flex_wrap().gap(px(8.)).children(
                COLORS.into_iter().enumerate().map(|(index, (argb, name))| {
                    self.swatch(dialog, palette, (index, argb, name), cx)
                }),
            ))
            .child(
                div().flex().child(
                    div()
                        .id("folder-dialog-preview")
                        .debug_selector(|| "folder-dialog-preview".into())
                        .max_w_full()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(palette.border)
                        .bg(color.opacity(if dialog.color.is_some() { 0.12 } else { 0. }))
                        .child(Icon::Folder.element(px(16.), color))
                        .child(
                            label(palette, TextRole::Caption, preview)
                                .min_w_0()
                                .truncate(),
                        ),
                ),
            )
            .into_any_element()
    }

    /// Рисует круг цвета с отдельными состояниями выбора и клавиатурного фокуса.
    fn swatch(
        &self,
        dialog: &OpenDialog,
        palette: Palette,
        (index, argb, name): (usize, u32, &'static str),
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let selected = dialog.color == Some(argb);
        div()
            .id(("folder-dialog-color", index))
            .debug_selector(move || format!("folder-dialog-color-{index}"))
            .size(px(32.))
            .rounded_full()
            .border_2()
            .border_color(if selected {
                palette.text
            } else {
                palette.border
            })
            .bg(rgb(argb & 0x00ffffff))
            .map(|swatch| {
                if !dialog.status.fields_enabled() {
                    return swatch.opacity(0.58);
                }
                swatch
                    .focusable()
                    .tab_stop(true)
                    .focus(|style| style.border_color(palette.focus))
                    .cursor_pointer()
                    .active(|style| style.opacity(0.7))
                    .on_click(cx.listener(move |view, _, _, cx| view.choose_color(argb, cx)))
                    .tooltip(move |_, app| popup::tooltip(palette, name.into(), app))
            })
    }

    /// Сообщает о превышении длины или о результате, который требует внимания.
    fn notice(
        &self,
        dialog: &OpenDialog,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        let text = match dialog.status {
            Status::Error(FolderCommandError::Unknown) if dialog.can_retry_create() => {
                "Результат не подтверждён. Можно повторить то же действие."
            }
            Status::Error(FolderCommandError::Unknown) => {
                "Результат неизвестен. Обновите папки перед повтором."
            }
            Status::Error(FolderCommandError::Rejected) => "Сервер отклонил изменение.",
            Status::Error(FolderCommandError::Unavailable) => "Эта папка или сессия недоступна.",
            Status::Error(FolderCommandError::Refresh) => {
                "Изменение принято. Обновить список не удалось."
            }
            Status::Error(FolderCommandError::WorkerUnavailable) => {
                "Не удалось отправить команду. Попробуйте снова."
            }
            _ if self.name.read(cx).text().trim().chars().count() > MAX_TITLE_LENGTH => {
                "В названии может быть до 64 символов."
            }
            _ => return None,
        };
        Some(
            label(palette, TextRole::Caption, text)
                .text_color(palette.danger)
                .id("folder-dialog-notice")
                .debug_selector(|| "folder-dialog-notice".into()),
        )
    }

    /// Показывает отмену и подтверждение с доступностью по состоянию запроса.
    fn footer(&self, dialog: &OpenDialog, palette: Palette, cx: &Context<Self>) -> Div {
        let pending = matches!(dialog.status, Status::Pending);
        let title = match (&dialog.mode, &dialog.status) {
            (Mode::Create(_), Status::Error(FolderCommandError::Unknown)) => "Повторить",
            (Mode::Create(_), Status::Pending) => "Создание…",
            (Mode::Edit { .. }, Status::Pending) => "Сохранение…",
            (Mode::Delete { .. }, Status::Pending) => "Удаление…",
            (Mode::Create(_), _) => "Создать",
            (Mode::Edit { .. }, _) => "Сохранить",
            (Mode::Delete { .. }, _) => "Удалить",
        };
        let dismiss_label = if matches!(dialog.status, Status::Error(FolderCommandError::Unknown))
            && !dialog.can_retry_create()
        {
            "Закрыть"
        } else {
            "Отмена"
        };
        let mut cancel = Button::new("folder-dialog-cancel", dismiss_label);
        cancel.disabled = pending;
        let mut submit = Button::new("folder-dialog-submit", title);
        submit.appearance = ButtonAppearance::Primary;
        submit.disabled = !self.can_submit(cx);
        let submit_palette = if matches!(dialog.mode, Mode::Delete { .. }) {
            Palette {
                accent: palette.danger.opacity(0.14),
                on_accent: palette.danger,
                ..palette
            }
        } else {
            palette
        };
        div()
            .flex()
            .justify_end()
            .gap(px(8.))
            .child(
                self.button(cancel, palette, &self.cancel, Self::dismiss, cx)
                    .debug_selector(|| "folder-dialog-cancel".into()),
            )
            .child(
                self.button(submit, submit_palette, &self.submit, Self::submit, cx)
                    .debug_selector(|| "folder-dialog-submit".into()),
            )
    }

    /// Связывает кнопку UI-кита с действием одного диалога.
    fn button(
        &self,
        button: Button,
        palette: Palette,
        focus: &gpui::FocusHandle,
        action: fn(&mut Self, &mut Context<Self>),
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let view = cx.entity().downgrade();
        button.render(palette, Some(focus), move |_, app| {
            let _ = view.update(app, action);
        })
    }
}
