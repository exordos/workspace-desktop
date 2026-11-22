//! Показывает команды системного буфера и выделения в рабочем редакторе сообщения.
//! Хранит положение меню, передаёт клавиши общему попапу и выполняет команды поля.

use gpui::{
    Context, Deferred, FocusHandle, MouseDownEvent, Pixels, Point, Window, point, prelude::*, px,
};

use super::Editor;
use crate::gpui_client::ui::{
    Button, ButtonAppearance, Palette,
    popup::{MenuInput, menu_layer, menu_navigation, menu_surface},
};

/// Состояние меню и отдельный фокус, который не принимает текстовый ввод.
pub(super) struct TextMenu {
    focus: FocusHandle, // Меню принимает клавиши, пока редактор хранит выделение.
    anchor: Option<Point<Pixels>>, // Координаты в окне или None, если меню закрыто.
    selected: Option<Command>, // Команда, подсвеченная для подтверждения с клавиатуры.
    busy: bool,         // Композер сейчас сохраняет отправленный снимок.
}

impl TextMenu {
    /// Готовит закрытое меню без выбранной команды.
    pub(super) fn new(cx: &mut Context<Editor>) -> Self {
        Self {
            focus: cx.focus_handle(),
            anchor: None,
            selected: None,
            busy: false,
        }
    }

    /// Проверяет, передан ли клавиатурный ввод меню.
    pub(super) fn is_open(&self) -> bool {
        self.anchor.is_some()
    }
}

/// Действия в том же порядке, что и в Electron.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Cut,       // Перенести выделение в системный буфер и удалить его из поля.
    Copy,      // Копировать выделение без изменения текста.
    Paste,     // Заменить выделение содержимым системного буфера.
    SelectAll, // Выделить весь текущий текст.
}

impl Command {
    const ALL: [Self; 4] = [Self::Cut, Self::Copy, Self::Paste, Self::SelectAll];

    /// Подпись команды в меню системного редактирования.
    fn label(self) -> &'static str {
        match self {
            Self::Cut => "Вырезать",
            Self::Copy => "Копировать",
            Self::Paste => "Вставить",
            Self::SelectAll => "Выбрать всё",
        }
    }

    /// Стабильное имя строки для сквозных проверок.
    fn selector(self) -> &'static str {
        match self {
            Self::Cut => "composer-text-cut",
            Self::Copy => "composer-text-copy",
            Self::Paste => "composer-text-paste",
            Self::SelectAll => "composer-text-select-all",
        }
    }
}

impl Editor {
    /// Проверяет закрытие меню через рабочее поле после события в окне.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn text_menu_is_open(&self) -> bool {
        self.text_menu.is_open()
    }

    /// Согласует доступность меню с сохранением отправленного снимка.
    pub(in crate::gpui_client::composer) fn set_text_menu_busy(
        &mut self,
        busy: bool,
        cx: &mut Context<Self>,
    ) {
        self.text_menu.busy = busy;
        cx.notify();
    }

    /// Открывает меню у указателя, сохраняя выделение и его направление.
    pub(super) fn open_text_menu_at_pointer(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.open_text_menu(event.position, window, cx);
    }

    /// Открывает меню внутри поля для системной клавиши и Shift+F10.
    pub(super) fn open_text_menu_from_keyboard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Координаты поля берём из видимого кадра. Быстрый набор мог уже изменить текст.
        let Some(layout) = &self.layout else {
            return;
        };
        self.open_text_menu(layout.bounds.origin + point(px(12.), px(12.)), window, cx);
    }

    /// Передаёт фокус меню только после завершения системной композиции.
    fn open_text_menu(
        &mut self,
        anchor: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.composing() {
            return;
        }
        self.dragging = false;
        self.text_menu.anchor = Some(anchor);
        self.text_menu.selected = Command::ALL
            .into_iter()
            .find(|&command| self.text_command_enabled(command));
        window.focus(&self.text_menu.focus);
        cx.notify();
    }

    /// Выясняет, разрешена ли команда для текущего текста и выделения.
    fn text_command_enabled(&self, command: Command) -> bool {
        !self.text_menu.busy
            && !self.composing()
            && match command {
                Command::Cut | Command::Copy => !self.selection().is_empty(),
                Command::Paste => true,
                Command::SelectAll => !self.text.is_empty(),
            }
    }

    /// Закрывает меню, сохраняя выделение. При клике снаружи фокус получает цель клика.
    fn close_text_menu(
        &mut self,
        restore_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.text_menu.anchor.take().is_some() {
            if restore_focus {
                window.focus(&self.focus);
            } else if self.text_menu.focus.is_focused(window) {
                window.blur();
            }
            cx.notify();
        }
    }

    /// Возвращает фокус в поле и выполняет ту же операцию, что и клавиатурное сочетание.
    fn run_text_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        if self.text_menu.is_open() && self.text_command_enabled(command) {
            self.close_text_menu(true, window, cx);
            match command {
                Command::Cut => self.copy(true, cx),
                Command::Copy => self.copy(false, cx),
                Command::Paste => self.paste(cx),
                Command::SelectAll => self.select_all(cx),
            }
        }
    }

    /// Перемещает подсветку только между доступными командами.
    fn text_menu_input(&mut self, input: MenuInput, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.text_menu.selected;
        let enabled: Vec<_> = Command::ALL
            .into_iter()
            .filter(|&command| self.text_command_enabled(command))
            .collect();
        let index = enabled
            .iter()
            .position(|&command| Some(command) == selected);
        let next = match input {
            MenuInput::First => enabled.first(),
            MenuInput::Last => enabled.last(),
            MenuInput::Next => enabled.get(index.map_or(0, |index| index + 1)),
            MenuInput::Previous => match index {
                Some(index) => index.checked_sub(1).and_then(|index| enabled.get(index)),
                None => enabled.last(),
            },
            MenuInput::Close => {
                self.close_text_menu(true, window, cx);
                return;
            }
            MenuInput::Select => {
                if let Some(command) = selected {
                    self.run_text_command(command, window, cx);
                }
                return;
            }
        };
        if let Some(&command) = next {
            self.text_menu.selected = Some(command);
            cx.notify();
        }
    }

    /// Использует общие стили и управление меню поверх переполненной рамки поля.
    pub(super) fn render_text_menu(&self, window: &Window, cx: &Context<Self>) -> Option<Deferred> {
        let anchor = self.text_menu.anchor?;
        let viewport = window.viewport_size();
        let palette = Palette::for_window(window, cx);
        let target = cx.entity().downgrade();
        let surface = menu_surface("composer-text-menu", palette, viewport, px(180.), px(200.))
            .debug_selector(|| "composer-text-menu".into())
            .overflow_y_scroll()
            .on_mouse_down_out(cx.listener(|editor, _, window, cx| {
                editor.close_text_menu(false, window, cx);
            }))
            .children(Command::ALL.map(|command| {
                let mut button = Button::new(command.selector(), command.label());
                button.appearance = ButtonAppearance::Ghost;
                button.disabled = !self.text_command_enabled(command);
                let selected = !button.disabled && self.text_menu.selected == Some(command);
                let target = target.clone();
                button
                    .render(palette, None, move |window, app| {
                        let _ = target.update(app, |editor, cx| {
                            editor.run_text_command(command, window, cx);
                        });
                    })
                    .w_full()
                    .min_w_0()
                    .justify_start()
                    .flex_none()
                    .when(selected, |row| row.bg(palette.card_selected))
                    .debug_selector(move || command.selector().into())
            }));
        let surface = menu_navigation(surface, &self.text_menu.focus, cx, Self::text_menu_input);
        // Контекстное меню немодальное. Клик снаружи закрывает его и проходит к элементу под ним.
        let [_, menu] = menu_layer(viewport, anchor, surface);
        Some(menu)
    }
}
