//! Собирает раскрывающийся ряд команд редактора и показывает сфокусированную кнопку при прокрутке.

use std::{cell::Cell, rc::Rc};

use gpui::{
    AnyElement, Context, Div, FocusHandle, Pixels, ScrollHandle, Stateful, Window, div, prelude::*,
    px,
};

use super::{COMPOSER_INSET, Composer, ToolbarPreferences, editor::Format};
use crate::gpui_client::{
    preferences::Language,
    ui::{Button, ButtonAppearance, Icon, Palette, popup},
};

/// Команда ряда с подписью и размерами иконки из референса.
struct Command {
    format: Format,          // Изменение Markdown в редакторе.
    icon: Icon,              // Глиф для кнопки команды.
    glyph: [f32; 2],         // Ширина и высота глифа в логических пикселях.
    id: &'static str,        // Устойчивое имя кнопки для маршрута фокуса и тестов.
    title_key: &'static str, // Ключ названия действия в подсказке.
}

impl Command {
    /// Описывает вид одной команды без состояния редактора.
    const fn new(
        format: Format,
        icon: Icon,
        glyph: [f32; 2],
        id: &'static str,
        title_key: &'static str,
    ) -> Self {
        Self {
            format,
            icon,
            glyph,
            id,
            title_key,
        }
    }
}

const COMMANDS: [Command; 10] = [
    Command::new(
        Format::Link,
        Icon::ComposerLink,
        [24., 14.769],
        "composer-format-link",
        "desktop.messenger.composer.insertLink",
    ),
    Command::new(
        Format::Bold,
        Icon::ComposerBold,
        [10.93, 16.667],
        "composer-format-bold",
        "composer.bold",
    ),
    Command::new(
        Format::Italic,
        Icon::ComposerItalic,
        [15.448, 16.747],
        "composer-format-italic",
        "composer.italic",
    ),
    Command::new(
        Format::Strike,
        Icon::ComposerStrike,
        [25.333, 19.79],
        "composer-format-strike",
        "composer.strikethrough",
    ),
    Command::new(
        Format::NumberedList,
        Icon::ComposerNumberedList,
        [21.333, 24.],
        "composer-format-numbered",
        "composer.numberedList",
    ),
    Command::new(
        Format::BulletedList,
        Icon::ComposerBulletedList,
        [21.328, 19.533],
        "composer-format-bulleted",
        "composer.bulletedList",
    ),
    Command::new(
        Format::Quote,
        Icon::ComposerQuote,
        [24., 21.38],
        "composer-format-quote",
        "composer.quote",
    ),
    Command::new(
        Format::CodeBlock,
        Icon::ComposerCodeBlock,
        [21.333, 21.333],
        "composer-format-block",
        "composer.codeBlock",
    ),
    Command::new(
        Format::InlineCode,
        Icon::ComposerInlineCode,
        [22., 18.],
        "composer-format-inline",
        "composer.code",
    ),
    Command::new(
        Format::Spoiler,
        Icon::ComposerSpoiler,
        [25.834, 17.333],
        "composer-format-spoiler",
        "composer.spoiler",
    ),
];
const STYLE_GROUP_END: usize = 4;
const DIVIDERS: [usize; 2] = [STYLE_GROUP_END, 6];
const TOOLBAR_GAP: Pixels = px(8.);
const EXTRA_COMMANDS: usize = 3;

/// Команды панели, которые используют состояние композера.
pub(super) struct ToolbarExtras {
    pub file: Option<Stateful<Div>>, // Вложение в подтверждённом обычном разговоре.
    pub time: Div,                   // Вставка даты и времени в текст.
    pub fragment: Option<Stateful<Div>>, // Защита выделения в поддерживаемом разговоре.
}

/// Устойчивый фокус и позиция прокрутки команд одного редактора.
pub(super) struct Toolbar {
    toggle: FocusHandle,         // Кнопка раскрытия переезжает из поля в ряд.
    commands: [FocusHandle; 10], // Кнопки сохраняют фокус при перерисовке.
    extras: [FocusHandle; EXTRA_COMMANDS], // Фокус для файла, времени и защищённого фрагмента.
    scroll: ScrollHandle,        // Показывает команды за правой границей узкого окна.
    last_focused: Option<usize>, // Дочерний элемент, который уже показан прокруткой.
    last_width: Rc<Cell<Pixels>>, // Фактическая ширина ряда после расчёта размеров.
}

impl Toolbar {
    /// Готовит устойчивые фокусы до первого кадра панели.
    pub(super) fn new(cx: &mut Context<Composer>) -> Self {
        Self {
            toggle: cx.focus_handle(),
            commands: std::array::from_fn(|_| cx.focus_handle()),
            extras: std::array::from_fn(|_| cx.focus_handle()),
            scroll: ScrollHandle::new(),
            last_focused: None,
            last_width: Rc::default(),
        }
    }

    /// Передаёт устойчивый фокус кнопкам, для которых доступность определяет редактор.
    pub(super) fn extra_focus(&self) -> [FocusHandle; EXTRA_COMMANDS] {
        self.extras.clone()
    }

    /// Возвращает кнопку раскрытия в поле или в начале панели.
    pub(super) fn toggle(
        &self,
        expanded: bool,
        palette: Palette,
        language: Language,
        cx: &Context<Composer>,
    ) -> Stateful<Div> {
        let (key, icon) = if expanded {
            ("composer.collapseToolbar", Icon::ComposerToolbar)
        } else {
            ("composer.expandToolbar", Icon::ComposerToolbarExpand)
        };
        let target = cx.entity().downgrade();
        icon_button(
            "composer-toolbar-toggle",
            language.tr(key),
            (icon, [21.333; 2]),
            palette,
            &self.toggle,
            false,
            move |window, app| {
                let _ = target.update(app, |composer, cx| {
                    ToolbarPreferences::set_expanded(!expanded, cx);
                    // У раскрытой и компактной кнопок разное место, поэтому ввод остаётся в поле.
                    composer.focus_editor(window, cx);
                    cx.notify();
                });
            },
        )
    }

    /// Рисует одну горизонтальную полосу и делает скрытую команду видимой при Tab.
    pub(super) fn row(
        &mut self,
        palette: Palette,
        can_format: bool,
        language: Language,
        extras: ToolbarExtras,
        window: &Window,
        cx: &Context<Composer>,
    ) -> Stateful<Div> {
        let [file_focus, time_focus, fragment_focus] = &self.extras;
        let mut items: Vec<(AnyElement, Option<&FocusHandle>)> = vec![(
            self.toggle(true, palette, language, cx).into_any_element(),
            Some(&self.toggle),
        )];
        if let Some(file) = extras.file {
            items.push((file.ml(TOOLBAR_GAP).into_any_element(), Some(file_focus)));
            items.push((separator(palette).into_any_element(), None));
        }
        items.push((
            extras.time.ml(TOOLBAR_GAP).into_any_element(),
            Some(time_focus),
        ));
        items.push((separator(palette).into_any_element(), None));

        let target = cx.entity().downgrade();
        for (index, command) in COMMANDS.iter().enumerate() {
            if DIVIDERS.contains(&index) {
                items.push((separator(palette).into_any_element(), None));
            }
            // Ссылка, жирный, курсив и зачёркивание стоят вплотную внутри одной группы.
            let gap = if (1..STYLE_GROUP_END).contains(&index) {
                px(0.)
            } else {
                TOOLBAR_GAP
            };
            let target = target.clone();
            let focus = &self.commands[index];
            let button = icon_button(
                command.id,
                language.tr(command.title_key),
                (command.icon, command.glyph),
                palette,
                focus,
                !can_format,
                move |window, app| {
                    let _ = target.update(app, |composer, cx| {
                        composer
                            .editor
                            .update(cx, |editor, cx| editor.apply_format(command.format, cx));
                        composer.focus_editor(window, cx);
                    });
                },
            );
            items.push((button.ml(gap).into_any_element(), Some(focus)));
        }
        if let Some(fragment) = extras.fragment {
            items.push((
                fragment.ml(TOOLBAR_GAP).into_any_element(),
                Some(fragment_focus),
            ));
        }

        // Индекс прокрутки включает разделители и зависит от того, какие кнопки видны.
        let focused = items
            .iter()
            .enumerate()
            .filter_map(|(index, (_, focus))| focus.map(|focus| (index, focus)))
            .find(|(_, focus)| focus.is_focused(window))
            .map(|(index, focus)| (index, focus.clone()));
        let focused_index = focused.as_ref().map(|(index, _)| *index);
        if self.last_focused != focused_index {
            self.last_focused = focused_index;
            if let Some(index) = focused_index {
                self.scroll.scroll_to_item(index);
            }
        }
        let scroll = self.scroll.clone();
        let last_width = self.last_width.clone();
        div()
            .on_children_prepainted(move |_, window, app| {
                let width = scroll.bounds().size.width;
                let resized = last_width.replace(width) != width;
                if resized && let Some((index, focus)) = focused.clone() {
                    let scroll = scroll.clone();
                    // Размер ряда известен после размещения. Перепроверяем фокус следующим кадром,
                    // в том числе если ширину изменило открытие боковой панели.
                    window.defer(app, move |window, _| {
                        if focus.is_focused(window) {
                            scroll.scroll_to_item(index);
                            window.refresh();
                        }
                    });
                }
            })
            .id("composer-toolbar")
            .debug_selector(|| "composer-toolbar".into())
            .h(px(32.))
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .pl(COMPOSER_INSET)
            .overflow_x_scroll()
            .track_scroll(&self.scroll)
            .children(items.into_iter().map(|(element, _)| element))
    }
}

/// Отделяет группы команд вертикальной линией по макету.
fn separator(palette: Palette) -> Div {
    div()
        .ml(TOOLBAR_GAP)
        .w(px(1.))
        .h(px(28.))
        .flex_none()
        .bg(palette.border)
}

/// Рисует доступную клавиатуре кнопку, используя общие состояния и подсказку.
pub(super) fn icon_button(
    id: &'static str,
    title: &'static str,
    (icon, [width, height]): (Icon, [f32; 2]),
    palette: Palette,
    focus: &FocusHandle,
    disabled: bool,
    activate: impl Fn(&mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    let mut button = Button::new(id, "");
    button.appearance = ButtonAppearance::Ghost;
    button.disabled = disabled;
    let color = if disabled {
        palette.disabled
    } else {
        palette.icon
    };
    let toolbar_palette = Palette {
        card_hover: palette.icon_hover_fill.opacity(0.35),
        ..palette
    };
    button
        .render(toolbar_palette, Some(focus), activate)
        .debug_selector(move || id.into())
        .group(id)
        .size(px(32.))
        .flex_none()
        .p(px(0.))
        .child(
            icon.element(px(width), color)
                .h(px(height))
                .when(!disabled, |glyph| {
                    glyph.group_hover(id, |style| style.text_color(palette.icon_hover))
                }),
        )
        .tooltip(move |_, app| popup::tooltip(palette, title.into(), app))
}
