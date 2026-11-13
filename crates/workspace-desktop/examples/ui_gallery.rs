//! Открывает демонстрационное окно для ручной проверки UI-кита.
//! Здесь хранятся настройки примеров и собираются вкладки с элементами управления и иконками.

use iced::keyboard::{self, Key, key::Named};
use iced::widget::{column, container, operation, row, scrollable, text, toggler};
use iced::{Element, Event, Fill, Subscription, Task, Theme, event};
use workspace_desktop::ui_kit::{
    ButtonAppearance, ButtonOptions, ButtonSize, ButtonVariant, IconName, IconTone, ThemeMode,
    button, button_with, icon, text_input, workspace_theme,
};

/// Определяет открытый раздел галереи.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum GalleryTab {
    #[default]
    Controls, // Кнопки и текстовое поле.
    Icons, // Примеры иконок.
}

/// Хранит настройки галереи и состояние примеров.
struct Gallery {
    dark: bool,                        // Признак включённой тёмной темы.
    input: String,                     // Текст демонстрационного поля ввода.
    last_button: Option<&'static str>, // Подпись последней нажатой кнопки. `None` до первого нажатия.
    button_presses: usize,             // Количество нажатий демонстрационных кнопок.
    tab: GalleryTab,                   // Открытый раздел галереи.
    icon_tone: IconTone,               // Выбранный способ окраски иконок.
    icon_size: u32,                    // Сторона иконок в логических пикселях.
}

impl Default for Gallery {
    /// Задаёт начальные настройки вкладок и демонстрационных элементов.
    fn default() -> Self {
        Self {
            dark: false,
            input: String::new(),
            last_button: None,
            button_presses: 0,
            tab: GalleryTab::default(),
            icon_tone: IconTone::default(),
            icon_size: 24,
        }
    }
}

/// Передаёт действия пользователя из виджетов в галерею.
#[derive(Debug, Clone)]
enum Message {
    ThemeChanged(bool),          // Новое состояние тёмной темы.
    InputChanged(String),        // Новый текст демонстрационного поля.
    ButtonPressed(&'static str), // Подпись нажатой кнопки.
    MoveFocus {
        backwards: bool, // Переход к предыдущему элементу по Shift+Tab.
    },
    TabChanged(GalleryTab),    // Вкладка, которую нужно открыть.
    IconToneChanged(IconTone), // Новый способ окраски иконок.
    IconSizeChanged(u32),      // Новый размер иконок в логических пикселях.
}

impl Gallery {
    /// Обновляет состояние примеров и при необходимости двигает фокус или прокрутку.
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ThemeChanged(dark) => self.dark = dark,
            Message::InputChanged(value) => self.input = value,
            Message::TabChanged(tab) => {
                self.tab = tab;
                return operation::snap_to("gallery-content", scrollable::RelativeOffset::START);
            }
            Message::IconToneChanged(tone) => self.icon_tone = tone,
            Message::IconSizeChanged(size) => self.icon_size = size,
            Message::ButtonPressed(label) => {
                self.last_button = Some(label);
                self.button_presses += 1;
            }
            Message::MoveFocus { backwards: true } => return operation::focus_previous(),
            Message::MoveFocus { backwards: false } => return operation::focus_next(),
        }
        Task::none()
    }

    /// Преобразует Tab и Shift+Tab в запросы перехода фокуса.
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

    /// Выбирает светлую или тёмную тему для окна галереи.
    fn theme(&self) -> Theme {
        workspace_theme(if self.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        })
    }

    /// Собирает шапку, вкладки и прокручиваемую область выбранного раздела.
    fn view(&self) -> Element<'_, Message> {
        let tabs = [
            ("Компоненты", GalleryTab::Controls),
            ("Иконки", GalleryTab::Icons),
        ]
        .into_iter()
        .fold(row![].spacing(12), |row, (label, tab)| {
            row.push(option_button(
                label,
                self.tab == tab,
                Message::TabChanged(tab),
            ))
        });
        let content = match self.tab {
            GalleryTab::Controls => self.controls(),
            GalleryTab::Icons => self.icons(),
        };
        column![
            container(
                column![
                    text("Workspace UI Kit").size(28),
                    toggler(self.dark)
                        .label("Тёмная тема")
                        .on_toggle(Message::ThemeChanged),
                    tabs,
                ]
                .spacing(16)
                .max_width(760)
            )
            .padding([20, 28])
            .center_x(Fill),
            scrollable(container(content).padding([16, 28]).center_x(Fill))
                .id("gallery-content")
                .height(Fill),
        ]
        .into()
    }

    /// Показывает варианты кнопок и текстового поля для ручной проверки.
    fn controls(&self) -> Element<'_, Message> {
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
                add_button(),
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

        column![buttons, inputs]
            .spacing(28)
            .width(Fill)
            .max_width(760)
            .into()
    }

    /// Показывает примеры иконок с выбором размера и цвета.
    fn icons(&self) -> Element<'_, Message> {
        let tones = [
            ("Основной", IconTone::Inherit),
            ("Приглушённый", IconTone::Muted),
            ("Акцентный", IconTone::Accent),
            ("Опасное действие", IconTone::Danger),
        ]
        .into_iter()
        .fold(row![].spacing(8), |row, (label, tone)| {
            row.push(option_button(
                label,
                self.icon_tone == tone,
                Message::IconToneChanged(tone),
            ))
        });
        let sizes = [("16", 16), ("20", 20), ("24", 24), ("32", 32), ("48", 48)]
            .into_iter()
            .fold(row![].spacing(8), |row, (label, size)| {
                row.push(option_button(
                    label,
                    self.icon_size == size,
                    Message::IconSizeChanged(size),
                ))
            });
        let previews = [
            ("Plus", IconName::Plus),
            ("Search", IconName::Search),
            ("Close", IconName::Close),
            ("Attach", IconName::Attach),
        ]
        .into_iter()
        .fold(row![].spacing(12), |row, (label, name)| {
            row.push(
                container(
                    column![
                        container(icon(name, self.icon_size, self.icon_tone))
                            .center_x(Fill)
                            .center_y(64),
                        text(label).size(13),
                    ]
                    .align_x(iced::Center),
                )
                .width(120)
                .padding([8, 12])
                .style(container::rounded_box),
            )
        });

        column![
            column![text("Цвет"), tones.wrap()].spacing(8),
            column![text("Размер, px"), sizes.wrap()].spacing(8),
            previews.wrap(),
            column![
                text("В кнопке · 16 px · цвет подписи").size(14),
                add_button()
            ]
            .spacing(8),
            text(format!("Нажатий на кнопки: {}", self.button_presses)).size(14),
        ]
        .width(Fill)
        .max_width(760)
        .spacing(20)
        .into()
    }
}

/// Создаёт кнопку выбора настройки и выделяет текущий вариант.
fn option_button(
    label: &'static str,
    selected: bool,
    message: Message,
) -> Element<'static, Message> {
    let (variant, appearance) = if selected {
        (ButtonVariant::Primary, ButtonAppearance::Filled)
    } else {
        (ButtonVariant::Neutral, ButtonAppearance::Outline)
    };
    button_with(
        label,
        ButtonOptions {
            variant,
            appearance,
            size: ButtonSize::Small,
            ..ButtonOptions::default()
        },
        Some(message),
    )
}

/// Показывает пример кнопки добавления с иконкой.
fn add_button() -> Element<'static, Message> {
    button_with(
        "Добавить",
        ButtonOptions {
            leading_icon: Some(icon(IconName::Plus, 16, IconTone::Inherit)),
            ..ButtonOptions::default()
        },
        Some(Message::ButtonPressed("Добавить")),
    )
}

/// Запускает окно Iced с темой, обработкой действий и подпиской на клавиатуру.
fn main() -> iced::Result {
    iced::application(Gallery::default, Gallery::update, Gallery::view)
        .title("Workspace UI Kit")
        .theme(Gallery::theme)
        .subscription(Gallery::subscription)
        .window_size((760.0, 700.0))
        .run()
}
