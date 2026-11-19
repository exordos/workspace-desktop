//! Рисует постоянную шапку и меню аккаунтов в окне GPUI.
//! Подсветка, фокус и прокрутка принадлежат меню. Выбранный вход сообщает оболочка.

use std::sync::Arc;

use gpui::{
    App, Context, Div, EventEmitter, FocusHandle, ImageSource, RenderImage, Resource, ScrollHandle,
    SharedString, Stateful, Window, div, img, point, prelude::*, px,
};

use super::{
    Avatar, AvatarSize, BadgeTone, Button, ButtonAppearance, Icon, PANEL_RADIUS, Palette, TextRole,
    controls::activate,
    icons::ORGANIZATION_LOGO,
    label,
    popup::{MenuInput, bind_menu_keys, menu_layer, menu_navigation, menu_surface, tooltip},
    theme::{NAVIGATION_CHIP_RADIUS, ellipsized_label},
    unread_badge,
};
use crate::gpui_client::contracts::AccountSession;

const HEIGHT: f32 = 77.;
const CHAT_SIZE: f32 = 48.;
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

/// Действие верхней панели для текущего входа.
pub(in crate::gpui_client) enum TopBarAction {
    OpenProfile, // Открывает или закрывает корневую страницу профиля.
}

/// Самостоятельный компонент верхней панели; меню живёт вместе с окном.
pub struct TopBar {
    accounts: Vec<TopBarAccount>, // Последний снимок из оболочки.
    profile_avatar: Option<(AccountSession, Arc<RenderImage>)>, // Готовое фото входа, переданное окном.
    quick_focus: Vec<(AccountSession, FocusHandle)>, // Фокус быстрых кнопок закреплён за входом.
    profile_focus: FocusHandle,                      // Кнопка профиля и точка возврата из панели.
    trigger_focus: FocusHandle,                      // Сюда возвращается фокус после закрытия меню.
    menu_focus: FocusHandle, // Одна точка клавиатурного ввода для всего меню.
    menu_scroll: ScrollHandle, // Удерживает активную строку в видимой части меню.
    highlight: Option<AccountSession>, // Подсвеченная строка. None означает закрытое меню.
}

impl EventEmitter<TopBarSelection> for TopBar {}
impl EventEmitter<TopBarAction> for TopBar {}

impl TopBar {
    /// Создаёт закрытое меню и независимые точки фокуса.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            accounts: Vec::new(),
            profile_avatar: None,
            profile_focus: cx.focus_handle(),
            quick_focus: Vec::new(),
            trigger_focus: cx.focus_handle(),
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

    /// Возвращает точку фокуса, из которой открыли профиль.
    pub(in crate::gpui_client) fn profile_focus(&self) -> FocusHandle {
        self.profile_focus.clone()
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
        if self
            .highlight
            .as_ref()
            .is_some_and(|highlight| !accounts.iter().any(|account| account.session == *highlight))
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
    fn first_account(accounts: &[TopBarAccount]) -> Option<AccountSession> {
        accounts
            .iter()
            .find(|account| account.selected)
            .or_else(|| accounts.first())
            .map(|account| account.session.clone())
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
        window.focus(&self.trigger_focus);
        cx.notify();
    }

    /// Находит подсвеченный вход в актуальном снимке.
    fn highlight_index(&self) -> Option<usize> {
        self.accounts
            .iter()
            .position(|account| self.highlight.as_ref() == Some(&account.session))
    }

    /// Выбирает соседнюю строку или край списка и показывает её при прокрутке.
    fn highlight_at(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(account) = self.accounts.get(index) {
            self.highlight = Some(account.session.clone());
            self.menu_scroll.scroll_to_item(index);
            cx.notify();
        }
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
        let highlighted = self.highlight.as_ref() == Some(&session);
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
                ),
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
                    self.highlight_at((index + 1).min(self.accounts.len() - 1), cx);
                }
            }
            MenuInput::First => self.highlight_at(0, cx),
            MenuInput::Last => self.highlight_at(self.accounts.len().saturating_sub(1), cx),
            MenuInput::Select => {
                if let Some(session) = self.highlight.clone() {
                    self.choose(&session, window, cx);
                }
            }
            MenuInput::Close => self.dismiss_menu(window, cx),
        }
    }

    /// Показывает активный аккаунт и открывает его панель профиля.
    fn active_identity(
        &self,
        account: &TopBarAccount,
        side: gpui::Pixels,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let initials: String = account.label.chars().next().into_iter().collect();
        let mut avatar = Avatar::from_initials(initials);
        avatar.size = AvatarSize::Small;
        avatar.image = self
            .profile_avatar
            .as_ref()
            .filter(|(session, _)| *session == account.session)
            .map(|(_, image)| ImageSource::Render(image.clone()));
        let has_image = avatar.image.is_some();
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
                        .h(px(40.))
                        .min_w_0()
                        .px_1()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(gpui::transparent_black())
                        .flex()
                        .items_center()
                        .gap_2()
                        .overflow_hidden()
                        .hover(|row| row.bg(palette.card_hover))
                        .child(avatar.render(palette).when(has_image, |photo| {
                            photo.debug_selector(|| "workspace-profile-avatar-photo".into())
                        }))
                        .child(
                            div()
                                .min_w_0()
                                .w(px(164.))
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
                        .child(Icon::ChevronDown.element(px(16.), palette.text_secondary)),
                    Some(&self.profile_focus),
                    palette,
                    move |_, app| {
                        let _ = target.update(app, |_, cx| cx.emit(TopBarAction::OpenProfile));
                    },
                )
                .debug_selector(|| "workspace-profile-trigger".into())
                .tooltip(move |_, app| tooltip(palette, "Профиль".into(), app)),
            )
    }
}

impl Render for TopBar {
    /// Ставит «Чаты» в геометрический центр окна и оставляет меню доступным при сужении.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let viewport = window.viewport_size();
        let width = f32::from(viewport.width);
        let chat = CHAT_SIZE.min((width - 2. * TRIGGER_SIZE).max(0.));
        let side = (width - chat) / 2.;
        let inset = (side - TRIGGER_SIZE).clamp(0., START_INSET);
        let quick = quick_count(self.accounts.len(), side - inset);
        let hidden = self.accounts.len() - quick;
        let overflow = hidden > 0 && side - inset >= TRIGGER_SIZE + GAP + OVERFLOW_SIZE;
        let trigger_x = inset
            + quick as f32 * (QUICK_SIZE + GAP)
            + if overflow { OVERFLOW_SIZE + GAP } else { 0. };

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
                                    tooltip(palette, "Выбрать аккаунт".into(), app)
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .id("workspace-top-bar-chats")
                    .debug_selector(|| "workspace-top-bar-chats".to_owned())
                    .w(px(chat))
                    .h(px(CHAT_SIZE))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .rounded(NAVIGATION_CHIP_RADIUS)
                    .border_1()
                    .border_color(palette.border)
                    .bg(palette.card_selected)
                    .child(Icon::Chat.element(px(chat.min(24.)), palette.text))
                    .tooltip(move |_, app| tooltip(palette, "Чаты".into(), app)),
            )
            .children(
                self.accounts
                    .iter()
                    .find(|account| account.selected)
                    .map(|account| self.active_identity(account, px(side), palette, cx)),
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
