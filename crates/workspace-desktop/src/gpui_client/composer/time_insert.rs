//! Открывает календарь у кнопки редактора и вставляет выбранное местное время как UTC.

use std::{cell::Cell, rc::Rc};

use gpui::{
    Bounds, Context, Corner, Deferred, Div, Entity, FocusHandle, Pixels, Subscription, Window,
    anchored, deferred, div, point, prelude::*, px,
};

use super::{Composer, toolbar::icon_button};
use crate::{
    gpui_client::{
        NextControl, PreviousControl,
        preferences::Language,
        ui::{
            Button, ButtonAppearance, DateTimePicker, DateTimePickerChanged, Icon, Palette,
            PickerOptions, TextRole, TimePrecision, label, popup,
        },
    },
    platform::message_time::{current_local_datetime, local_datetime_to_utc},
};

const PANEL_WIDTH: Pixels = px(371.);
const PANEL_GAP: Pixels = px(8.);
const WINDOW_MARGIN: Pixels = px(4.);

/// Держит календарь и цели фокуса до применения или закрытия формы.
pub(super) struct TimeInsert {
    picker: Entity<DateTimePicker>,   // Выбранные местные дата и время.
    anchor: Rc<Cell<Bounds<Pixels>>>, // Актуальные границы кнопки в окне.
    trigger: FocusHandle,             // Кнопка, куда возвращается фокус после отмены.
    pub(super) region: FocusHandle,   // Начало клавиатурного обхода календаря.
    confirm: FocusHandle,             // Последняя остановка обхода и действие вставки.
    invalid: bool,                    // Выбранное местное время не удалось перевести в UTC.
    pub(super) editor_version: u64,   // Версия текста, где пользователь выбрал точку вставки.
    _change: Subscription,            // Убирает ошибку после изменения даты или времени.
    _focus_out: Subscription,         // Закрывает календарь при уходе фокуса из него.
}

impl TimeInsert {
    /// Создаёт календарь с текущими местными значениями и фокусом на выбранном дне.
    fn new(
        anchor: Rc<Cell<Bounds<Pixels>>>,
        trigger: FocusHandle,
        language: Language,
        editor_version: u64,
        window: &mut Window,
        cx: &mut Context<Composer>,
    ) -> Self {
        let now = current_local_datetime();
        let picker = cx.new(|cx| {
            DateTimePicker::new(
                Some(now),
                now.date(),
                TimePrecision::Seconds,
                PickerOptions {
                    language,
                    ..PickerOptions::default()
                },
                window,
                cx,
            )
            .embedded()
        });
        picker.read(cx).focus_date(window, cx);
        let change = cx.subscribe(
            &picker,
            |composer, picker, _: &DateTimePickerChanged, cx| {
                if let Some(insert) = &mut composer.time_insert
                    && insert.picker == picker
                    && insert.invalid
                {
                    insert.invalid = false;
                    cx.notify();
                }
            },
        );
        let region = cx.focus_handle();
        let active_region = region.clone();
        let focus_out = cx.on_focus_out(&region, window, move |composer, _, _, cx| {
            if composer
                .time_insert
                .as_ref()
                .is_some_and(|insert| insert.region == active_region)
            {
                composer.time_insert = None;
                cx.notify();
            }
        });
        Self {
            picker,
            anchor,
            trigger,
            region,
            confirm: cx.focus_handle(),
            invalid: false,
            editor_version,
            _change: change,
            _focus_out: focus_out,
        }
    }

    /// Рисует форму поверх истории и добавляет кнопку, которая применяет выбор.
    fn render(
        &self,
        palette: Palette,
        language: Language,
        window: &Window,
        cx: &Context<Composer>,
    ) -> [Deferred; 2] {
        let mut confirm = Button::new(
            "composer-time-confirm",
            language.tr("desktop.messenger.composer.insertTimeConfirm"),
        );
        confirm.appearance = ButtonAppearance::Primary;
        let insert = cx.weak_entity();
        let viewport = window.viewport_size();
        let surface = popup::menu_surface(
            "composer-time-picker",
            palette,
            viewport,
            PANEL_WIDTH,
            viewport.height,
        )
        .p_0()
        .overflow_y_scroll()
        .track_focus(&self.region)
        .tab_index(0)
        .tab_stop(false)
        .debug_selector(|| "composer-time-picker".into())
        .on_mouse_down_out(cx.listener(|composer, _, window, cx| {
            composer.close_time_picker(window, cx);
        }))
        .capture_key_down(
            cx.listener(|composer, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                    cx.stop_propagation();
                    composer.close_time_picker(window, cx);
                }
            }),
        )
        .on_action(cx.listener(|composer, _: &NextControl, window, cx| {
            composer.cycle_time_focus(false, window, cx);
        }))
        .on_action(cx.listener(|composer, _: &PreviousControl, window, cx| {
            composer.cycle_time_focus(true, window, cx);
        }))
        .child(self.picker.clone())
        .child(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .gap_2()
                .px_4()
                .pb_4()
                .children(self.invalid.then(|| {
                    label(
                        palette,
                        TextRole::Caption,
                        language.tr("desktop.messenger.composer.timeInvalid"),
                    )
                    .text_color(palette.danger)
                }))
                .child(
                    confirm
                        .render(palette, Some(&self.confirm), move |window, app| {
                            let _ = insert.update(app, |composer, cx| {
                                composer.confirm_time(window, cx);
                            });
                        })
                        .h(px(40.))
                        .debug_selector(|| "composer-time-confirm".into()),
                ),
        );
        let anchor = self.anchor.get();
        let width = PANEL_WIDTH.min(viewport.width - WINDOW_MARGIN * 2.);
        let left = (anchor.center().x - width / 2.).clamp(
            WINDOW_MARGIN,
            (viewport.width - width - WINDOW_MARGIN).max(WINDOW_MARGIN),
        );
        let above = anchor.top();
        let below = viewport.height - anchor.bottom();
        let (corner, y) = if above >= below {
            (Corner::BottomLeft, anchor.top() - PANEL_GAP)
        } else {
            (Corner::TopLeft, anchor.bottom() + PANEL_GAP)
        };
        let backdrop = div().w(viewport.width).h(viewport.height).occlude();
        [
            deferred(anchored().position(point(px(0.), px(0.))).child(backdrop)).with_priority(1),
            deferred(
                anchored()
                    .anchor(corner)
                    .position(point(left, y))
                    .snap_to_window_with_margin(WINDOW_MARGIN)
                    .child(surface),
            )
            .with_priority(2),
        ]
    }
}

impl Composer {
    /// Проверяет, осталась ли форма открытой после действия в тестовом окне.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_time_picker_is_open(&self) -> bool {
        self.time_insert.is_some()
    }

    /// Рисует часы в панели редактора и запоминает их положение после прокрутки.
    pub(super) fn time_button(
        &self,
        palette: Palette,
        language: Language,
        enabled: bool,
        focus: &FocusHandle,
        cx: &Context<Self>,
    ) -> Div {
        let target = cx.weak_entity();
        let open = self.time_insert.is_some();
        let anchor = self
            .time_insert
            .as_ref()
            .map_or_else(Rc::default, |picker| picker.anchor.clone());
        let trigger_anchor = anchor.clone();
        let trigger_focus = focus.clone();
        let palette = if open {
            Palette {
                icon: palette.icon_hover,
                ..palette
            }
        } else {
            palette
        };
        let button = icon_button(
            "composer-insert-time",
            language.tr("desktop.messenger.composer.insertTime"),
            (Icon::ComposerTime, [24.; 2]),
            palette,
            focus,
            !enabled || !self.is_available(),
            move |window, app| {
                let _ = target.update(app, |composer, cx| {
                    composer.toggle_time_picker(
                        trigger_anchor.clone(),
                        trigger_focus.clone(),
                        window,
                        cx,
                    );
                });
            },
        );
        div()
            .flex_none()
            .child(button)
            .on_children_prepainted(move |children, window, app| {
                if let Some(&bounds) = children.first()
                    && anchor.replace(bounds) != bounds
                    && open
                {
                    // Прокрутка панели и размер окна меняют привязку после расчёта раскладки.
                    window.defer(app, |window, _| window.refresh());
                }
            })
    }

    /// Показывает календарь в слое окна без пересчёта высоты редактора и истории.
    pub(super) fn render_time_picker(
        &self,
        palette: Palette,
        language: Language,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<[Deferred; 2]> {
        self.time_insert
            .as_ref()
            .map(|picker| picker.render(palette, language, window, cx))
    }

    /// Открывает форму у кнопки и сохраняет текущее выделение редактора.
    fn toggle_time_picker(
        &mut self,
        anchor: Rc<Cell<Bounds<Pixels>>>,
        trigger: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.time_insert.is_some() {
            self.close_time_picker(window, cx);
        } else if self.is_available() && self.editor.read(cx).can_format() {
            self.time_insert = Some(TimeInsert::new(
                anchor,
                trigger,
                self.language(cx),
                self.editor.read(cx).version(),
                window,
                cx,
            ));
            cx.notify();
        }
    }

    /// Закрывает форму и возвращает фокус к кнопке, которая её открыла.
    fn close_time_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = self.time_insert.take() {
            picker.trigger.focus(window);
            cx.notify();
        }
    }

    /// Соединяет первую и последнюю кнопки, удерживая обход Tab внутри формы.
    fn cycle_time_focus(&self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = &self.time_insert else {
            return;
        };
        cx.stop_propagation();
        if reverse {
            window.focus_prev();
            if !picker.region.contains_focused(window, cx) {
                picker.confirm.focus(window);
            }
        } else {
            window.focus_next();
            if !picker.region.contains_focused(window, cx) {
                picker.region.focus(window);
                window.focus_next();
            }
        }
    }

    /// Вставляет выбранный момент в Markdown и возвращает фокус к вводу сообщения.
    fn confirm_time(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_available() || !self.editor.read(cx).can_format() {
            self.close_time_picker(window, cx);
            return;
        }
        let Some(picker) = &mut self.time_insert else {
            return;
        };
        if picker.editor_version != self.editor.read(cx).version() {
            self.close_time_picker(window, cx);
            return;
        }
        let Some(time) = picker
            .picker
            .read(cx)
            .value_for_confirmation(cx)
            .and_then(local_datetime_to_utc)
        else {
            picker.invalid = true;
            cx.notify();
            return;
        };
        self.editor
            .update(cx, |editor, cx| editor.insert_time(time, cx));
        self.time_insert = None;
        self.focus_editor(window, cx);
        cx.notify();
    }
}
