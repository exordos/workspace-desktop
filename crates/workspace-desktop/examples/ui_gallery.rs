//! Открывает демонстрационное окно для ручной проверки UI-кита.
//! Здесь хранятся настройки примеров и собираются вкладки с кнопками, полями, иконками и индикаторами.

use iced::keyboard::{self, Key, key::Named};
use iced::widget::{column, container, operation, row, scrollable, text};
use iced::{Element, Event, Fill, Subscription, Task, Theme, event};
use workspace_desktop::ui_kit::{
    BadgeSize, BadgeTone, ButtonAppearance, ButtonOptions, ButtonSize, ButtonVariant, FieldSize,
    IconName, IconTone, PresenceSize, PresenceStatus, SelectionSize, ThemeMode, button,
    button_with, form_field, icon, mention_badge, presence_indicator, search_input,
    selection_checkbox, selection_indicator, switch, text_input, text_input_with_error,
    unread_count_badge, workspace_theme,
};

const FIELD_EXAMPLES: [(FieldSize, &str); 2] = [
    (FieldSize::Compact, "chat-search-compact"),
    (FieldSize::Regular, "chat-search-regular"),
];

/// Хранит значения полей для одного размера.
struct FieldExample {
    title: String, // Название темы в примере с проверкой.
    query: String, // Поисковый запрос в примере с очисткой.
}

/// Определяет открытый раздел галереи.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum GalleryTab {
    #[default]
    Controls, // Варианты кнопок.
    Icons,      // Каталог иконок.
    Indicators, // Выбор, переключатели, статусы присутствия и бейджи.
    Fields,     // Поля формы и поиск.
}

/// Хранит настройки галереи и состояние примеров.
struct Gallery {
    dark: bool,                        // Признак включённой тёмной темы.
    fields: [FieldExample; 2],         // Значения примеров в порядке FIELD_EXAMPLES.
    last_button: Option<&'static str>, // Подпись последней нажатой кнопки. `None` до первого нажатия.
    button_presses: usize,             // Количество нажатий демонстрационных кнопок.
    tab: GalleryTab,                   // Открытый раздел галереи.
    icon_tone: IconTone,               // Выбранный способ окраски иконок.
    icon_size: u32,                    // Сторона иконок в логических пикселях.
    selected: bool,                    // Отмечен ли демонстрационный чекбокс.
    switch_on: bool,                   // Включён ли демонстрационный переключатель.
}

impl Default for Gallery {
    /// Задаёт начальные настройки вкладок и демонстрационных элементов.
    fn default() -> Self {
        Self {
            dark: false,
            fields: std::array::from_fn(|_| FieldExample {
                title: "Чат!".to_owned(),
                query: String::new(),
            }),
            last_button: None,
            button_presses: 0,
            tab: GalleryTab::default(),
            icon_tone: IconTone::default(),
            icon_size: 24,
            selected: false,
            switch_on: true,
        }
    }
}

/// Передаёт действия пользователя из виджетов в галерею.
#[derive(Debug, Clone)]
enum Message {
    ThemeChanged(bool),              // Новое состояние тёмной темы.
    InputChanged(usize, String),     // Индекс примера и новое название темы.
    ChatQueryChanged(usize, String), // Индекс примера и новый запрос.
    ChatQueryCleared(usize),         // Индекс примера, в котором очищают поиск.
    ButtonPressed(&'static str),     // Подпись нажатой кнопки.
    MoveFocus {
        backwards: bool, // Переход к предыдущему элементу по Shift+Tab.
    },
    TabChanged(GalleryTab),    // Вкладка, которую нужно открыть.
    IconToneChanged(IconTone), // Новый способ окраски иконок.
    IconSizeChanged(u32),      // Новый размер иконок в логических пикселях.
    SelectionChanged(bool),    // Чекбокс передал новую отметку.
    SwitchChanged(bool),       // Переключатель передал новое положение.
}

impl Gallery {
    /// Обновляет состояние примеров и при необходимости двигает фокус или прокрутку.
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ThemeChanged(dark) => self.dark = dark,
            Message::InputChanged(index, value) => self.fields[index].title = value,
            Message::ChatQueryChanged(index, query) => self.fields[index].query = query,
            Message::ChatQueryCleared(index) => {
                self.fields[index].query.clear();
                return operation::focus(FIELD_EXAMPLES[index].1);
            }
            Message::SelectionChanged(selected) => self.selected = selected,
            Message::SwitchChanged(on) => self.switch_on = on,
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
            ("Выбор и статусы", GalleryTab::Indicators),
            ("Поля", GalleryTab::Fields),
        ]
        .into_iter()
        .fold(row![].spacing(12), |row, (label, tab)| {
            row.push(option_button(
                label,
                self.tab == tab,
                Message::TabChanged(tab),
            ))
        });
        let section = match self.tab {
            GalleryTab::Controls => self.controls(),
            GalleryTab::Indicators => self.indicators(),
            GalleryTab::Fields => self.fields(),
            GalleryTab::Icons => self.icons(),
        };
        let content = scrollable(container(section).padding([16, 28]).center_x(Fill))
            .id("gallery-content")
            .height(Fill);
        column![
            container(
                column![
                    text("Workspace UI Kit").size(28),
                    switch("Тёмная тема", self.dark, Some(Message::ThemeChanged)),
                    tabs,
                ]
                .spacing(16)
                .max_width(760)
            )
            .padding([20, 28])
            .center_x(Fill),
            content,
        ]
        .into()
    }

    /// Показывает варианты кнопок для ручной проверки.
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

        buttons.width(Fill).max_width(760).into()
    }

    /// Показывает оба размера полей с проверкой ввода и очисткой поиска.
    fn fields(&self) -> Element<'_, Message> {
        row(FIELD_EXAMPLES
            .iter()
            .enumerate()
            .map(|(index, &(size, search_id))| self.field_example(index, size, search_id)))
        .spacing(24)
        .wrap()
        .into()
    }

    /// Собирает примеры полей одного размера с отдельными значениями и фокусом поиска.
    fn field_example(
        &self,
        index: usize,
        size: FieldSize,
        search_id: &'static str,
    ) -> Element<'_, Message> {
        let example = &self.fields[index];
        let invalid = example.title.contains('!');
        let field = form_field(
            "Название темы",
            text_input_with_error(
                "Введите название",
                &example.title,
                size,
                Some(move |value| Message::InputChanged(index, value)),
                invalid,
            ),
            Some("Например, «Обсуждение макетов»."),
            invalid.then_some("Уберите восклицательный знак из названия."),
        );
        let disabled = form_field(
            "Название канала",
            text_input("", "Команда", size, None::<fn(String) -> Message>),
            Some("Название может изменить администратор."),
            None,
        );
        let search = search_input(
            search_id,
            "Поиск по чатам",
            &example.query,
            size,
            Some(move |query| Message::ChatQueryChanged(index, query)),
            Message::ChatQueryCleared(index),
        );

        column![
            text(format!("{} px", size.height())).size(20),
            field,
            disabled,
            search
        ]
        .spacing(16)
        .width(320)
        .into()
    }

    /// Показывает чекбоксы, переключатели, статусы присутствия и счётчики.
    fn indicators(&self) -> Element<'_, Message> {
        use BadgeSize::{Regular, Small};
        use BadgeTone::{Muted, Unread};
        use PresenceSize::{Lg, Md, Sm, Xs};
        use PresenceStatus::{Away, Deactivated, Offline, Online};

        let selection = column![
            text("Индикатор выбора").size(20),
            row![
                text("16 px"),
                selection_indicator(SelectionSize::Small, false),
                selection_indicator(SelectionSize::Small, true),
                text("20 px"),
                selection_indicator(SelectionSize::Large, false),
                selection_indicator(SelectionSize::Large, true),
            ]
            .spacing(12)
            .align_y(iced::Center),
            selection_checkbox(
                "Выбрать участника",
                SelectionSize::Small,
                self.selected,
                Some(Message::SelectionChanged),
            ),
            selection_checkbox(
                "Недоступный чекбокс",
                SelectionSize::Large,
                true,
                None::<fn(bool) -> Message>,
            ),
        ]
        .spacing(12);

        let switches = column![
            text("Переключатель").size(20),
            switch("Уведомления", self.switch_on, Some(Message::SwitchChanged)),
            switch("Недоступен (выключен)", false, None::<fn(bool) -> Message>),
            switch("Недоступен (включён)", true, None::<fn(bool) -> Message>),
        ]
        .spacing(12);

        let presence = [
            ("В сети", Online, Lg),
            ("Отошёл", Away, Md),
            ("Не в сети", Offline, Sm),
            ("Деактивирован", Deactivated, Xs),
        ]
        .into_iter()
        .fold(row![].spacing(16), |row, (label, status, size)| {
            row.push(
                row![presence_indicator(status, size, false), text(label)]
                    .spacing(6)
                    .align_y(iced::Center),
            )
        });

        let badges = row![
            unread_count_badge(7, Unread, Small),
            unread_count_badge(0, Unread, Small),
            unread_count_badge(32, Muted, Regular),
            unread_count_badge(1_200, Unread, Regular),
            mention_badge(Small),
            mention_badge(Regular),
        ]
        .spacing(12)
        .align_y(iced::Center);

        column![
            selection,
            switches,
            column![text("Присутствие").size(20), presence.wrap()].spacing(12),
            column![text("Бейджи").size(20), badges].spacing(12),
        ]
        .spacing(22)
        .width(Fill)
        .max_width(760)
        .into()
    }

    /// Показывает каталог SVG с поиском и выбором размера, формы и цвета.
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
