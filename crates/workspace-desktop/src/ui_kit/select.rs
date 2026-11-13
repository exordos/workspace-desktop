//! Поле выбора одного значения. Сначала идёт функция сборки, затем состояние поля,
//! управление мышью и клавиатурой и подключение общего всплывающего списка.

use iced::advanced::renderer::{self, Renderer as _};
use iced::advanced::widget::{Operation, Tree, operation::Focusable, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, overlay};
use iced::keyboard::{self, Key, key::Named};
use iced::widget::text::Wrapping;
use iced::widget::{container, row, text};
use iced::{
    Border, Center, Element, Event, Fill, Length, Pixels, Rectangle, Renderer, Size, Theme, Vector,
    mouse, touch, window,
};

use super::listbox::{ListboxOption, anchored_popup, listbox, listbox_edge, listbox_next};
use super::theme::{button_colors, text_muted};
use super::{CONTROL_RADIUS, CONTROL_TEXT_SIZE, FieldSize, IconName, IconTone, icon};

/// Показывает значение из готового списка. Без `on_change` поле отключено.
/// Экран хранит выбранное значение, а поле управляет раскрытием, подсветкой и фокусом.
pub fn select<'a, Message: Clone + 'static>(
    options: &'a [ListboxOption],
    value: Option<&'a str>,
    placeholder: &'a str,
    empty: &'a str,
    size: FieldSize,
    on_change: Option<impl Fn(&str) -> Message + 'a>,
) -> Element<'a, Message> {
    let label = value
        .and_then(|value| options.iter().find(|option| option.value == value))
        .map(|option| option.label.as_str());
    let inactive_text = label.is_none() || on_change.is_none();
    let label = label.unwrap_or(placeholder);
    Element::new(Select {
        options,
        value,
        empty,
        on_change: on_change.map(|change| Box::new(change) as Box<Change<'a, Message>>),
        closed: trigger(label, size, inactive_text, IconName::ChevronDown),
        opened: trigger(label, size, inactive_text, IconName::ChevronUp),
    })
}

/// Рисует подпись поля и резервирует место для стрелки даже у длинного значения.
fn trigger<'a, Message: 'a>(
    label: &'a str,
    size: FieldSize,
    inactive_text: bool,
    chevron: IconName,
) -> Element<'a, Message> {
    let label = text(label)
        .size(CONTROL_TEXT_SIZE)
        .line_height(Pixels(f32::from(FieldSize::LINE_HEIGHT)))
        .wrapping(Wrapping::None);
    let label = if inactive_text {
        label.style(|theme| iced::widget::text::Style {
            color: Some(text_muted(theme)),
        })
    } else {
        label
    };
    container(
        row![
            container(label).width(Fill).clip(true),
            icon(chevron, 16, IconTone::Muted, None),
        ]
        .spacing(8)
        .align_y(Center),
    )
    .width(Fill)
    .height(u32::from(size.height()))
    .align_y(Center)
    .padding([0, size.horizontal_padding()])
    .into()
}

/// Создаёт сообщение экрана со значением выбранного пункта.
type Change<'a, Message> = dyn Fn(&str) -> Message + 'a;

/// Данные поля, доступные во время текущего прохода отрисовки.
struct Select<'a, Message> {
    options: &'a [ListboxOption], // Варианты, переданные экраном.
    value: Option<&'a str>,       // Выбранное значение или `None`.
    empty: &'a str,               // Текст внутри пустого списка.
    on_change: Option<Box<Change<'a, Message>>>, // При `None` поле недоступно.
    closed: Element<'a, Message>, // Содержимое поля со стрелкой вниз.
    opened: Element<'a, Message>, // Содержимое поля со стрелкой вверх.
}

/// Состояние на время жизни поля Iced. Оно сохраняется между отрисовками.
struct State {
    focused: bool,          // Поле принимает клавиатурный ввод.
    pressed: bool,          // Нажатие началось в границах поля.
    open: bool,             // Список сейчас раскрыт.
    active: Option<String>, // Ключ подсвеченного пункта или `None`.
    popup: Tree,            // Состояние прокрутки и строк списка между событиями.
}

impl State {
    /// Закрывает список и очищает его состояние перед следующим открытием.
    fn close(&mut self) {
        self.open = false;
        self.active = None;
        self.popup = Tree::empty();
    }
}

impl Focusable for State {
    /// Возвращает состояние фокуса для обхода клавишей Tab.
    fn is_focused(&self) -> bool {
        self.focused
    }

    /// Передаёт полю клавиатурный фокус.
    fn focus(&mut self) {
        self.focused = true;
    }

    /// Снимает фокус и закрывает список.
    fn unfocus(&mut self) {
        self.focused = false;
        self.pressed = false;
        self.close();
    }
}

impl<Message> Select<'_, Message> {
    /// Проверяет, что ключ ещё есть среди доступных вариантов.
    fn available(&self, value: Option<&str>) -> bool {
        self.options
            .iter()
            .any(|option| !option.disabled && Some(option.value.as_str()) == value)
    }

    /// Находит первый активный пункт. Выбранный пункт имеет приоритет, если доступен.
    fn show(&self, state: &mut State, last: bool) {
        let selected = self.value.filter(|value| self.available(Some(value)));
        state.active = selected
            .or_else(|| listbox_edge(self.options, last))
            .map(str::to_owned);
        state.open = true;
    }

    /// Обрабатывает клавишу поля. Возвращает `false`, если ввод нужен другим виджетам.
    fn handle_key(
        &self,
        state: &mut State,
        key: Named,
        repeat: bool,
        shell: &mut Shell<'_, Message>,
    ) -> bool {
        match key {
            Named::ArrowUp | Named::ArrowDown => {
                let backwards = key == Named::ArrowUp;
                if state.open {
                    state.active = listbox_next(self.options, state.active.as_deref(), backwards)
                        .map(str::to_owned);
                } else {
                    self.show(state, backwards);
                }
            }
            Named::Home | Named::End if state.open => {
                state.active = listbox_edge(self.options, key == Named::End).map(str::to_owned);
            }
            Named::Enter | Named::Space => {
                if repeat {
                    return true;
                }
                if !state.open {
                    self.show(state, false);
                } else if let Some(active) = state.active.as_deref()
                    && self.available(Some(active))
                    && let Some(change) = &self.on_change
                {
                    shell.publish(change(active));
                    state.close();
                }
            }
            Named::Escape if state.open => state.close(),
            _ => return false,
        }
        true
    }
}

impl<Message: Clone + 'static> Widget<Message, Theme, Renderer> for Select<'_, Message> {
    /// Даёт Iced отдельный тег для хранения состояния поля.
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    /// Создаёт закрытое поле без фокуса.
    fn state(&self) -> tree::State {
        tree::State::new(State {
            focused: false,
            pressed: false,
            open: false,
            active: None,
            popup: Tree::empty(),
        })
    }

    /// Создаёт состояние разметки подписи и стрелки.
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.closed)]
    }

    /// Обновляет подпись и убирает недействительный активный пункт после смены опций.
    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.closed));
        let state = tree.state.downcast_mut::<State>();
        if self.on_change.is_none() {
            state.unfocus();
        } else if state.open && !self.available(state.active.as_deref()) {
            state.active = listbox_edge(self.options, false).map(str::to_owned);
        }
    }

    /// Берёт ширину и высоту из разметки содержимого поля.
    fn size(&self) -> Size<Length> {
        self.closed.as_widget().size()
    }

    /// Размещает подпись и стрелку в границах поля.
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.closed
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    /// Добавляет доступное поле в порядок обхода Tab и передаёт Iced текст подписи.
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        if self.on_change.is_some() {
            operation.focusable(None, layout.bounds(), tree.state.downcast_mut::<State>());
        }
        self.closed
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    /// Открывает список по нажатию, обрабатывает клавиши и освобождает фокус при уходе.
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if self.on_change.is_none() {
            return;
        }
        let state = tree.state.downcast_mut::<State>();
        let pointer = match event {
            Event::Touch(
                touch::Event::FingerPressed { position, .. }
                | touch::Event::FingerLifted { position, .. },
            ) => Some(*position),
            _ => cursor.position(),
        };
        let inside = pointer.is_some_and(|position| {
            layout.bounds().contains(position) && viewport.contains(position)
        });
        let mut relayout = false;
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerPressed { .. }) => {
                state.pressed = inside;
                if inside {
                    state.focus();
                    shell.capture_event();
                } else if state.focused {
                    relayout = state.open;
                    state.unfocus();
                }
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerLifted { .. })
                if state.pressed =>
            {
                state.pressed = false;
                if inside {
                    if state.open {
                        state.close();
                    } else {
                        self.show(state, false);
                    }
                    shell.capture_event();
                    relayout = true;
                }
                shell.request_redraw();
            }
            Event::Touch(touch::Event::FingerLost { .. })
            | Event::Window(window::Event::Unfocused) => {
                relayout = state.open;
                state.unfocus();
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => shell.request_redraw(),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: Key::Named(key),
                modifiers,
                repeat,
                ..
            }) if state.focused
                && !modifiers.control()
                && !modifiers.alt()
                && !modifiers.logo() =>
            {
                if *key == Named::Tab && state.open {
                    state.close();
                    relayout = true;
                } else if self.handle_key(state, *key, *repeat, shell) {
                    shell.capture_event();
                    relayout = true;
                }
            }
            _ => {}
        }
        if relayout {
            shell.invalidate_layout();
            shell.request_redraw();
        }
    }

    /// Рисует фон, рамку и соответствующее раскрытию направление стрелки.
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let state = tree.state.downcast_ref::<State>();
        let colors = button_colors(theme);
        let palette = theme.palette();
        let enabled = self.on_change.is_some();
        let hovered = enabled && cursor.is_over(bounds) && cursor.is_over(*viewport);
        let border_color = if state.focused || state.open {
            palette.primary
        } else {
            colors.border_subtle
        };
        let background = if !enabled {
            colors.card.scale_alpha(0.5)
        } else if state.pressed || hovered {
            colors.card_active
        } else {
            colors.elevated
        };
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: Border::default()
                    .rounded(CONTROL_RADIUS)
                    .width(if state.focused { 2 } else { 1 })
                    .color(border_color),
                ..renderer::Quad::default()
            },
            background,
        );

        let content = if state.open {
            &self.opened
        } else {
            &self.closed
        };
        content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    /// Меняет курсор над доступным полем.
    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.on_change.is_some() && cursor.is_over(layout.bounds()) && cursor.is_over(*viewport)
        {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::None
        }
    }

    /// Привязывает общий список к полю и закрывает его при выборе или клике снаружи.
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        _renderer: &Renderer,
        _viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        let state = tree.state.downcast_mut::<State>();
        let on_change = self.on_change.as_ref().filter(|_| state.open)?;
        let State {
            open,
            active,
            popup,
            ..
        } = state;
        let panel = listbox(
            self.options,
            self.value,
            active.as_deref(),
            self.empty,
            on_change,
        );
        Some(anchored_popup(
            layout.bounds() + translation,
            panel,
            popup,
            || *open = false,
        ))
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use iced::advanced::{clipboard, renderer::Headless};
    use iced::{Point, event};
    use iced_runtime::user_interface::{Cache, UserInterface};

    use super::*;

    /// Запускает Select в Iced с одним недоступным пунктом между доступными.
    /// Посылает события по одному, как при работе обычного нативного окна.
    fn scenario(
        enabled: bool,
        events: Vec<(Event, mouse::Cursor)>,
    ) -> (Vec<String>, Vec<event::Status>) {
        let options =
            [("a", false), ("b", true), ("c", false)].map(|(value, disabled)| ListboxOption {
                value: value.to_owned(),
                label: value.to_owned(),
                description: None,
                disabled,
            });
        let mut renderer = block_on(<Renderer as Headless>::new(
            iced::Font::DEFAULT,
            Pixels(14.0),
            Some("tiny-skia"),
        ))
        .unwrap();
        let view = select(
            &options,
            Some("a"),
            "Выбрать",
            "Пусто",
            FieldSize::Compact,
            enabled.then_some(str::to_owned),
        );
        let mut ui =
            UserInterface::build(view, Size::new(220.0, 220.0), Cache::new(), &mut renderer);
        let mut messages = Vec::new();
        let mut statuses = Vec::new();
        for (event, cursor) in events {
            let (_, result) = ui.update(
                &[event],
                cursor,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            statuses.extend(result);
        }
        (messages, statuses)
    }

    /// Создаёт входящее нажатие клавиши без модификаторов.
    fn key(named: Named) -> (Event, mouse::Cursor) {
        (
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: Key::Named(named),
                modified_key: Key::Named(named),
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            }),
            mouse::Cursor::Unavailable,
        )
    }

    /// Создаёт щелчок мыши с нажатием и отпусканием в одной точке.
    fn click(x: f32, y: f32) -> [(Event, mouse::Cursor); 2] {
        let cursor = mouse::Cursor::Available(Point::new(x, y));
        [
            (
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                cursor,
            ),
            (
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                cursor,
            ),
        ]
    }

    /// Escape оставляет фокус и отменяет выбор. Стрелки пропускают недоступный пункт.
    #[test]
    fn escape_cancels_and_arrows_skip_disabled_options() {
        let events = click(10.0, 16.0)
            .into_iter()
            .chain([
                key(Named::ArrowDown),
                key(Named::Escape),
                key(Named::Enter),
                key(Named::ArrowDown),
                key(Named::Space),
            ])
            .collect();
        let (messages, statuses) = scenario(true, events);
        assert_eq!(messages, ["c"]);
        assert!(
            statuses
                .iter()
                .all(|status| *status == event::Status::Captured)
        );
    }

    /// Клик снаружи проходит дальше и снимает фокус; Tab свободно идёт к следующему полю.
    #[test]
    fn outside_click_and_tab_close_without_selection() {
        let events = click(10.0, 16.0)
            .into_iter()
            .chain(click(10.0, 200.0))
            .chain([key(Named::Enter)])
            .chain(click(10.0, 16.0))
            .chain([key(Named::Tab), key(Named::Escape)])
            .collect();
        let (messages, statuses) = scenario(true, events);
        assert!(messages.is_empty());
        assert_eq!(&statuses[2..5], &[event::Status::Ignored; 3]);
        assert_eq!(&statuses[7..], &[event::Status::Ignored; 2]);
    }

    /// Недоступное поле не открывает список и не забирает события окна.
    #[test]
    fn disabled_select_ignores_pointer_and_keyboard() {
        let events = click(10.0, 16.0)
            .into_iter()
            .chain([key(Named::Enter)])
            .collect();
        let (messages, statuses) = scenario(false, events);
        assert!(messages.is_empty());
        assert_eq!(statuses, [event::Status::Ignored; 3]);
    }
}
