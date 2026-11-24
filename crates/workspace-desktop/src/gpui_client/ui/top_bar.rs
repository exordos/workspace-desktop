//! Рисует постоянную шапку и меню аккаунтов в окне GPUI.
//! Подсветка, фокус и прокрутка принадлежат меню. Выбранный вход сообщает оболочка.

use std::sync::Arc;

use gpui::{
    App, Context, Div, EventEmitter, FocusHandle, ImageSource, RenderImage, Resource, ScrollHandle,
    SharedString, Stateful, Window, div, img, point, prelude::*, px,
};
use workspace_core::directory::{ProfilePresence, UserProfile};

use super::{
    Avatar, AvatarPresence, BadgeTone, Button, ButtonAppearance, Icon, PANEL_RADIUS, Palette,
    TextRole,
    controls::activate,
    icons::ORGANIZATION_LOGO,
    label,
    popup::{MenuInput, bind_menu_keys, menu_layer, menu_navigation, menu_surface, tooltip},
    theme::{ellipsized_label, label_width},
    unread_badge,
};
use crate::gpui_client::contracts::AccountSession;

const HEIGHT: f32 = 77.;
const SECTION_SIZE: f32 = 48.;
const SECTION_GAP: f32 = 12.;
const SECTION_ICON_SIZE: f32 = 32.;
const QUICK_SIZE: f32 = 40.;
const TRIGGER_SIZE: f32 = 32.;
const GAP: f32 = 6.;
const OVERFLOW_SIZE: f32 = 34.;
const START_INSET: f32 = 28.;
const END_INSET: f32 = 12.;

/// Строка аккаунта, подготовленная оболочкой для отображения.
pub struct TopBarAccount {
    pub session: AccountSession, // Вход, который выберет оболочка при нажатии.
    pub label: SharedString,     // Имя аккаунта с состоянием входа, если есть ошибка.
    pub detail: SharedString,    // Сервер и проект.
    pub selected: bool,          // Этот вход сейчас открыт в окне.
    pub unread: Option<u32>,     // Подтверждённый счётчик, None до первого ответа.
}

/// Пользователь попросил открыть конкретный вход.
pub struct TopBarSelection(pub AccountSession); // Вход, видимый на момент действия.

/// Раздел, который открывает кнопка верхней панели.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TopBarSection {
    #[default]
    Chats, // Мессенджер с текущим разговором.
    Calendar, // Календарь.
    Mail,     // Почта.
    Contacts, // Контакты.
    Calls,    // Звонки.
}

impl TopBarSection {
    const ALL: [Self; 5] = [
        Self::Chats,
        Self::Calendar,
        Self::Mail,
        Self::Contacts,
        Self::Calls,
    ];

    /// Даёт подпись для текущего языка интерфейса.
    pub(in crate::gpui_client) fn localized_title(self, app: &App) -> &'static str {
        let locale = crate::gpui_client::preferences::Preferences::read(app).language;
        locale.tr(match self {
            Self::Chats => "desktop.nav.chats",
            Self::Calendar => "nav.calendar",
            Self::Mail => "nav.mail",
            Self::Contacts => "desktop.nav.contacts",
            Self::Calls => "nav.calls",
        })
    }

    /// Часть идентификатора кнопки и страницы раздела.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Chats => "chats",
            Self::Calendar => "calendar",
            Self::Mail => "mail",
            Self::Contacts => "contacts",
            Self::Calls => "calls",
        }
    }

    /// Выбирает иконку раздела из набора Workspace.
    fn icon(self) -> Icon {
        match self {
            Self::Chats => Icon::ChatBubble,
            Self::Calendar => Icon::Calendar,
            Self::Mail => Icon::Envelope,
            Self::Contacts => Icon::Group,
            Self::Calls => Icon::Phone,
        }
    }
}

/// Действие шапки, которое открывает отдельный экран.
pub enum TopBarAction {
    AddAccount,                 // Начать вход в ещё один аккаунт.
    OpenProfile,                // Открыть панель выбранного аккаунта.
    OpenSection(TopBarSection), // Открыть указанный раздел текущего аккаунта.
}

/// Строка, на которой остановилась клавиатурная навигация.
#[derive(Clone, PartialEq)]
enum MenuRow {
    Account(AccountSession), // Существующий вход.
    AddAccount,              // Последняя строка под списком входов.
}

/// Самостоятельный компонент верхней панели; меню живёт вместе с окном.
pub struct TopBar {
    section: TopBarSection,       // Выбранный раздел из состояния окна.
    accounts: Vec<TopBarAccount>, // Последний снимок из оболочки.
    quick_focus: Vec<(AccountSession, FocusHandle)>, // Фокус быстрых кнопок закреплён за входом.
    trigger_focus: FocusHandle,   // Сюда возвращается фокус после закрытия меню.
    profile_focus: FocusHandle,   // Кнопка профиля и точка возврата из панели.
    profile_avatar: Option<(AccountSession, Arc<RenderImage>)>, // Готовое фото входа, переданное окном.
    profile_details: Option<(AccountSession, UserProfile)>,     // Имя и статус выбранного входа.
    menu_focus: FocusHandle, // Одна точка клавиатурного ввода для всего меню.
    menu_scroll: ScrollHandle, // Удерживает активную строку в видимой части меню.
    highlight: Option<MenuRow>, // Подсвеченная строка. None означает закрытое меню.
}

impl EventEmitter<TopBarSelection> for TopBar {}
impl EventEmitter<TopBarAction> for TopBar {}

impl TopBar {
    /// Создаёт закрытое меню и независимые точки фокуса.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            section: TopBarSection::default(),
            accounts: Vec::new(),
            quick_focus: Vec::new(),
            trigger_focus: cx.focus_handle(),
            profile_focus: cx.focus_handle(),
            profile_avatar: None,
            profile_details: None,
            menu_focus: cx.focus_handle(),
            menu_scroll: ScrollHandle::new(),
            highlight: None,
        }
    }

    /// Подменяет инициалы готовым изображением указанного входа.
    pub(in crate::gpui_client) fn set_profile_avatar(
        &mut self,
        avatar: Option<(AccountSession, Arc<RenderImage>)>,
        cx: &mut Context<Self>,
    ) {
        if self.profile_avatar != avatar {
            self.profile_avatar = avatar;
            cx.notify();
        }
    }

    /// Показывает сведения только для поколения, которому они принадлежат.
    pub(in crate::gpui_client) fn set_profile_details(
        &mut self,
        details: Option<(AccountSession, UserProfile)>,
        cx: &mut Context<Self>,
    ) {
        if self.profile_details != details {
            self.profile_details = details;
            cx.notify();
        }
    }

    /// Подсвечивает раздел, который показало окно.
    pub(crate) fn set_section(&mut self, section: TopBarSection, cx: &mut Context<Self>) {
        self.section = section;
        cx.notify();
    }

    /// Ограничивает обработку клавиш открытым списком, не меняя обход остальных полей окна.
    pub fn bind_keys(app: &mut App) {
        bind_menu_keys(app);
    }

    /// Сохраняет фокус и подсветку у того же входа при обновлении непрочитанного.
    pub fn set_accounts(&mut self, accounts: Vec<TopBarAccount>, cx: &mut Context<Self>) {
        self.quick_focus
            .retain(|(session, _)| accounts.iter().any(|account| account.session == *session));
        for account in &accounts {
            if !self
                .quick_focus
                .iter()
                .any(|(session, _)| *session == account.session)
            {
                self.quick_focus
                    .push((account.session.clone(), cx.focus_handle()));
            }
        }
        if accounts.is_empty() {
            self.highlight = None;
        } else if let Some(MenuRow::Account(highlight)) = &self.highlight
            && !accounts.iter().any(|account| account.session == *highlight)
        {
            self.highlight = Self::first_account(&accounts);
        }
        self.accounts = accounts;
        cx.notify();
    }

    /// Возвращает состояние меню тесту, не полагаясь на геометрию прошлых кадров GPUI.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(crate) fn menu_is_open(&self) -> bool {
        self.highlight.is_some()
    }

    /// Подтверждает тесту, что меню оставило клавиатурный фокус у себя.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(crate) fn menu_has_focus(&self, window: &Window) -> bool {
        self.menu_focus.is_focused(window)
    }

    /// Находит вход, с которого следует начать обход меню.
    fn first_account(accounts: &[TopBarAccount]) -> Option<MenuRow> {
        accounts
            .iter()
            .find(|account| account.selected)
            .or_else(|| accounts.first())
            .map(|account| MenuRow::Account(account.session.clone()))
    }

    /// Открывает список и передаёт ему фокус или закрывает уже открытый список.
    fn toggle_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.highlight.is_some() {
            self.dismiss_menu(window, cx);
            return;
        }
        self.highlight = Self::first_account(&self.accounts);
        if let Some(index) = self.highlight_index() {
            self.menu_scroll.scroll_to_item(index);
            window.focus(&self.menu_focus);
            cx.notify();
        }
    }

    /// Закрывает меню и возвращает фокус кнопке раскрытия.
    fn dismiss_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.highlight = None;
        self.focus_account_menu(window);
        cx.notify();
    }

    /// Возвращает фокус к закрытому меню аккаунтов.
    pub(crate) fn focus_account_menu(&self, window: &mut Window) {
        window.focus(&self.trigger_focus);
    }

    /// Даёт правой панели точку возврата к кнопке профиля.
    pub(crate) fn profile_focus(&self) -> FocusHandle {
        self.profile_focus.clone()
    }

    /// Находит подсвеченную строку, включая действие под списком входов.
    fn highlight_index(&self) -> Option<usize> {
        match self.highlight.as_ref()? {
            MenuRow::Account(session) => {
                self.accounts.iter().position(|row| row.session == *session)
            }
            MenuRow::AddAccount => Some(self.accounts.len()),
        }
    }

    /// Перемещает подсветку и прокручивает список к новой строке.
    fn highlight_at(&mut self, index: usize, cx: &mut Context<Self>) {
        self.highlight = Some(
            self.accounts
                .get(index)
                .map_or(MenuRow::AddAccount, |account| {
                    MenuRow::Account(account.session.clone())
                }),
        );
        self.menu_scroll.scroll_to_item(index);
        cx.notify();
    }

    /// Закрывает меню и просит оболочку открыть форму входа.
    fn add_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_menu(window, cx);
        cx.emit(TopBarAction::AddAccount);
    }

    /// Передаёт актуальный вход оболочке, которая повторно проверяет выбор.
    fn choose(&mut self, session: &AccountSession, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .accounts
            .iter()
            .any(|account| account.session == *session)
        {
            return;
        }
        if self.highlight.is_some() {
            self.dismiss_menu(window, cx);
        }
        cx.emit(TopBarSelection(session.clone()));
    }

    /// Рисует быструю кнопку с логотипом, точкой непрочитанного и подписью при наведении.
    fn quick_button(
        &self,
        index: usize,
        account: &TopBarAccount,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let session = account.session.clone();
        let target = cx.entity().downgrade();
        let description = account.label.clone();
        let focus = self
            .quick_focus
            .iter()
            .find_map(|(id, focus)| (id == &session).then_some(focus));
        activate(
            div()
                .id(account_element_id("quick", &session))
                .relative()
                .size(px(QUICK_SIZE))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .border_1()
                .border_color(gpui::transparent_black())
                .hover(|style| style.bg(palette.card_hover))
                .when(account.selected, |button| button.bg(palette.card_selected))
                .child(organization_logo(palette, account.selected, 36.))
                .when(account.unread.is_some_and(|count| count > 0), |button| {
                    button.child(
                        div()
                            .id("unread-dot")
                            .debug_selector(move || format!("workspace-account-dot-{index}"))
                            .absolute()
                            .top_0()
                            .right_0()
                            .size(px(10.))
                            .rounded_full()
                            .border_1()
                            .border_color(palette.surface)
                            .bg(palette.unread),
                    )
                }),
            focus,
            palette,
            move |window, app| {
                let _ = target.update(app, |view, cx| view.choose(&session, window, cx));
            },
        )
        .debug_selector(move || format!("workspace-account-{index}"))
        .tooltip(move |_, app| tooltip(palette, description.clone(), app))
    }

    /// Рисует кликабельную строку меню и числовой счётчик поверх логотипа.
    fn menu_row(
        &self,
        index: usize,
        account: &TopBarAccount,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let session = account.session.clone();
        let highlighted = self.highlight.as_ref() == Some(&MenuRow::Account(session.clone()));
        let description: SharedString = format!("{} · {}", account.label, account.detail).into();
        div()
            .id(account_element_id("menu", &session))
            .debug_selector(move || format!("workspace-account-menu-row-{index}"))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .p_1()
            .min_h(px(48.))
            .rounded(px(8.))
            .border_1()
            .border_color(if highlighted {
                palette.accent
            } else {
                gpui::transparent_black()
            })
            .when(account.selected || highlighted, |row| {
                row.bg(palette.card_selected)
            })
            .hover(|row| row.bg(palette.card_hover))
            .cursor_pointer()
            .on_click(cx.listener(move |view, _, window, cx| view.choose(&session, window, cx)))
            .tooltip(move |_, app| tooltip(palette, description.clone(), app))
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(organization_logo(palette, account.selected, 36.))
                    .children(account.unread.and_then(|count| {
                        unread_badge(palette, u64::from(count), BadgeTone::Accent).map(|badge| {
                            div()
                                .id("unread-count")
                                .debug_selector(move || format!("workspace-account-count-{index}"))
                                .absolute()
                                .top_0()
                                .right_0()
                                .child(badge)
                        })
                    })),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .child(ellipsized_label(
                        palette,
                        TextRole::Label,
                        account.label.clone(),
                    ))
                    .child(ellipsized_label(
                        palette,
                        TextRole::Caption,
                        account.detail.clone(),
                    )),
            )
    }

    /// Показывает действие входа последней строкой того же меню.
    fn add_account_row(&self, palette: Palette, cx: &Context<Self>) -> Stateful<Div> {
        div()
            .id("workspace-add-account")
            .debug_selector(|| "workspace-add-account".into())
            .min_h(px(36.))
            .px_2()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .rounded(px(8.))
            .border_1()
            .border_color(gpui::transparent_black())
            .when(self.highlight == Some(MenuRow::AddAccount), |row| {
                row.border_color(palette.accent).bg(palette.card_selected)
            })
            .hover(|row| row.bg(palette.card_hover))
            .cursor_pointer()
            .on_click(cx.listener(|view, _, window, cx| view.add_account(window, cx)))
            .child(Icon::Add.element(px(16.), palette.text_secondary))
            .child(ellipsized_label(
                palette,
                TextRole::Label,
                crate::gpui_client::preferences::Preferences::read(cx)
                    .language
                    .tr("desktop.auth.addAccount"),
            ))
    }

    /// Собирает прокручиваемый список и перехватывает нажатие вне его границ.
    fn menu(
        &self,
        palette: Palette,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let height = px(320.).min((viewport.height - px(HEIGHT + 4.)).max(px(32.)));
        menu_surface(
            "workspace-account-menu",
            palette,
            viewport,
            px(320.),
            height,
        )
        .debug_selector(|| "workspace-account-menu".to_owned())
        .on_mouse_down_out(cx.listener(|view, _, window, cx| {
            cx.stop_propagation();
            view.dismiss_menu(window, cx);
        }))
        .child(
            div()
                .id("workspace-account-menu-scroll")
                .min_h_0()
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(&self.menu_scroll)
                .children(
                    self.accounts
                        .iter()
                        .enumerate()
                        .map(|(index, account)| self.menu_row(index, account, palette, cx)),
                )
                .child(self.add_account_row(palette, cx)),
        )
    }

    /// Перемещает подсветку списка или выполняет действие текущего входа.
    fn menu_input(&mut self, input: MenuInput, window: &mut Window, cx: &mut Context<Self>) {
        match input {
            MenuInput::Previous => {
                if let Some(index) = self.highlight_index() {
                    self.highlight_at(index.saturating_sub(1), cx);
                }
            }
            MenuInput::Next => {
                if let Some(index) = self.highlight_index() {
                    self.highlight_at((index + 1).min(self.accounts.len()), cx);
                }
            }
            MenuInput::First => self.highlight_at(0, cx),
            MenuInput::Last => self.highlight_at(self.accounts.len(), cx),
            MenuInput::Select => match self.highlight.clone() {
                Some(MenuRow::Account(session)) => self.choose(&session, window, cx),
                Some(MenuRow::AddAccount) => self.add_account(window, cx),
                None => {}
            },
            MenuInput::Close => self.dismiss_menu(window, cx),
        }
    }
    /// Рисует кнопку раздела с общей зоной для клика и наведения.
    fn section_button(
        &self,
        section: TopBarSection,
        size: f32,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let target = cx.weak_entity();
        let active = self.section == section;
        let available = !self.accounts.is_empty();
        let id: SharedString = format!("workspace-top-bar-{}", section.id()).into();
        let selector = id.clone();
        let icon = section
            .icon()
            .element(
                px(size.min(SECTION_ICON_SIZE)),
                if active { palette.text } else { palette.icon },
            )
            .when(available, |icon| {
                icon.group_hover(id.clone(), |style| style.text_color(palette.text))
            });
        let button = div()
            .id(id.clone())
            .debug_selector(move || selector.to_string())
            .size(px(size))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .rounded(PANEL_RADIUS)
            .border_1()
            .border_color(if active {
                palette.border
            } else {
                gpui::transparent_black()
            })
            .when(active, |button| button.bg(palette.card_selected))
            .child(icon);
        if !available {
            return button;
        }
        activate(
            button.group(id).hover(|style| {
                style
                    .bg(palette.icon_hover_fill.opacity(0.35))
                    .border_color(palette.border)
            }),
            None,
            palette,
            move |_, app| {
                let _ = target.update(app, |_, cx| cx.emit(TopBarAction::OpenSection(section)));
            },
        )
    }

    /// Показывает активный аккаунт и открывает его панель профиля.
    fn active_identity(
        &self,
        account: &TopBarAccount,
        side: gpui::Pixels,
        palette: Palette,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let profile = self
            .profile_details
            .as_ref()
            .filter(|(session, _)| *session == account.session)
            .map(|(_, profile)| profile);
        let name: SharedString = profile
            .map(|profile| profile.display_name.clone().into())
            .unwrap_or_else(|| account.label.clone());
        let language = crate::gpui_client::preferences::Preferences::read(cx).language;
        let detail: SharedString = profile
            .map(|profile| {
                profile
                    .status_label()
                    .unwrap_or_else(|| {
                        match profile.presence {
                            Some(ProfilePresence::Active) => language.tr("a11y.online"),
                            Some(ProfilePresence::Idle | ProfilePresence::DoNotDisturb) => {
                                language.tr("a11y.away")
                            }
                            Some(ProfilePresence::Offline) => language.tr("presence.offline"),
                            None => "",
                        }
                        .to_owned()
                    })
                    .into()
            })
            .unwrap_or_else(|| account.detail.clone());
        let initials: String = name.chars().next().into_iter().collect();
        let mut avatar = Avatar::from_initials(initials);
        avatar.presence = match profile.and_then(|profile| profile.presence) {
            Some(ProfilePresence::Active) => AvatarPresence::Online,
            Some(ProfilePresence::Idle | ProfilePresence::DoNotDisturb) => AvatarPresence::Away,
            _ => AvatarPresence::Hidden,
        };
        avatar.image = self
            .profile_avatar
            .as_ref()
            .filter(|(session, _)| *session == account.session)
            .map(|(_, image)| ImageSource::Render(image.clone()));
        let has_image = avatar.image.is_some();
        let label_width = label_width(palette, TextRole::Label, &name, window).max(label_width(
            palette,
            TextRole::Caption,
            &detail,
            window,
        ));
        let target = cx.weak_entity();
        div()
            .w(side)
            .pr(px(END_INSET))
            .pl_2()
            .min_w_0()
            .flex_none()
            .flex()
            .items_center()
            .justify_end()
            .child(
                activate(
                    div()
                        .id("workspace-profile-trigger")
                        .h(px(48.))
                        .min_w_0()
                        // С учётом границы фокуса отступ от края до аватара равен 6 px.
                        .p(px(5.))
                        .rounded(PANEL_RADIUS)
                        .border_1()
                        .border_color(gpui::transparent_black())
                        .flex()
                        .items_center()
                        .gap_2()
                        .overflow_hidden()
                        .hover(|row| row.bg(palette.card_selected))
                        .child(
                            avatar
                                .render_at_with_background(palette, px(36.), None)
                                .when(has_image, |photo| {
                                    photo.debug_selector(|| "workspace-profile-avatar-photo".into())
                                }),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .w(label_width)
                                .max_w(px(164.))
                                .child(ellipsized_label(palette, TextRole::Label, name))
                                .child(ellipsized_label(palette, TextRole::Caption, detail)),
                        )
                        .child(Icon::ChevronDown.element(px(16.), palette.text_muted)),
                    Some(&self.profile_focus),
                    palette,
                    move |_, app| {
                        let _ = target.update(app, |_, cx| cx.emit(TopBarAction::OpenProfile));
                    },
                )
                .debug_selector(|| "workspace-profile-trigger".into()),
            )
    }
}

impl Render for TopBar {
    /// Центрирует разделы и оставляет меню аккаунтов доступным при сужении.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let viewport = window.viewport_size();
        let width = f32::from(viewport.width);
        let count = TopBarSection::ALL.len() as f32;
        let available = (width - 2. * TRIGGER_SIZE).max(0.);
        let section_gap = SECTION_GAP.min(available / (count * 2.));
        let gaps = (count - 1.) * section_gap;
        let section_size = SECTION_SIZE.min((available - gaps) / count);
        let side = (width - (section_size * count + gaps)) / 2.;
        let quick = quick_count(self.accounts.len(), side);
        let hidden = self.accounts.len() - quick;
        let quick_width = quick as f32 * (QUICK_SIZE + GAP);
        let overflow = hidden > 0 && side - quick_width >= TRIGGER_SIZE + GAP + OVERFLOW_SIZE;
        let trigger_offset = quick_width + if overflow { OVERFLOW_SIZE + GAP } else { 0. };
        // Сначала оставляем место кнопкам аккаунтов, затем внешнему отступу.
        let inset = (side - trigger_offset - TRIGGER_SIZE).clamp(0., START_INSET);
        let trigger_x = inset + trigger_offset;

        let target = cx.entity().downgrade();
        let mut trigger = Button::new("workspace-account-menu-trigger", "");
        trigger.icon = Some(Icon::ChevronDown);
        trigger.appearance = ButtonAppearance::Ghost;

        div()
            .id("workspace-top-bar")
            .debug_selector(|| "workspace-top-bar".to_owned())
            .w_full()
            .h(px(HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .rounded_b(PANEL_RADIUS)
            .bg(palette.surface)
            .child(
                div()
                    .w(px(side))
                    .pl(px(inset))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(GAP))
                    .children(
                        self.accounts
                            .iter()
                            .take(quick)
                            .enumerate()
                            .map(|(index, account)| self.quick_button(index, account, palette, cx)),
                    )
                    .when(overflow, |left| {
                        left.child(
                            div()
                                .id("workspace-account-overflow")
                                .debug_selector(|| "workspace-account-overflow".to_owned())
                                .w(px(OVERFLOW_SIZE))
                                .flex_none()
                                .flex()
                                .justify_center()
                                .child(label(palette, TextRole::Caption, format!("+{hidden}"))),
                        )
                    })
                    .when(!self.accounts.is_empty(), |left| {
                        left.child(
                            trigger
                                .render(palette, Some(&self.trigger_focus), move |window, app| {
                                    let _ =
                                        target.update(app, |view, cx| view.toggle_menu(window, cx));
                                })
                                .debug_selector(|| "workspace-account-menu-trigger".to_owned())
                                .tooltip(move |_, app| {
                                    let locale =
                                        crate::gpui_client::preferences::Preferences::read(app)
                                            .language;
                                    tooltip(palette, locale.tr("auth.selectServer").into(), app)
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .id("workspace-top-bar-sections")
                    .debug_selector(|| "workspace-top-bar-sections".into())
                    .flex_none()
                    .flex()
                    .gap(px(section_gap))
                    .children(
                        TopBarSection::ALL
                            .map(|section| self.section_button(section, section_size, palette, cx)),
                    ),
            )
            .children(
                self.accounts
                    .iter()
                    .find(|account| account.selected)
                    .map(|account| self.active_identity(account, px(side), palette, window, cx)),
            )
            .when(self.highlight.is_some(), |bar| {
                let origin = point(px(trigger_x), px((HEIGHT + TRIGGER_SIZE) / 2. + 6.));
                menu_navigation(bar, &self.menu_focus, cx, Self::menu_input).children(menu_layer(
                    viewport,
                    origin,
                    self.menu(palette, viewport, cx),
                ))
            })
    }
}

/// Считает быстрые кнопки, оставляя место стрелке и счётчику скрытых аккаунтов.
fn quick_count(total: usize, available: f32) -> usize {
    (1..=total.min(3))
        .rev()
        .find(|&count| {
            let overflow = if count < total {
                OVERFLOW_SIZE + GAP
            } else {
                0.
            };
            count as f32 * (QUICK_SIZE + GAP) + TRIGGER_SIZE + overflow <= available
        })
        .unwrap_or(0)
}

/// Закрепляет нажатие GPUI за входом вместо позиции в списке.
fn account_element_id(group: &str, session: &AccountSession) -> SharedString {
    format!(
        "{group}-{}-{}",
        session.owner.account_id.0, session.generation.0
    )
    .into()
}

/// Рисует встроенный многоцветный логотип организации.
fn organization_logo(palette: Palette, selected: bool, size: f32) -> Div {
    div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .rounded(PANEL_RADIUS)
        .border_1()
        .border_color(if selected {
            palette.accent
        } else {
            palette.border
        })
        .bg(palette.card)
        .child(
            img(ImageSource::Resource(Resource::Embedded(
                ORGANIZATION_LOGO.into(),
            )))
            .size(px(size - 4.)),
        )
}
