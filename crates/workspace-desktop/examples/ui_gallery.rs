//! Открывает демонстрационное окно для ручной проверки UI-кита.
//! Здесь хранятся настройки примеров и собираются вкладки с компонентами.

use iced::keyboard::{self, Key, key::Named};
use iced::widget::text_editor::{self, Content};
use iced::widget::{Column, column, container, operation, row, scrollable, text};
use iced::{Element, Event, Fill, Subscription, Task, Theme, event};
use workspace_desktop::ui_kit::{
    AvatarContent, AvatarDecoration, AvatarPhoto, AvatarSize, BadgeSize, BadgeTone,
    ButtonAppearance, ButtonOptions, ButtonSize, ButtonVariant, ChatRowOptions, ChatRowPreview,
    FieldSize, IconName, IconTone, IconVariant, InputSelectorOptions, ListboxOption,
    MessageComposerOptions, MessageDelivery, MessageDirection, PanelMenuRowOptions, PresenceSize,
    PresenceStatus, SelectionSize, ThemeMode, avatar, avatar_group, button, button_with, chat_row,
    day_divider, form_field, icon, input_selector, listbox, member_row, member_row_content,
    mention_badge, message_bubble, message_composer, panel_menu_row, presence_indicator,
    search_input, select, selectable_row, selection_checkbox, selection_indicator, switch,
    text_input, text_input_with_error, unread_count_badge, workspace_theme,
};

const FIELD_EXAMPLES: [(FieldSize, &str); 2] = [
    (FieldSize::Compact, "chat-search-compact"),
    (FieldSize::Regular, "chat-search-regular"),
];

const SELECTOR_EXAMPLES: [(FieldSize, &str); 2] = [
    (FieldSize::Compact, "timezone-input-compact"),
    (FieldSize::Regular, "timezone-input-regular"),
];

const LONG_MESSAGE_DRAFT: &str = concat!(
    "Посмотри, пожалуйста, варианты интерфейса.\n",
    "Проверь светлую и тёмную темы.\n",
    "Открой узкое окно и попробуй отправить сообщение.\n",
    "Убедись, что длинный текст не перекрывает кнопки.\n",
    "Сравни отступы в коротком и многострочном сообщении.\n",
    "Когда закончишь, напиши, что стоит поправить.\n",
    "Спасибо!",
);

/// Хранит значения полей для одного размера.
struct FieldExample {
    title: String, // Название темы в примере с проверкой.
    query: String, // Поисковый запрос в примере с очисткой.
}

/// Хранит независимые значения селекторов одного размера.
struct SelectorExample {
    selected: Option<String>, // Значение из готового списка.
    value: String,            // Введённый текст или значение подсказки.
}

/// Определяет открытый раздел галереи.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum GalleryTab {
    #[default]
    Controls, // Варианты кнопок.
    Icons,      // Каталог иконок.
    Indicators, // Выбор, переключатели, статусы присутствия и бейджи.
    Fields,     // Поля формы и поиск.
    Rows,       // Строки чатов, аватары, карточки и строки действий.
    Selectors,  // Выбор из списка и ввод с подсказками.
    Messages,   // Сообщения и редактор.
}

/// Хранит настройки галереи и состояние примеров.
struct Gallery {
    dark: bool,                              // Признак включённой тёмной темы.
    fields: [FieldExample; 2],               // Значения примеров в порядке FIELD_EXAMPLES.
    last_button: Option<&'static str>, // Подпись последней нажатой кнопки. `None` до первого нажатия.
    button_presses: usize,             // Количество нажатий демонстрационных кнопок.
    tab: GalleryTab,                   // Открытый раздел галереи.
    icon_tone: IconTone,               // Выбранный способ окраски иконок.
    icon_size: u32,                    // Сторона иконок в логических пикселях.
    icon_variant: IconVariant,         // Выбранная форма для иконок, у которых есть пара SVG.
    icon_query: String,                // Строка фильтра по именам иконок.
    selected: bool,                    // Отмечен ли демонстрационный чекбокс.
    switch_on: bool,                   // Включён ли демонстрационный переключатель.
    avatar_photo: AvatarPhoto,         // Искусственное изображение для проверки обрезки аватара.
    member_selected: bool,             // Отмечен ли участник в примере выбора.
    selected_chat: usize,              // Индекс выбранного чата в примере.
    last_panel_action: Option<&'static str>, // Подпись последнего действия панели.
    selectors: [SelectorExample; 2],   // Значения примеров для обоих размеров.
    selector_options: Vec<ListboxOption>, // Подсказки для выбора часового пояса.
    listbox_value: String,             // Выбранный вариант в отдельном примере списка.
    composer: Content,                 // Черновик в примере редактора сообщения.
    sent_preview: Option<String>,      // Последнее сообщение, отправленное из примера.
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
            icon_variant: IconVariant::default(),
            icon_query: String::new(),
            selected: false,
            switch_on: true,
            avatar_photo: example_avatar_photo(),
            member_selected: false,
            selected_chat: 0,
            last_panel_action: None,
            selectors: std::array::from_fn(|_| SelectorExample {
                selected: Some("Europe/Moscow".to_owned()),
                value: String::new(),
            }),
            selector_options: [
                ("Europe/Moscow", "Москва", false),
                ("Asia/Yekaterinburg", "Екатеринбург", true),
                ("Europe/Berlin", "Берлин", false),
                ("Asia/Tokyo", "Токио", false),
            ]
            .into_iter()
            .map(|(value, label, disabled)| ListboxOption {
                value: value.to_owned(),
                label: label.to_owned(),
                description: Some(value.to_owned()),
                disabled,
            })
            .collect(),
            listbox_value: "Europe/Moscow".to_owned(),
            composer: Content::new(),
            sent_preview: None,
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
    TabChanged(GalleryTab),                // Вкладка, которую нужно открыть.
    IconToneChanged(IconTone),             // Новый способ окраски иконок.
    IconSizeChanged(u32),                  // Новый размер иконок в логических пикселях.
    IconVariantChanged(IconVariant),       // Новая форма иконок.
    IconQueryChanged(String),              // Новый фильтр списка иконок.
    SelectionChanged(bool),                // Чекбокс передал новую отметку.
    SwitchChanged(bool),                   // Переключатель передал новое положение.
    MemberSelectionChanged(bool),          // Новая отметка в строке выбора участника.
    ChatSelected(usize),                   // Индекс выбранного чата в примере.
    PanelActionPressed(&'static str),      // Подпись нажатой строки действия.
    SelectorChanged(usize, String),        // Индекс и значение выбранной опции.
    SuggestionInputChanged(usize, String), // Индекс и свободный текст поля.
    ListboxValueChanged(String),           // Значение из отдельного примера списка.
    ComposerEdited(text_editor::Action),   // Действие редактирования в поле сообщения.
    ComposerSent,                          // Отправка демонстрационного черновика.
    ComposerDraftLoaded,                   // Загрузка длинного текста для проверки редактора.
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
            Message::MemberSelectionChanged(selected) => self.member_selected = selected,
            Message::ChatSelected(index) => self.selected_chat = index,
            Message::PanelActionPressed(action) => self.last_panel_action = Some(action),
            Message::SelectorChanged(index, value) => self.selectors[index].selected = Some(value),
            Message::SuggestionInputChanged(index, value) => self.selectors[index].value = value,
            Message::ListboxValueChanged(value) => self.listbox_value = value,
            Message::ComposerEdited(action) => self.composer.perform(action),
            Message::ComposerDraftLoaded => self.composer = Content::with_text(LONG_MESSAGE_DRAFT),
            Message::ComposerSent => {
                self.sent_preview = Some(self.composer.text());
                self.composer = Content::new();
            }
            Message::TabChanged(tab) => {
                self.tab = tab;
                return operation::snap_to("gallery-content", scrollable::RelativeOffset::START);
            }
            Message::IconToneChanged(tone) => self.icon_tone = tone,
            Message::IconSizeChanged(size) => self.icon_size = size,
            Message::IconVariantChanged(variant) => self.icon_variant = variant,
            Message::IconQueryChanged(query) => {
                self.icon_query = query;
                return operation::snap_to("gallery-content", scrollable::RelativeOffset::START);
            }
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
            ("Аватары и строки", GalleryTab::Rows),
            ("Селекторы", GalleryTab::Selectors),
            ("Сообщения", GalleryTab::Messages),
        ]
        .into_iter()
        .fold(row![].spacing(12), |row, (label, tab)| {
            row.push(option_button(
                label,
                self.tab == tab,
                Message::TabChanged(tab),
            ))
        })
        .wrap();
        let section = match self.tab {
            GalleryTab::Controls => self.controls(),
            GalleryTab::Indicators => self.indicators(),
            GalleryTab::Fields => self.fields(),
            GalleryTab::Rows => self.rows(),
            GalleryTab::Selectors => self.selectors(),
            GalleryTab::Messages => self.messages(),
            GalleryTab::Icons => self.icons(),
        };
        let content = if self.tab == GalleryTab::Icons {
            section
        } else {
            scrollable(container(section).padding([16, 28]).center_x(Fill))
                .id("gallery-content")
                .height(Fill)
                .into()
        };
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

    /// Показывает строки чатов, аватары разных размеров, выбираемые строки и действия панели.
    fn rows(&self) -> Element<'_, Message> {
        use AvatarContent::{Initials, Photo, Placeholder};
        use AvatarDecoration::{Presence, Selection};
        use AvatarSize::{Large, Medium, Profile, Small};

        let avatars = row![
            avatar(Small, Initials("МЛ"), Some(Selection(false))),
            avatar(Small, Initials("АК"), Some(Selection(true))),
            avatar(
                Medium,
                Initials("ВН"),
                Some(Presence(PresenceStatus::Online))
            ),
            avatar(Large, Placeholder, Some(Presence(PresenceStatus::Away))),
            avatar(Profile, Photo(self.avatar_photo.clone()), None),
            avatar_group(
                &[
                    Initials("МЛ"),
                    Photo(self.avatar_photo.clone()),
                    Placeholder
                ],
                4
            ),
        ]
        .spacing(16)
        .align_y(iced::Center)
        .wrap();

        let members = column![
            member_row(
                avatar(
                    Medium,
                    Initials("ВН"),
                    Some(Presence(PresenceStatus::Online))
                ),
                "Вера Новикова",
                Some("Дизайн интерфейсов"),
                Some(text("Владелец").size(12).into()),
            ),
            selectable_row(
                member_row_content(
                    avatar(Small, Initials("АК"), Some(Selection(self.member_selected))),
                    "Александра Константинова-Воронцова",
                    Some("Команда исследования и разработки"),
                    None,
                ),
                self.member_selected,
                Some(Message::MemberSelectionChanged(!self.member_selected)),
            ),
            selectable_row(
                member_row_content(
                    avatar(Small, Placeholder, Some(Selection(true))),
                    "Участник уже добавлен",
                    None,
                    None,
                ),
                true,
                None,
            ),
        ]
        .spacing(6)
        .width(Fill)
        .max_width(340);

        let panels = column![
            panel_menu_row(
                "Звуки уведомлений",
                PanelMenuRowOptions {
                    icon: Some(IconName::VolumeUp),
                    trailing: Some(
                        row![
                            text("По умолчанию").size(12),
                            icon(IconName::ChevronRight, 16, IconTone::Muted, None)
                        ]
                        .spacing(6)
                        .align_y(iced::Center)
                        .into()
                    ),
                    ..PanelMenuRowOptions::default()
                },
                Some(Message::PanelActionPressed("Звуки уведомлений")),
            ),
            panel_menu_row(
                "Настройка оформления списка чатов",
                PanelMenuRowOptions {
                    icon: Some(IconName::Settings),
                    subtitle: Some("Тема, расположение папок и плотность списка чатов."),
                    trailing: Some(icon(IconName::ChevronRight, 16, IconTone::Muted, None)),
                    ..PanelMenuRowOptions::default()
                },
                Some(Message::PanelActionPressed("Настройка оформления")),
            ),
            panel_menu_row(
                "Язык",
                PanelMenuRowOptions {
                    icon: Some(IconName::Language),
                    compact: true,
                    trailing: Some(text("Русский").size(12).into()),
                    ..PanelMenuRowOptions::default()
                },
                None,
            ),
            panel_menu_row(
                "Выйти",
                PanelMenuRowOptions {
                    icon: Some(IconName::LogoutCompact),
                    danger: true,
                    ..PanelMenuRowOptions::default()
                },
                Some(Message::PanelActionPressed("Выйти")),
            ),
            text(match self.last_panel_action {
                Some(action) => format!("Последнее действие: {action}"),
                None => "Действие ещё не выбрано".to_owned(),
            })
            .size(12),
        ]
        .spacing(6)
        .width(Fill)
        .max_width(340);

        column![
            self.chat_rows(),
            text("Аватары").size(20),
            avatars,
            row![
                column![text("Участники").size(20), members]
                    .spacing(12)
                    .max_width(340),
                column![text("Действия панели").size(20), panels]
                    .spacing(12)
                    .max_width(340),
            ]
            .spacing(24)
            .wrap(),
        ]
        .spacing(18)
        .width(Fill)
        .max_width(760)
        .into()
    }

    /// Показывает две выбираемые строки с метаданными и строку с данными каталога.
    fn chat_rows(&self) -> Element<'_, Message> {
        column![
            text("Список чатов").size(20),
            chat_row(ChatRowOptions {
                avatar: avatar(
                    AvatarSize::Medium,
                    AvatarContent::Photo(self.avatar_photo.clone()),
                    Some(AvatarDecoration::Presence(PresenceStatus::Online)),
                ),
                title: "Вера Новикова",
                preview: Some(ChatRowPreview {
                    sender: None,
                    text: "Созвонимся через пять минут?",
                }),
                time: Some("14:25"),
                unread: Some((3, BadgeTone::Unread)),
                mention: false,
                selected: self.selected_chat == 0,
                on_press: Some(Message::ChatSelected(0)),
            }),
            chat_row(ChatRowOptions {
                avatar: avatar(AvatarSize::Medium, AvatarContent::Initials("Д"), None),
                title: "Дизайн 🎨 и разработка интерфейсов рабочего пространства",
                preview: Some(ChatRowPreview {
                    sender: Some("Аня"),
                    text: "Проверила 👩🏻‍💻 новый список чатов и оставила комментарии",
                }),
                time: Some("12:08"),
                unread: Some((42, BadgeTone::Muted)),
                mention: true,
                selected: self.selected_chat == 1,
                on_press: Some(Message::ChatSelected(1)),
            }),
            chat_row(ChatRowOptions {
                avatar: avatar(AvatarSize::Medium, AvatarContent::Initials("П"), None),
                title: "Поддержка",
                preview: None,
                time: None,
                unread: None,
                mention: false,
                selected: false,
                on_press: None,
            }),
        ]
        .spacing(6)
        .width(Fill)
        // У стандартной панели в workspace_ui ширина 299 px.
        .max_width(299)
        .into()
    }

    /// Показывает оба размера селекторов и отдельно раскрытый список опций.
    fn selectors(&self) -> Element<'_, Message> {
        let fields = row(SELECTOR_EXAMPLES
            .iter()
            .enumerate()
            .map(|(index, &(size, input_id))| self.selector_example(index, size, input_id)))
        .spacing(24)
        .wrap();
        let cases = column![
            text("Состояния").size(20),
            form_field(
                "Пустой список",
                select(
                    &[],
                    None,
                    "Нет вариантов",
                    "Список пуст",
                    FieldSize::Regular,
                    Some(|value: &str| Message::ListboxValueChanged(value.to_owned()))
                ),
                None,
                None,
            ),
            form_field(
                "Недоступное поле",
                input_selector(
                    "timezone-input-disabled",
                    InputSelectorOptions {
                        placeholder: "",
                        value: "Europe/Moscow",
                        suggestions: &self.selector_options,
                        empty: "Нет совпадений",
                        size: FieldSize::Compact,
                        searchable: true,
                        invalid: false,
                        on_submit: None,
                    },
                    None::<fn(String) -> Message>,
                ),
                None,
                None,
            ),
        ]
        .width(Fill)
        .max_width(340)
        .spacing(12);
        let list = column![
            text("Список вариантов").size(20),
            listbox(
                &self.selector_options,
                Some(&self.listbox_value),
                Some("Europe/Berlin"),
                "Список пуст",
                |value| Message::ListboxValueChanged(value.to_owned()),
            ),
        ]
        .width(Fill)
        .max_width(340)
        .spacing(12);
        column![fields, row![list, cases].spacing(24).wrap()]
            .width(Fill)
            .max_width(760)
            .spacing(22)
            .into()
    }

    /// Собирает закрытые поля одного размера с разными сценариями ввода.
    fn selector_example(
        &self,
        index: usize,
        size: FieldSize,
        input_id: &'static str,
    ) -> Element<'_, Message> {
        let example = &self.selectors[index];
        let title = match size {
            FieldSize::Compact => "32 px",
            FieldSize::Regular => "40 px",
        };
        column![
            text(title).size(20),
            form_field(
                "Часовой пояс",
                select(
                    &self.selector_options,
                    example.selected.as_deref(),
                    "Выберите город",
                    "Список пуст",
                    size,
                    Some(move |value: &str| Message::SelectorChanged(index, value.to_owned())),
                ),
                None,
                None,
            ),
            form_field(
                "Ввод с подсказками",
                input_selector(
                    input_id,
                    InputSelectorOptions {
                        placeholder: "Введите город или часовой пояс",
                        value: &example.value,
                        suggestions: &self.selector_options,
                        empty: "Нет совпадений",
                        size,
                        searchable: true,
                        invalid: false,
                        on_submit: None,
                    },
                    Some(move |value| Message::SuggestionInputChanged(index, value)),
                ),
                None,
                None,
            ),
        ]
        .width(Fill)
        .max_width(340)
        .spacing(12)
        .into()
    }

    /// Показывает короткое и длинное сообщение, статусы доставки и отправку черновика.
    fn messages(&self) -> Element<'_, Message> {
        use MessageDelivery::{Delivered, Failed, Read, Sending, Sent};
        use MessageDirection::{Incoming, Outgoing};

        let mut bubbles = column![message_bubble(
            "Посмотри макет, когда будет время.",
            Some("Саша"),
            "12:05",
            Incoming,
        )]
        .spacing(10)
        .width(Fill);

        for (body, time, delivery, max_width) in [
            (
                "Сначала проверю, как список читается в узком окне, затем сравню светлую и тёмную темы.\nНачну сегодня.",
                "12:07",
                Read,
                500,
            ),
            (
                "Если окно узкое, у времени может не хватить места.\nПрикреплю небольшой пример.",
                "12:08",
                Delivered,
                245,
            ),
            ("Добавил комментарии.", "12:09", Sent, 500),
            ("Пришлю ещё один пример.", "12:10", Sending, 500),
            (
                "На этой строке произошла ошибка отправки.",
                "12:11",
                Failed,
                500,
            ),
        ] {
            let bubble = container(message_bubble(body, None, time, Outgoing(Some(delivery))))
                .max_width(max_width);
            bubbles = bubbles.push(container(bubble).align_right(Fill));
        }

        if let Some(body) = self.sent_preview.as_deref() {
            bubbles = bubbles.push(
                container(message_bubble(body, None, "Сейчас", Outgoing(Some(Sent))))
                    .align_right(Fill),
            );
        }
        let composer = message_composer(
            &self.composer,
            MessageComposerOptions {
                leading: Some(composer_slot(IconName::Attach)),
                trailing: Some(composer_slot(IconName::ComposerEmoji)),
                ..MessageComposerOptions::default()
            },
            Some(Message::ComposerEdited),
            Some(Message::ComposerSent),
        );
        column![
            row![
                text("Сообщения").size(20),
                button(
                    "Длинный черновик",
                    ButtonVariant::Neutral,
                    Some(Message::ComposerDraftLoaded),
                ),
            ]
            .spacing(12)
            .align_y(iced::Center)
            .wrap(),
            composer,
            day_divider("16.11.2026".to_owned()),
            bubbles,
        ]
        .spacing(16)
        .width(Fill)
        .max_width(600)
        .into()
    }

    /// Показывает каталог SVG с поиском и выбором размера, формы и цвета.
    fn icons(&self) -> Element<'_, Message> {
        let tones = [
            ("По умолчанию", IconTone::Inherit),
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
        let query = self.icon_query.trim().to_ascii_lowercase();
        let mut matches: Vec<_> = IconName::ALL
            .iter()
            .copied()
            .filter(|name| match name {
                IconName::HomeFilled => false,
                IconName::Home => IconName::HomeFilled.as_str().contains(&query),
                _ => name.as_str().to_ascii_lowercase().contains(&query),
            })
            .collect();
        matches.sort_unstable_by_key(|name| name.as_str().to_ascii_lowercase());
        let previews: Element<'_, Message> = if matches.is_empty() {
            text("Иконки не найдены").into()
        } else {
            row(matches.into_iter().map(|name| {
                container(column![
                    container(icon(
                        name,
                        self.icon_size,
                        self.icon_tone,
                        Some(self.icon_variant),
                    ))
                    .center_x(Fill)
                    .center_y(64),
                    container(text(name.as_str()).size(13).center())
                        .center_x(Fill)
                        .center_y(38),
                ])
                .width(128)
                .padding([8, 6])
                .style(container::rounded_box)
                .into()
            }))
            .spacing(12)
            .wrap()
            .into()
        };

        let settings = column![
            column![text("Цвет"), tones.wrap()].spacing(8),
            column![text("Размер, px"), sizes.wrap()].spacing(8),
            row![
                text("Форма Home"),
                option_button(
                    "Outline",
                    self.icon_variant == IconVariant::Outline,
                    Message::IconVariantChanged(IconVariant::Outline)
                ),
                option_button(
                    "Filled",
                    self.icon_variant == IconVariant::Filled,
                    Message::IconVariantChanged(IconVariant::Filled)
                ),
            ]
            .spacing(8)
            .align_y(iced::Center)
            .wrap(),
            text_input(
                "Поиск по имени",
                &self.icon_query,
                FieldSize::Regular,
                Some(Message::IconQueryChanged)
            ),
        ]
        .spacing(12);
        let catalog = Column::new()
            .push(
                row![
                    add_button(),
                    text(format!("Нажатий: {}", self.button_presses)).size(14),
                ]
                .spacing(12)
                .align_y(iced::Center),
            )
            .push(previews)
            .spacing(16)
            .width(Fill)
            .max_width(760);

        column![
            container(settings.width(Fill).max_width(760))
                .padding([0, 28])
                .center_x(Fill),
            scrollable(container(catalog).padding([12, 28]).center_x(Fill))
                .id("gallery-content")
                .height(Fill),
        ]
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
            leading_icon: Some(icon(IconName::Plus, 16, IconTone::Inherit, None)),
            ..ButtonOptions::default()
        },
        Some(Message::ButtonPressed("Добавить")),
    )
}

/// Показывает иконку действия редактора в слоте шириной 32 px.
fn composer_slot(name: IconName) -> Element<'static, Message> {
    container(icon(name, 24, IconTone::Muted, None))
        .center(32)
        .into()
}

/// Создаёт тестовое изображение для проверки круглой обрезки и масштабирования.
fn example_avatar_photo() -> AvatarPhoto {
    let mut pixels = Vec::new();
    for y in 0..64 {
        for x in 0..80 {
            // Рисуем два полукруга и контрастный фон, чтобы были заметны края обрезки.
            let head = (x - 40) * (x - 40) + (y - 23) * (y - 23) < 11 * 11;
            let shoulders = (x - 40) * (x - 40) + (y - 65) * (y - 65) < 25 * 25;
            let color = match (head, shoulders) {
                (true, _) => [0xff, 0xd0, 0xac, 0xff],
                (_, true) => [0x30, 0x55, 0x79, 0xff],
                _ => [0x79, 0x98 + y as u8, 0xaf + x as u8, 0xff],
            };
            pixels.extend_from_slice(&color);
        }
    }
    AvatarPhoto::from_rgba(80, 64, pixels).expect("размер примера совпадает с числом пикселей")
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
