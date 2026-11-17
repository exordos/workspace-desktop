//! Собирает редактор сообщения, места для дополнительных действий и кнопку отправки.

use std::{cell::Cell, rc::Rc};

use iced::advanced::widget::Id;
use iced::widget::button::{Status, Style};
use iced::widget::text_editor::{self, Action, Content};
use iced::widget::{button, container, row, text_editor as iced_text_editor};
use iced::{Border, Bottom, Element, Fill, Pixels, Theme};

use super::button::keyboard_button;
use super::theme::{button_colors, text_secondary};
use super::{IconName, IconTone, icon};

const COMPOSER_RADIUS: f32 = 12.0;
const EDITOR_LINE_HEIGHT: f32 = 20.0;
const EDITOR_VERTICAL_PADDING: u16 = 10;
const EDITOR_MIN_HEIGHT: f32 = 40.0;
const EDITOR_MAX_HEIGHT: f32 = 128.0;
const SEND_BUTTON_SIZE: u32 = 40;

/// Подсказка и дополнительные элементы, которые добавляет экран.
pub struct MessageComposerOptions<'a, Message> {
    pub placeholder: &'a str,                   // Подсказка в пустом редакторе.
    pub leading: Option<Element<'a, Message>>,  // Элементы перед текстом, например вложение.
    pub trailing: Option<Element<'a, Message>>, // Элементы после текста, например эмодзи.
    pub send_id: Option<Id>,                    // ID кнопки для операций Iced. `None` без ID.
}

impl<Message> Default for MessageComposerOptions<'_, Message> {
    /// Возвращает настройки редактора без дополнительных действий.
    fn default() -> Self {
        Self {
            placeholder: "Сообщение…",
            leading: None,
            trailing: None,
            send_id: None,
        }
    }
}

/// Рисует многострочный редактор сообщения с состоянием экрана.
/// Экран применяет полученные действия через `Content::perform` и обрабатывает отправку сам.
/// Если редактирование отключено или текст состоит из пробельных символов, отправка недоступна.
pub fn message_composer<'a, Message: Clone + 'a>(
    content: &'a Content,
    options: MessageComposerOptions<'a, Message>,
    on_edit: Option<impl Fn(Action) -> Message + 'a>,
    on_send: Option<Message>,
) -> Element<'a, Message> {
    // Проверяем строки по отдельности, чтобы не собирать копию всего черновика для кнопки.
    let on_send = on_send
        .filter(|_| on_edit.is_some() && content.lines().any(|line| !line.text.trim().is_empty()));
    let vertical_padding = f32::from(EDITOR_VERTICAL_PADDING) * 2.0;
    // TextEditor добавляет вертикальные отступы после расчёта высоты при Length::Shrink.
    let mut editor = iced_text_editor(content)
        .placeholder(options.placeholder)
        .size(16)
        .line_height(Pixels(EDITOR_LINE_HEIGHT))
        .min_height(EDITOR_MIN_HEIGHT - vertical_padding)
        .max_height(EDITOR_MAX_HEIGHT - vertical_padding)
        .padding([EDITOR_VERTICAL_PADDING, 8])
        .style(editor_style);
    if let Some(on_edit) = on_edit {
        editor = editor.on_action(on_edit);
    }

    let mut field = row![].align_y(Bottom).width(Fill);
    if let Some(leading) = options.leading {
        field = field.push(container(leading).padding(4));
    }
    field = field.push(editor);
    if let Some(trailing) = options.trailing {
        field = field.push(container(trailing).padding(4));
    }
    let inset = container(field)
        .width(Fill)
        .style(|theme: &Theme| container::Style {
            background: Some(theme.palette().background.into()),
            border: Border::default().rounded(COMPOSER_RADIUS),
            ..container::Style::default()
        });
    let mut send = container(send_button(on_send));
    if let Some(id) = options.send_id {
        send = send.id(id);
    }
    container(row![inset, send].spacing(12).align_y(Bottom))
        .width(Fill)
        .padding(8)
        .style(|theme: &Theme| container::Style {
            background: Some(button_colors(theme).elevated.into()),
            border: Border::default().rounded(COMPOSER_RADIUS),
            ..container::Style::default()
        })
        .into()
}

/// Выделяет границу редактора при фокусе и применяет цвета темы к подсказке и черновику.
fn editor_style(theme: &Theme, status: text_editor::Status) -> text_editor::Style {
    let palette = theme.palette();
    let colors = button_colors(theme);
    let border = Border::default().rounded(COMPOSER_RADIUS);
    let mut style = text_editor::default(theme, status);
    style.background = palette.background.into();
    style.placeholder = text_secondary(theme);
    style.value = if status == text_editor::Status::Disabled {
        colors.text_muted
    } else {
        palette.text
    };
    style.border = if matches!(status, text_editor::Status::Focused { .. }) {
        border.width(1).color(palette.primary)
    } else {
        border
    };
    style
}

/// Создаёт кнопку отправки с управлением мышью и клавиатурой.
fn send_button<'a, Message: Clone + 'a>(on_send: Option<Message>) -> Element<'a, Message> {
    let keyboard_pressed = Rc::new(Cell::new(false));
    let pressed_for_style = Rc::clone(&keyboard_pressed);
    let enabled = on_send.is_some();
    let glyph = container(icon(IconName::ComposerSend, 24, IconTone::Inherit, None))
        .center(SEND_BUTTON_SIZE);
    let inner = button(glyph)
        .padding(0)
        .on_press_maybe(on_send.clone())
        .style(move |theme, status| {
            let opacity = if !enabled {
                0.45
            } else if status == Status::Pressed || pressed_for_style.get() {
                0.8
            } else if status == Status::Hovered {
                0.9
            } else {
                1.0
            };
            Style {
                background: Some(theme.palette().primary.scale_alpha(opacity).into()),
                text_color: button_colors(theme).on_accent,
                border: Border::default().rounded(COMPOSER_RADIUS),
                ..Style::default()
            }
        });
    keyboard_button(inner, on_send, keyboard_pressed)
}

#[cfg(test)]
mod tests {
    use iced::advanced::widget::Tree;
    use iced::advanced::{Layout, Shell, clipboard, layout};
    use iced::widget::Space;
    use iced::{Event, Font, Point, Rectangle, Renderer, Size, mouse};

    use super::*;

    /// Сообщения тестового экрана для редактирования и отправки.
    #[derive(Clone, Debug, PartialEq)]
    enum Message {
        Edit(Action), // Команда от редактора.
        Send,         // Пользователь запросил отправку.
    }

    #[test]
    /// Нажатие отправляет текстовый черновик, а пустой, пробельный и недоступный оставляет на месте.
    fn clicking_send_requires_a_nonempty_enabled_draft() {
        let renderer = Renderer::new(Font::DEFAULT, Pixels(14.0));
        let available = Size::new(480.0, 200.0);
        let limits = layout::Limits::new(Size::ZERO, available);
        let viewport = Rectangle::with_size(available);

        for (value, editable, can_send, expected) in [
            ("", true, true, vec![]),
            (" \n\t", true, true, vec![]),
            ("Привет\nещё строка", false, true, vec![]),
            ("Привет\nещё строка", true, false, vec![]),
            ("Привет\nещё строка", true, true, vec![Message::Send]),
        ] {
            let content = Content::with_text(value);
            let mut composer = message_composer(
                &content,
                MessageComposerOptions::default(),
                editable.then_some(Message::Edit),
                can_send.then_some(Message::Send),
            );
            let mut tree = Tree::new(composer.as_widget());
            let node = composer
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let layout = Layout::new(&node);
            let send = layout.child(0).child(1).bounds();
            let cursor = mouse::Cursor::Available(Point::new(send.center_x(), send.center_y()));
            let mut messages = Vec::new();
            for event in [
                mouse::Event::ButtonPressed(mouse::Button::Left),
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ] {
                composer.as_widget_mut().update(
                    &mut tree,
                    &Event::Mouse(event),
                    layout,
                    cursor,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
            }
            assert_eq!(
                messages, expected,
                "черновик {value:?}, редактирование {editable}, отправка {can_send}"
            );
            assert_eq!(content.text(), value);
        }
    }

    #[test]
    /// В узком окне длинный черновик ограничен по высоте, а поле и кнопка работают рядом.
    fn narrow_composer_fits_actions_and_caps_a_long_draft() {
        let renderer = Renderer::new(Font::DEFAULT, Pixels(16.0));
        let viewport = Size::new(240.0, 360.0);
        let limits = layout::Limits::new(Size::ZERO, viewport);
        let long = "Длинная строка черновика, которая переносится в узком окне.\n".repeat(16);

        for (value, heights) in [
            ("", EDITOR_MIN_HEIGHT..=EDITOR_MIN_HEIGHT),
            (
                &long,
                EDITOR_MAX_HEIGHT - EDITOR_LINE_HEIGHT..=EDITOR_MAX_HEIGHT,
            ),
        ] {
            let content = Content::with_text(value);
            let slots = MessageComposerOptions {
                leading: Some(Space::new().width(32).height(32).into()),
                trailing: Some(Space::new().width(32).height(32).into()),
                ..MessageComposerOptions::default()
            };
            let mut composer =
                message_composer(&content, slots, Some(Message::Edit), Some(Message::Send));
            let mut tree = Tree::new(composer.as_widget());
            let node = composer
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let layout = Layout::new(&node);
            let row = layout.child(0);
            let editor = row.child(0).child(0).child(1).bounds();
            let send = row.child(1).bounds();

            assert!(heights.contains(&editor.height));
            assert_eq!(editor.width, 92.0);
            assert_eq!(send.width, SEND_BUTTON_SIZE as f32);
            assert_eq!(send.x + send.width, viewport.width - 8.0);
            assert_eq!(send.y + send.height, editor.y + editor.height);
        }
    }
}
