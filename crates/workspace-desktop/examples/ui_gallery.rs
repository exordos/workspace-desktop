use iced::keyboard::{self, Key, key::Named};
use iced::widget::{canvas, column, container, operation, row, scrollable, text, toggler};
use iced::{
    Color, Element, Event, Fill, Rectangle, Renderer, Subscription, Task, Theme, Vector, event,
    mouse,
};
use workspace_desktop::ui_kit::{
    ButtonAppearance, ButtonOptions, ButtonSize, ButtonVariant, ThemeMode, button, button_with,
    text_input, workspace_theme,
};

#[derive(Default)]
struct Gallery {
    dark: bool,
    input: String,
    last_button: Option<&'static str>,
    button_presses: usize,
}

#[derive(Debug, Clone)]
enum Message {
    ThemeChanged(bool),
    InputChanged(String),
    ButtonPressed(&'static str),
    MoveFocus { backwards: bool },
}

impl Gallery {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ThemeChanged(dark) => self.dark = dark,
            Message::InputChanged(value) => self.input = value,
            Message::ButtonPressed(label) => {
                self.last_button = Some(label);
                self.button_presses += 1;
            }
            Message::MoveFocus { backwards: true } => return operation::focus_previous(),
            Message::MoveFocus { backwards: false } => return operation::focus_next(),
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        event::listen().filter_map(|event| match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: Key::Named(Named::Tab),
                modifiers,
                ..
            }) if !modifiers.alt() && !modifiers.control() && !modifiers.logo() => {
                Some(Message::MoveFocus {
                    backwards: modifiers.shift(),
                })
            }
            _ => None,
        })
    }

    fn theme(&self) -> Theme {
        workspace_theme(if self.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        })
    }

    fn view(&self) -> Element<'_, Message> {
        use ButtonAppearance::{Filled, Ghost, Outline};
        use ButtonSize::{Large, Medium, Small};
        use ButtonVariant::{Danger, Neutral, Primary};

        let appearance_row = |name, appearance| {
            column![
                text(name).size(14),
                [
                    ("Сохранить", Primary),
                    ("Отменить", Neutral),
                    ("Удалить", Danger),
                ]
                .into_iter()
                .fold(row![].spacing(12), |row, (label, variant)| {
                    row.push(button_with(
                        label,
                        ButtonOptions {
                            variant,
                            appearance,
                            ..ButtonOptions::default()
                        },
                        Some(Message::ButtonPressed(label)),
                    ))
                })
                .wrap()
            ]
            .spacing(6)
        };
        let sizes = [("32 px", Small), ("36 px", Medium), ("40 px", Large)]
            .into_iter()
            .fold(
                row![].spacing(12).align_y(iced::Center),
                |row, (label, size)| {
                    row.push(button_with(
                        label,
                        ButtonOptions {
                            size,
                            ..ButtonOptions::default()
                        },
                        Some(Message::ButtonPressed(label)),
                    ))
                },
            );

        let buttons = column![
            text("Кнопки").size(20),
            appearance_row("С заливкой", Filled),
            appearance_row("С контуром", Outline),
            appearance_row("Без фона", Ghost),
            sizes.wrap(),
            row![
                button("Недоступна", Primary, None),
                button_with(
                    "Загрузка",
                    ButtonOptions {
                        loading: true,
                        ..ButtonOptions::default()
                    },
                    Some(Message::ButtonPressed("Загрузка"))
                ),
                button_with(
                    "Добавить",
                    ButtonOptions {
                        leading_icon: Some(canvas(PlusIcon).width(12).height(12).into()),
                        ..ButtonOptions::default()
                    },
                    Some(Message::ButtonPressed("Добавить"))
                ),
            ]
            .spacing(12)
            .wrap(),
            button_with(
                "На всю ширину",
                ButtonOptions {
                    full_width: true,
                    appearance: Outline,
                    variant: Neutral,
                    ..ButtonOptions::default()
                },
                Some(Message::ButtonPressed("На всю ширину"))
            ),
            text(match self.last_button {
                Some(label) => format!(
                    "Нажата кнопка «{label}». Всего нажатий: {}",
                    self.button_presses
                ),
                None => "Кнопка ещё не нажата".to_owned(),
            })
            .size(14),
        ]
        .spacing(12);

        let inputs = column![
            text("Поля ввода").size(20),
            text_input("Введите текст", &self.input, Some(Message::InputChanged)),
            text_input("", "Недоступное поле", None::<fn(String) -> Message>),
        ]
        .spacing(12)
        .max_width(360);

        let content = column![
            text("Workspace UI Kit").size(28),
            toggler(self.dark)
                .label("Тёмная тема")
                .on_toggle(Message::ThemeChanged),
            buttons,
            inputs,
        ]
        .spacing(28)
        .max_width(760);

        scrollable(container(content).padding(28).center_x(Fill)).into()
    }
}

struct PlusIcon;

impl canvas::Program<Message> for PlusIcon {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let center = frame.center();
        let path = canvas::Path::new(|path| {
            path.move_to(center - Vector::new(4.0, 0.0));
            path.line_to(center + Vector::new(4.0, 0.0));
            path.move_to(center - Vector::new(0.0, 4.0));
            path.line_to(center + Vector::new(0.0, 4.0));
        });
        frame.stroke(
            &path,
            canvas::Stroke::default()
                .with_width(1.5)
                .with_color(Color::from_rgb8(0x10, 0x1a, 0x28)),
        );
        vec![frame.into_geometry()]
    }
}

fn main() -> iced::Result {
    iced::application(Gallery::default, Gallery::update, Gallery::view)
        .title("Workspace UI Kit")
        .theme(Gallery::theme)
        .subscription(Gallery::subscription)
        .window_size((760.0, 700.0))
        .run()
}
