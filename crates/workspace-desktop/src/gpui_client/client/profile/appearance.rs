//! Вложенная панель «Внешний вид». Рисует пять раскрываемых секций и передаёт
//! настройки локальному профилю или существующей панели папок.

use gpui::{
    App, Context, Div, FontWeight, Render, Stateful, WeakEntity, Window, div, prelude::*, px, rgb,
};

use crate::gpui_client::{
    contracts::AccountSession,
    preferences::{
        Language, Preferences,
        appearance::{ChatDensity, SidebarSort, ThemeChoice, ThemePalette},
    },
    right_panel::{PanelKey, RightPanel, Scope},
    sidebar::Sidebar,
    ui::{Icon, Palette, TextRole, activate, label, popup::tooltip},
};

mod background;

use background::BackgroundSection;

/// Ключ раскрываемой группы позволяет держать открытыми несколько секций.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Theme,      // Светлый, тёмный и системный режимы и цветовые палитры.
    Sorting,    // Порядок стримов и топиков.
    Folders,    // Расположение панели папок.
    Density,    // Размер строк каталога чатов.
    Background, // Изображение под историей сообщений.
}

impl Section {
    const ALL: [Self; 5] = [
        Self::Theme,
        Self::Sorting,
        Self::Folders,
        Self::Density,
        Self::Background,
    ];

    /// Возвращает стабильный ключ секции, подпись и значок.
    fn metadata(self, language: Language) -> (&'static str, &'static str, Icon) {
        match self {
            Self::Theme => ("theme", language.tr("settings.themeSettings"), Icon::Draw),
            Self::Sorting => (
                "sorting",
                language.tr("settings.messengerSidebarSortMode"),
                Icon::StreamOrder,
            ),
            Self::Folders => (
                "folder",
                language.tr("settings.folderLayout"),
                Icon::FolderLayout,
            ),
            Self::Density => (
                "density",
                language.tr("settings.chatListDensity"),
                Icon::Topic,
            ),
            Self::Background => (
                "background",
                language.tr("desktop.appearance.title"),
                Icon::Background,
            ),
        }
    }
}

/// Конкретный выбор в открытой группе.
#[derive(Clone, Copy)]
enum Change {
    Theme(ThemeChoice),    // Принудительный режим или следование ОС.
    Palette(ThemePalette), // Цветовая схема независимо от режима.
    Sort(SidebarSort),     // Правило порядка строк.
    Vertical(bool),        // Вертикальная панель папок, иначе горизонтальные вкладки.
    Density(ChatDensity),  // Плотность строк каталога.
}

/// Живое содержимое страницы под общей шапкой стека.
pub(super) struct Appearance {
    host: WeakEntity<RightPanel>, // Панель, которая открывает подтверждение фона.
    session: AccountSession,      // Активный вход на момент перехода.
    sidebar: WeakEntity<Sidebar>, // Рабочий каталог для настройки размещения папок.
    opened: Vec<Section>,         // Одновременно открытые секции.
    background: BackgroundSection, // Кнопки выбора фона, состояние чтения файла.
    error: bool,                  // Показывает ошибку записи общих настроек.
    _sidebar_updates: Option<gpui::Subscription>, // Следит за изменениями папок из их меню.
}

impl Appearance {
    /// Отличает эту страницу от других страниц того же входа в общем стеке.
    fn key(session: &AccountSession) -> PanelKey {
        PanelKey {
            id: "profile-appearance".into(),
            scope: Scope::Account(session.clone()),
        }
    }

    /// Создаёт свернутую страницу для выбранного аккаунта.
    pub(super) fn new(
        host: WeakEntity<RightPanel>,
        session: AccountSession,
        sidebar: WeakEntity<Sidebar>,
        cx: &mut Context<Self>,
    ) -> Self {
        let sidebar_updates = sidebar
            .upgrade()
            .map(|view| cx.observe(&view, |_, _, cx| cx.notify()));
        Self {
            host,
            session,
            sidebar,
            opened: Vec::new(),
            background: BackgroundSection::new(cx),
            error: false,
            _sidebar_updates: sidebar_updates,
        }
    }

    /// Переключает только выбранную группу, сохраняя прочие открытые группы.
    fn toggle(&mut self, section: Section, cx: &mut Context<Self>) {
        if let Some(position) = self.opened.iter().position(|&open| open == section) {
            self.opened.remove(position);
        } else {
            self.opened.push(section);
        }
        cx.notify();
    }

    /// Сразу применяет выбор ко всему окну после успешной записи в профиль.
    fn change(&mut self, change: Change, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .host
            .upgrade()
            .is_some_and(|host| host.read(cx).is_active(&Self::key(&self.session)))
        {
            return;
        }
        if let Change::Vertical(vertical) = change {
            let _ = self.sidebar.update(cx, |sidebar, cx| {
                sidebar.set_folder_placement(&self.session, vertical, cx);
            });
            return;
        }
        let result = Preferences::update(cx, |settings| {
            let appearance = &mut settings.appearance;
            match change {
                Change::Theme(value) => appearance.theme = value,
                Change::Palette(value) => appearance.palette = value,
                Change::Sort(value) => appearance.sort = value,
                Change::Density(value) => appearance.density = value,
                Change::Vertical(_) => unreachable!("расположение хранит панель папок"),
            }
        });
        self.error = result.is_err();
        window.refresh();
        cx.notify();
    }

    /// Связывает выбор с живой страницей; закрытая страница больше его не примет.
    fn select(cx: &Context<Self>, change: Change) -> impl Fn(&mut Window, &mut App) + 'static {
        let view = cx.weak_entity();
        move |window, app| {
            let _ = view.update(app, |view, cx| view.change(change, window, cx));
        }
    }

    /// Возвращает компактную строку раскрытия с отступом 8 px и иконкой 22 px.
    fn trigger(
        &self,
        section: Section,
        index: usize,
        palette: Palette,
        language: Language,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let (id, name, icon) = section.metadata(language);
        let expanded = self.opened.contains(&section);
        let arrow = if expanded {
            Icon::ChevronUp
        } else {
            Icon::ChevronRight
        };
        let trigger = format!("right-panel-appearance-{id}-trigger");
        let view = cx.weak_entity();
        let icon_color = match section {
            Section::Sorting => rgb(0x707070).into(),
            _ => palette.text_secondary,
        };
        let row = div()
            .id(gpui::SharedString::from(trigger.clone()))
            .debug_selector(move || trigger.clone())
            .relative()
            .w_full()
            .min_h(px(44.))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .hover(|style| style.bg(palette.card_hover))
            .child(
                div()
                    .size(px(32.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon.element(px(22.), icon_color)),
            )
            .child(label(palette, TextRole::Body, name).flex_1().min_w_0())
            .child(arrow.element(px(16.), palette.text))
            .when(index != 0, |row| {
                row.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_2()
                        .right_2()
                        .h(px(1.))
                        .bg(palette.border),
                )
            });
        activate(row, None, palette, move |_, app| {
            let _ = view.update(app, |view, cx| view.toggle(section, cx));
        })
        .when(palette.keyboard_focus_visible(), |row| {
            row.focus(|row| row.bg(palette.card_hover))
        })
    }

    /// Рисует рамку раскрытой секции и её вкладки или список выбранных значений.
    fn options(
        &self,
        section: Section,
        palette: Palette,
        language: Language,
        cx: &mut Context<Self>,
    ) -> Div {
        let preferences = &Preferences::read(cx).appearance;
        let (id, ..) = section.metadata(language);
        let debug = format!("right-panel-appearance-{id}-options");
        let panel = div()
            .debug_selector(move || debug.clone())
            .mx_2()
            .rounded(px(8.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface.opacity(0.6))
            .overflow_hidden();
        let selections: &[(&str, &str, bool, Change)] = match section {
            Section::Theme => {
                return panel.child(theme_options(
                    preferences.theme,
                    preferences.palette,
                    palette,
                    language,
                    cx,
                ));
            }
            Section::Background => {
                return panel.child(self.background_options(palette, language, cx));
            }
            Section::Sorting => &[
                (
                    "last-message",
                    "settings.messengerSidebarSortModeLastMessage",
                    preferences.sort == SidebarSort::LastMessage,
                    Change::Sort(SidebarSort::LastMessage),
                ),
                (
                    "unread-first",
                    "settings.messengerSidebarSortModeUnreadFirst",
                    preferences.sort == SidebarSort::UnreadFirst,
                    Change::Sort(SidebarSort::UnreadFirst),
                ),
                (
                    "alphabetical",
                    "settings.messengerSidebarSortModeAlphabetical",
                    preferences.sort == SidebarSort::Alphabetical,
                    Change::Sort(SidebarSort::Alphabetical),
                ),
            ],
            Section::Folders => {
                let vertical = self
                    .sidebar
                    .upgrade()
                    .and_then(|sidebar| sidebar.read(cx).folder_placement(&self.session))
                    .unwrap_or(true);
                &[
                    (
                        "vertical",
                        "settings.folderLayoutVertical",
                        vertical,
                        Change::Vertical(true),
                    ),
                    (
                        "horizontal",
                        "settings.folderLayoutHorizontal",
                        !vertical,
                        Change::Vertical(false),
                    ),
                ]
            }
            Section::Density => &[
                (
                    "standard",
                    "settings.chatListDensityStandard",
                    preferences.density == ChatDensity::Standard,
                    Change::Density(ChatDensity::Standard),
                ),
                (
                    "compact",
                    "settings.chatListDensityCompact",
                    preferences.density == ChatDensity::Compact,
                    Change::Density(ChatDensity::Compact),
                ),
            ],
        };
        panel.children(
            selections
                .iter()
                .enumerate()
                .map(|(index, (id, key, active, change))| {
                    option_row(
                        format!("settings-{id}"),
                        language.tr(key),
                        *active,
                        index != 0,
                        palette,
                        Self::select(cx, *change),
                    )
                }),
        )
    }
}

impl Render for Appearance {
    /// Рисует тело страницы с отдельной прокруткой и стабильными ключами секций.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read(cx).language;
        let mut sections = div()
            .debug_selector(|| "right-panel-appearance-sections".into())
            .flex()
            .flex_col();
        for (index, section) in Section::ALL.into_iter().enumerate() {
            sections = sections.child(self.trigger(section, index, palette, language, cx));
            if self.opened.contains(&section) {
                sections = sections.child(self.options(section, palette, language, cx));
            }
        }
        div()
            .id("profile-appearance-page")
            .debug_selector(|| "profile-appearance-page".into())
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(
                div()
                    .id("appearance-scroll")
                    .debug_selector(|| "right-panel-appearance".into())
                    .size_full()
                    .overflow_y_scroll()
                    .pt_3()
                    .pb_5()
                    .child(sections)
                    .when(self.error, |body| {
                        body.child(
                            label(
                                palette,
                                TextRole::Caption,
                                language.tr("desktop.appearance.saveFailed"),
                            )
                            .m_2()
                            .text_color(palette.danger),
                        )
                    }),
            )
    }
}

/// Рисует вариант списка с тонким разделителем и галочкой у выбранного значения.
fn option_row(
    id: String,
    title: &'static str,
    active: bool,
    separator: bool,
    palette: Palette,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let row = div()
        .id(gpui::SharedString::from(id.clone()))
        .debug_selector(move || id.clone())
        .relative()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .px_2()
        .py_2()
        .hover(|style| style.bg(palette.card_selected))
        .child(
            label(palette, TextRole::Body, title)
                .line_height(px(16.))
                .when(active, |title| title.font_weight(FontWeight::MEDIUM)),
        )
        .when(active, |row| {
            row.child(Icon::Check.element(px(16.), palette.accent))
        })
        .when(separator, |row| {
            row.child(
                div()
                    .absolute()
                    .top_0()
                    .left_2()
                    .right_2()
                    .h(px(1.))
                    .bg(palette.border),
            )
        });
    activate(row, None, palette, on_click).when(palette.keyboard_focus_visible(), |row| {
        row.focus(|row| row.bg(palette.card_selected))
    })
}

/// Рисует трёхпозиционный переключатель и две строки цветовых палитр.
fn theme_options(
    current_mode: ThemeChoice,
    current_palette: ThemePalette,
    palette: Palette,
    language: Language,
    cx: &Context<Appearance>,
) -> Div {
    let modes = [
        (
            ThemeChoice::Light,
            "light",
            Icon::DayTheme,
            18.,
            0xffd633,
            language.tr("settings.themeLight"),
        ),
        (
            ThemeChoice::Dark,
            "dark",
            Icon::NightTheme,
            16.,
            0xf7fdff,
            language.tr("settings.themeDark"),
        ),
        (
            ThemeChoice::System,
            "system",
            Icon::DesktopWindows,
            17.,
            0x707070,
            language.tr("settings.themeSystem"),
        ),
    ];
    let themes = [
        (
            ThemePalette::OrangeWarm,
            "orange-warm",
            language.tr("desktop.appearance.paletteOrangeWarm"),
            0xff8438,
        ),
        (
            ThemePalette::BlueCold,
            "blue-cold",
            language.tr("desktop.appearance.paletteBlueCold"),
            0x7087ff,
        ),
        (
            ThemePalette::BlueMist,
            "blue-mist",
            language.tr("desktop.appearance.paletteBlueMist"),
            0x7d9ddf,
        ),
        (
            ThemePalette::EmeraldChat,
            "emerald-chat",
            language.tr("desktop.appearance.paletteEmerald"),
            0x2ecf72,
        ),
    ];
    div()
        .p_2()
        .flex()
        .flex_col()
        .gap_5()
        .child(
            div()
                .flex()
                .gap_2()
                .p_1()
                .rounded(px(8.))
                .bg(palette.background)
                .children(modes.map(|(mode, id, icon, size, color, name)| {
                    let id = format!("settings-theme-mode-{id}");
                    let active = mode == current_mode;
                    let chip = div()
                        .id(gpui::SharedString::from(id.clone()))
                        .debug_selector(move || id.clone())
                        .flex_1()
                        .h_6()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(8.))
                        .when(active, |chip| chip.bg(palette.card))
                        .when(!active, |chip| {
                            chip.hover(|style| style.bg(palette.card_selected))
                        })
                        .child(icon.element(px(size), rgb(color).into()))
                        .tooltip(move |_, app| tooltip(palette, name.into(), app));
                    activate(
                        chip,
                        None,
                        palette,
                        Appearance::select(cx, Change::Theme(mode)),
                    )
                    .when(palette.keyboard_focus_visible(), |chip| {
                        chip.focus(|style| style.bg(palette.card_selected))
                    })
                })),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .children(themes.chunks(2).map(|row| {
                    div()
                        .flex()
                        .gap_1()
                        .children(row.iter().map(|&(choice, id, name, accent)| {
                            let id = format!("settings-theme-palette-{id}");
                            let active = choice == current_palette;
                            let chip = div()
                                .id(gpui::SharedString::from(id.clone()))
                                .debug_selector(move || id.clone())
                                .flex_1()
                                .min_w_0()
                                .h_8()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .rounded(px(8.))
                                .border_1()
                                .border_color(if active {
                                    palette.accent
                                } else {
                                    gpui::transparent_black()
                                })
                                .bg(palette.background)
                                .when(!active, |chip| {
                                    chip.hover(|style| style.bg(palette.card_selected))
                                })
                                .child(div().size_3().flex_none().rounded_full().bg(rgb(accent)))
                                .child(
                                    label(palette, TextRole::Body, name)
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .min_w_0(),
                                );
                            activate(
                                chip,
                                None,
                                palette,
                                Appearance::select(cx, Change::Palette(choice)),
                            )
                        }))
                })),
        )
}

#[cfg(all(test, feature = "gpui-test"))]
mod tests;
