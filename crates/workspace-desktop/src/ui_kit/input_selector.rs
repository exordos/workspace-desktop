//! Показывает редактируемое поле с подсказками. Сначала здесь описаны параметры,
//! затем клавиатура, фокус и всплывающий список.

use std::rc::Rc;

use iced::advanced::widget::{Id, Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, input_method, layout, overlay, renderer};
use iced::keyboard::{self, Key, key::Named};
use iced::widget::container;
use iced::widget::text_input::{State as TextInputState, TextInput};
use iced::{
    Element, Event, Length, Padding, Point, Rectangle, Renderer, Size, Theme, Vector, mouse, touch,
    window,
};

use super::listbox::{ListboxOption, anchored_popup, listbox, listbox_next};
use super::{FieldSize, IconName, IconTone, icon, text_input_with_error};

const ARROW_SIZE: u16 = 24;

/// Данные поля и списка подсказок, которые передаёт экран.
pub struct InputSelectorOptions<'a, Message> {
    pub placeholder: &'a str,             // Подсказка в пустом поле.
    pub value: &'a str, // Текущее значение. Свободный текст допустим и без совпадения со списком.
    pub suggestions: &'a [ListboxOption], // Все возможные подсказки.
    pub empty: &'a str, // Текст списка, если поиск не нашёл подсказок.
    pub size: FieldSize, // Высота поля и отступы.
    pub searchable: bool, // Фильтровать подсказки по вводимому тексту.
    pub invalid: bool,  // Отметить поле как ошибочное по результатам проверки на экране.
    pub on_submit: Option<Message>, // Событие Enter, когда активная подсказка не выбрана.
}

/// Создаёт поле с подсказками. `None` вместо обработчика изменения отключает поле.
/// И ввод текста, и выбор подсказки передают новое значение одному обработчику.
pub fn input_selector<'a, Message: Clone + 'a>(
    id: impl Into<Id>,
    options: InputSelectorOptions<'a, Message>,
    on_change: Option<impl Fn(String) -> Message + 'a>,
) -> Element<'a, Message> {
    let InputSelectorOptions {
        placeholder,
        value,
        suggestions,
        empty,
        size,
        searchable,
        invalid,
        on_submit,
    } = options;
    let horizontal_padding = size.horizontal_padding();
    let vertical_padding = f32::from(size.vertical_padding());
    // TextInput хранит собственный обработчик. Обе копии вызывают одно замыкание.
    let on_change = on_change.map(|change| Rc::new(change) as Rc<Change<'a, Message>>);
    let has_suggestions = !suggestions.is_empty();
    let arrow_padding = if has_suggestions { ARROW_SIZE + 4 } else { 0 };
    let input = text_input_with_error(
        placeholder,
        value,
        size,
        on_change.clone().map(|change| move |value| change(value)),
        invalid,
    )
    .id(id)
    .padding(Padding {
        top: vertical_padding,
        right: f32::from(arrow_padding + horizontal_padding),
        bottom: vertical_padding,
        left: f32::from(horizontal_padding),
    });

    let arrows = [IconName::ChevronDown, IconName::ChevronUp].map(|name| {
        container(icon(name, 16, IconTone::Muted))
            .center(u32::from(ARROW_SIZE))
            .into()
    });
    Element::new(InputSelector {
        input,
        arrows,
        inset: f32::from(horizontal_padding),
        options: visible_options(suggestions, value, searchable),
        selected: value,
        empty,
        has_suggestions,
        on_change,
        on_submit,
    })
}

/// Возвращает варианты, которые подходят по названию, значению или дополнительной подписи.
fn visible_options(options: &[ListboxOption], value: &str, searchable: bool) -> Vec<ListboxOption> {
    let query = value.to_lowercase();
    options
        .iter()
        .filter(|option| {
            !searchable
                || query.is_empty()
                || [&option.label, &option.value]
                    .into_iter()
                    .chain(option.description.iter())
                    .any(|field| field.to_lowercase().contains(&query))
        })
        .cloned()
        .collect()
}

/// Получает новую строку и передаёт её экрану.
type Change<'a, Message> = dyn Fn(String) -> Message + 'a;

/// Хранит внешний вид поля и варианты, доступные в текущем кадре.
struct InputSelector<'a, Message> {
    input: TextInput<'a, Message>, // Штатное поле Iced со своим курсором и фокусом.
    arrows: [Element<'a, Message>; 2], // Стрелки для закрытого и открытого списка.
    inset: f32,                    // Отступ стрелки от правого края.
    options: Vec<ListboxOption>,   // Подсказки, которые проходят текущий фильтр.
    selected: &'a str,             // Текст поля для отметки совпавшей подсказки.
    empty: &'a str,                // Подпись в списке без совпадений.
    has_suggestions: bool,         // В исходном наборе есть хотя бы одна подсказка.
    on_change: Option<Rc<Change<'a, Message>>>, // Обработчик выбора и ввода.
    on_submit: Option<Message>,    // Обработчик Enter без активной подсказки.
}

/// Состояние, которое Iced сохраняет между кадрами.
struct State {
    open: bool,             // Список раскрыт.
    active: Option<String>, // Подсказка, выделенная стрелками.
    was_focused: bool,      // Фокус поля на предыдущем событии.
    composing: bool,        // Пользователь вводит символ через системный метод ввода.
    popup: Tree,            // Состояние прокрутки и нажатий в списке.
}

impl State {
    /// Закрывает список и сбрасывает подсветку клавиатуры.
    fn close(&mut self) {
        self.open = false;
        self.active = None;
    }
}

/// Возвращает состояние штатного TextInput без отдельного механизма фокуса.
fn text_state(
    tree: &mut Tree,
) -> &mut TextInputState<<Renderer as iced::advanced::text::Renderer>::Paragraph> {
    tree.state.downcast_mut()
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for InputSelector<'_, Message> {
    /// Сохраняет состояние при обновлении параметров того же виджета.
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    /// Создаёт закрытый список без активной подсказки.
    fn state(&self) -> tree::State {
        tree::State::new(State {
            open: false,
            active: None,
            was_focused: false,
            composing: false,
            popup: Tree::empty(),
        })
    }

    /// Отводит отдельное место для состояния поля и декоративной стрелки.
    fn children(&self) -> Vec<Tree> {
        vec![
            Tree::new(&self.input as &dyn Widget<Message, Theme, Renderer>),
            Tree::new(self.arrows[0].as_widget()),
        ]
    }

    /// Закрывает недоступный список и оставляет курсор в штатном поле при обычном обновлении.
    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if self.on_change.is_none() {
            state.was_focused = false;
            state.composing = false;
            text_state(&mut tree.children[0]).unfocus();
        }
        if self.on_change.is_none() || !self.has_suggestions {
            state.close();
        }
        self.input.diff(&mut tree.children[0]);
    }

    /// Возвращает размер вложенного поля.
    fn size(&self) -> Size<Length> {
        Widget::size(&self.input)
    }

    /// Размещает стрелку справа внутри границ поля.
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let field = <TextInput<'_, Message> as Widget<Message, Theme, Renderer>>::layout(
            &mut self.input,
            &mut tree.children[0],
            renderer,
            limits,
        );
        let size = field.size();
        let arrow = self.arrows[0].as_widget_mut().layout(
            &mut tree.children[1],
            renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        let arrow_size = arrow.size();
        let arrow = arrow.move_to(Point::new(
            (size.width - arrow_size.width - self.inset).max(0.0),
            (size.height - arrow_size.height) / 2.0,
        ));
        layout::Node::with_children(size, vec![field, arrow])
    }

    /// Регистрирует только поле, чтобы Tab не останавливался отдельно на стрелке.
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        if self.on_change.is_some() {
            self.input
                .operate(&mut tree.children[0], layout.child(0), renderer, operation);
        }
    }

    /// Обрабатывает навигацию по подсказкам и передаёт все команды редактирования в TextInput.
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let Some(on_change) = &self.on_change else {
            return;
        };
        let state = tree.state.downcast_mut::<State>();
        let input_tree = &mut tree.children[0];
        let was_open = state.open;
        let focused = text_state(input_tree).is_focused();
        if focused != state.was_focused {
            state.open = focused && self.has_suggestions;
            state.active = None;
            state.was_focused = focused;
            state.composing &= focused;
        }

        let press = match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => cursor.position(),
            Event::Touch(touch::Event::FingerPressed { position, .. }) => Some(*position),
            _ => None,
        };
        if self.has_suggestions
            && press.is_some_and(|point| layout.child(1).bounds().contains(point))
        {
            if !focused {
                text_state(input_tree).focus();
            }
            state.was_focused = true;
            state.open = !was_open;
            state.active = None;
            shell.capture_event();
            shell.request_redraw();
            return;
        }

        match event {
            Event::InputMethod(input_method::Event::Preedit(text, _)) => {
                state.composing = focused && !text.is_empty();
            }
            Event::InputMethod(input_method::Event::Closed | input_method::Event::Commit(_)) => {
                state.composing = false;
            }
            Event::Window(window::Event::Unfocused) => {
                state.composing = false;
                state.close();
            }
            _ => {}
        }
        if focused
            && !state.composing
            && let Event::Keyboard(keyboard::Event::KeyPressed {
                key: Key::Named(key),
                modifiers,
                ..
            }) = event
            && !modifiers.command()
            && !modifiers.control()
            && !modifiers.alt()
            && !modifiers.logo()
        {
            match key {
                Named::ArrowDown | Named::ArrowUp if self.has_suggestions => {
                    state.active = listbox_next(
                        &self.options,
                        state.active.as_deref(),
                        *key == Named::ArrowUp,
                    )
                    .map(str::to_owned);
                    state.open = true;
                    shell.capture_event();
                    shell.request_redraw();
                    return;
                }
                Named::Enter => {
                    if let Some(value) = state.active.take().filter(|active| {
                        state.open
                            && self
                                .options
                                .iter()
                                .any(|option| !option.disabled && option.value == *active)
                    }) {
                        state.close();
                        text_state(input_tree).move_cursor_to_end();
                        shell.publish(on_change(value));
                        shell.capture_event();
                        shell.request_redraw();
                        return;
                    }
                    state.close();
                    if let Some(message) = self.on_submit.clone() {
                        shell.publish(message);
                        shell.capture_event();
                    }
                    if was_open {
                        shell.request_redraw();
                    }
                    return;
                }
                Named::Escape if state.open => {
                    state.close();
                    shell.capture_event();
                    shell.request_redraw();
                    return;
                }
                Named::Tab => state.close(),
                _ => {}
            }
        }

        // Штатное поле публикует только изменение строки. Оно открывает актуальные подсказки.
        let mut messages = Vec::new();
        let mut local = Shell::new(&mut messages);
        self.input.update(
            input_tree,
            event,
            layout.child(0),
            cursor,
            renderer,
            clipboard,
            &mut local,
            viewport,
        );
        let focused_now = text_state(input_tree).is_focused();
        if !focused_now {
            state.close();
        } else if !focused || !local.is_empty() {
            state.open = self.has_suggestions;
            state.active = None;
        }
        state.was_focused = focused_now;
        if state.open != was_open {
            local.request_redraw();
        }
        shell.merge(local, std::convert::identity);
    }

    /// Рисует штатное поле и стрелку текущего состояния.
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
        Widget::draw(
            &self.input,
            &tree.children[0],
            renderer,
            theme,
            style,
            layout.child(0),
            cursor,
            viewport,
        );
        let state = tree.state.downcast_ref::<State>();
        if self.has_suggestions {
            self.arrows[usize::from(state.open)].as_widget().draw(
                &tree.children[1],
                renderer,
                theme,
                style,
                layout.child(1),
                cursor,
                viewport,
            );
        }
    }

    /// Показывает, где можно раскрыть список, а в тексте оставляет курсор ввода.
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.on_change.is_some()
            && self.has_suggestions
            && cursor.is_over(layout.child(1).bounds())
        {
            mouse::Interaction::Pointer
        } else {
            self.input.mouse_interaction(
                &tree.children[0],
                layout.child(0),
                cursor,
                viewport,
                renderer,
            )
        }
    }

    /// Открывает общий Listbox над содержимым окна, не передавая ему фокус поля.
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        _renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        let on_change = self.on_change.as_ref()?;
        let state = tree.state.downcast_mut::<State>();
        if !state.open || !layout.bounds().intersects(viewport) {
            return None;
        }
        let list = listbox(
            &self.options,
            Some(self.selected),
            state.active.as_deref(),
            self.empty,
            |value| on_change(value.to_owned()),
        );
        let State {
            open,
            active,
            popup,
            ..
        } = state;
        Some(anchored_popup(
            layout.bounds() + translation,
            list,
            popup,
            || {
                *open = false;
                *active = None;
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use iced::advanced::clipboard;
    use iced::advanced::renderer::Headless;
    use iced::advanced::widget::operation::focusable;
    use iced::{Font, Pixels};
    use iced_runtime::user_interface::{Cache, UserInterface};

    use super::*;

    /// Событие, которое тестовый экран получает из селектора.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Message {
        Updated(String), // Новое значение после ввода или выбора.
        Submitted,       // Пользователь отправил введённое значение.
    }

    /// Ставит фокус через операцию Iced, передаёт события и возвращает сообщения виджета.
    fn scenario(
        value: &str,
        events: impl IntoIterator<Item = (Event, mouse::Cursor)>,
    ) -> Vec<Message> {
        let id = Id::new("test-input-selector");
        let options = [("a", true), ("b", false)].map(|(value, disabled)| ListboxOption {
            value: value.to_owned(),
            label: value.to_owned(),
            description: None,
            disabled,
        });
        let view = input_selector(
            id.clone(),
            InputSelectorOptions {
                placeholder: "",
                value,
                suggestions: &options,
                empty: "Пусто",
                size: FieldSize::Compact,
                searchable: false,
                invalid: false,
                on_submit: Some(Message::Submitted),
            },
            Some(Message::Updated),
        );
        let mut renderer = block_on(<Renderer as Headless>::new(
            Font::DEFAULT,
            Pixels(14.0),
            Some("tiny-skia"),
        ))
        .unwrap();
        let mut ui =
            UserInterface::build(view, Size::new(220.0, 220.0), Cache::new(), &mut renderer);
        ui.operate(&renderer, &mut focusable::focus(id));
        let mut messages = Vec::new();
        for (event, cursor) in events {
            ui.update(
                &[event],
                cursor,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
        }
        messages
    }

    /// Создаёт нажатие системной клавиши без текста и модификаторов.
    fn key(name: Named) -> (Event, mouse::Cursor) {
        (
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: Key::Named(name),
                modified_key: Key::Named(name),
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

    /// Escape отменяет подсказку и сохраняет текст, Enter после повторного открытия выбирает вариант.
    #[test]
    fn escape_cancels_suggestion_and_enter_submits_or_chooses() {
        let messages = scenario(
            "свой вариант",
            [
                key(Named::ArrowDown),
                key(Named::Escape),
                key(Named::Enter),
                key(Named::ArrowDown),
                key(Named::Enter),
            ],
        );
        assert_eq!(
            messages,
            [Message::Submitted, Message::Updated("b".to_owned())]
        );
    }

    /// На открытом списке стрелка сохраняет курсор текста, и системный ввод меняет значение как обычно.
    #[test]
    fn chevron_keeps_caret_for_free_text() {
        let messages = scenario(
            "абв",
            [
                key(Named::Home),
                (
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    mouse::Cursor::Available(Point::new(199.0, 16.0)),
                ),
                (
                    Event::InputMethod(input_method::Event::Commit("Я".to_owned())),
                    mouse::Cursor::Unavailable,
                ),
            ],
        );
        assert_eq!(messages, [Message::Updated("Яабв".to_owned())]);
    }
}
