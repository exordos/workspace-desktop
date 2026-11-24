//! Даёт выбрать местную дату и время и вставляет в текст переносимый момент.

use gpui::{Context, Div, Entity, FocusHandle, Stateful, Window, div, prelude::*, px};

use super::Composer;
use crate::{
    gpui_client::{
        preferences::Language,
        ui::{Button, ButtonAppearance, Icon, Palette, TextInput, TextRole, label, popup},
    },
    platform::message_time::{local_time_fields, parse_local_time},
};

/// Дата и время редактируются отдельно до подтверждения.
pub(super) struct TimeInsert {
    date: Entity<TextInput>,  // Дата в формате YYYY-MM-DD.
    clock: Entity<TextInput>, // Местные часы, минуты и необязательные секунды.
    invalid: bool,            // Последняя попытка вставить момент не прошла проверку.
}

impl TimeInsert {
    /// Создаёт форму с текущими местными значениями.
    fn new(cx: &mut Context<Composer>) -> Self {
        let (date, clock) = local_time_fields();
        let date = cx.new(|cx| {
            let mut field = TextInput::new("composer-time-date", "YYYY-MM-DD", false, cx);
            field.set_text(&date, cx);
            field
        });
        let clock = cx.new(|cx| {
            let mut field = TextInput::new("composer-time-clock", "HH:MM:SS", false, cx);
            field.set_text(&clock, cx);
            field
        });
        Self {
            date,
            clock,
            invalid: false,
        }
    }

    /// Рисует форму выбора рядом с полем этого редактора.
    pub(super) fn render(
        &self,
        palette: Palette,
        language: Language,
        cx: &Context<Composer>,
    ) -> Stateful<Div> {
        let mut confirm = Button::new(
            "composer-time-confirm",
            language.tr("desktop.messenger.composer.insertTime"),
        );
        confirm.appearance = ButtonAppearance::Primary;
        let insert = cx.entity().downgrade();
        let cancel = cx.entity().downgrade();
        div()
            .id("composer-time-picker")
            .debug_selector(|| "composer-time-picker".into())
            .p_2()
            .gap_2()
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(palette.border)
            .on_key_down(
                cx.listener(|composer, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                        cx.stop_propagation();
                        composer.close_time_picker(window, cx);
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(labelled_field(
                        language.tr("desktop.messenger.composer.timeDate"),
                        self.date.clone(),
                        palette,
                    ))
                    .child(labelled_field(
                        language.tr("desktop.messenger.composer.timeClock"),
                        self.clock.clone(),
                        palette,
                    )),
            )
            .children(self.invalid.then(|| {
                label(
                    palette,
                    TextRole::Caption,
                    language.tr("desktop.messenger.composer.timeInvalid"),
                )
                .text_color(palette.danger)
            }))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new(
                            "composer-time-cancel",
                            language.tr("desktop.messenger.composer.timeCancel"),
                        )
                        .render(palette, None, move |window, app| {
                            let _ = cancel
                                .update(app, |composer, cx| composer.close_time_picker(window, cx));
                        }),
                    )
                    .child(
                        confirm
                            .render(palette, None, move |window, app| {
                                let _ = insert
                                    .update(app, |composer, cx| composer.confirm_time(window, cx));
                            })
                            .debug_selector(|| "composer-time-confirm".into()),
                    ),
            )
    }
}

/// Показывает, какое значение ожидается в редактируемом поле.
fn labelled_field(title: &'static str, field: Entity<TextInput>, palette: Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .flex_1()
        .gap_1()
        .child(label(palette, TextRole::Caption, title))
        .child(field)
}

impl Composer {
    /// Рисует короткую команду часов в раскрытой панели.
    pub(super) fn time_button(
        &self,
        palette: Palette,
        language: Language,
        enabled: bool,
        focus: &FocusHandle,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let target = cx.entity().downgrade();
        let mut button = Button::new("composer-insert-time", "");
        button.icon = Some(Icon::Pending);
        button.appearance = ButtonAppearance::Ghost;
        button.disabled = !enabled;
        button
            .render(palette, Some(focus), move |window, app| {
                let _ = target.update(app, |composer, cx| composer.toggle_time_picker(window, cx));
            })
            .size(px(32.))
            .p_0()
            .flex_none()
            .debug_selector(|| "composer-insert-time".into())
            .tooltip(move |_, app| {
                popup::tooltip(
                    palette,
                    language.tr("desktop.messenger.composer.insertTime").into(),
                    app,
                )
            })
    }

    /// Открывает форму и переносит фокус в дату, сохранив точку вставки редактора.
    fn toggle_time_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.time_insert.is_some() {
            self.close_time_picker(window, cx);
        } else if self.is_available() && self.editor.read(cx).can_format() {
            let insert = TimeInsert::new(cx);
            insert.date.read(cx).focus(window);
            self.time_insert = Some(insert);
            cx.notify();
        }
    }

    /// Закрывает форму и возвращает точку ввода к черновику.
    fn close_time_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.time_insert = None;
        self.focus_editor(window, cx);
        cx.notify();
    }

    /// Проверяет дату и время и вставляет UTC через обычную замену текста.
    fn confirm_time(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_available() {
            self.close_time_picker(window, cx);
            return;
        }
        let Some(picker) = &mut self.time_insert else {
            return;
        };
        let date = picker.date.read(cx);
        let clock = picker.clock.read(cx);
        if date.is_composing() || clock.is_composing() {
            return;
        }
        let Some(time) = parse_local_time(date.text(), clock.text()) else {
            picker.invalid = true;
            cx.notify();
            return;
        };
        self.editor
            .update(cx, |editor, cx| editor.insert_time(time, cx));
        self.close_time_picker(window, cx);
    }
}
