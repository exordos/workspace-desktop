//! Рисует постоянную шапку и меню аккаунтов в окне GPUI.
//! Подсветка, фокус и прокрутка принадлежат меню. Выбранный вход сообщает оболочка.

use gpui::{
    App, Context, Div, EventEmitter, FocusHandle, ImageSource, KeyBinding, Resource, ScrollHandle,
    SharedString, Stateful, Window, actions, anchored, deferred, div, img, point, prelude::*, px,
};

use super::{
    Avatar, AvatarSize, BadgeTone, Button, ButtonAppearance, Icon, Palette, TextRole, ThemeMode,
    controls::activate, icons::ORGANIZATION_LOGO, label, theme::ellipsized_label, unread_badge,
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

actions!(
    workspace_account_menu,
    [Previous, Next, First, Last, Close, KeepFocus]
);

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

/// Самостоятельный компонент верхней панели; меню живёт вместе с окном.
pub struct TopBar {
    accounts: Vec<TopBarAccount>, // Последний снимок из оболочки.
    quick_focus: Vec<(AccountSession, FocusHandle)>, // Фокус быстрых кнопок закреплён за входом.
    trigger_focus: FocusHandle,   // Сюда возвращается фокус после закрытия меню.
    menu_focus: FocusHandle,      // Одна точка клавиатурного ввода для всего меню.
    menu_scroll: ScrollHandle,    // Удерживает активную строку в видимой части меню.
    highlight: Option<AccountSession>, // Подсвеченная строка. None означает закрытое меню.
}

impl EventEmitter<TopBarSelection> for TopBar {}

impl TopBar {
    /// Создаёт закрытое меню и независимые точки фокуса.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            accounts: Vec::new(),
            quick_focus: Vec::new(),
            trigger_focus: cx.focus_handle(),
            menu_focus: cx.focus_handle(),
            menu_scroll: ScrollHandle::new(),
            highlight: None,
        }
    }

    /// Ограничивает обработку клавиш открытым списком, не меняя обход остальных полей окна.
    pub fn bind_keys(app: &mut App) {
        let context = Some("WorkspaceAccountMenu");
        app.bind_keys([
            KeyBinding::new("up", Previous, context),
            KeyBinding::new("down", Next, context),
            KeyBinding::new("home", First, context),
            KeyBinding::new("end", Last, context),
            KeyBinding::new("escape", Close, context),
            KeyBinding::new("tab", KeepFocus, context),
            KeyBinding::new("shift-tab", KeepFocus, context),
        ]);
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
        let tooltip = account.label.clone();
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
        .tooltip(move |_, app| app.new(|_| AccountTooltip(palette, tooltip.clone())).into())
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
        let tooltip: SharedString = format!("{} · {}", account.label, account.detail).into();
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
            .tooltip(move |_, app| app.new(|_| AccountTooltip(palette, tooltip.clone())).into())
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
        div()
            .id("workspace-account-menu")
            .debug_selector(|| "workspace-account-menu".to_owned())
            .w(px(320.).min((viewport.width - px(8.)).max(px(0.))))
            .max_h(px(320.).min((viewport.height - px(HEIGHT + 4.)).max(px(32.))))
            .p_1()
            .flex()
            .flex_col()
            .rounded(px(12.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .shadow_md()
            .occlude()
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

    /// Держит клавиатурный маршрут в обычном дереве окна, пока меню нарисовано поверх него.
    fn menu_keys(&self, bar: Stateful<Div>, cx: &Context<Self>) -> Stateful<Div> {
        bar.track_focus(&self.menu_focus)
            .key_context("WorkspaceAccountMenu")
            .on_action(cx.listener(|view, _: &Previous, _, cx| {
                if let Some(index) = view.highlight_index() {
                    view.highlight_at(index.saturating_sub(1), cx);
                }
            }))
            .on_action(cx.listener(|view, _: &Next, _, cx| {
                if let Some(index) = view.highlight_index() {
                    view.highlight_at((index + 1).min(view.accounts.len() - 1), cx);
                }
            }))
            .on_action(cx.listener(|view, _: &First, _, cx| view.highlight_at(0, cx)))
            .on_action(cx.listener(|view, _: &Last, _, cx| {
                view.highlight_at(view.accounts.len().saturating_sub(1), cx);
            }))
            .on_key_up(cx.listener(|view, event: &gpui::KeyUpEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space")
                    && !event.keystroke.modifiers.modified()
                    && let Some(session) = view.highlight.clone()
                {
                    // Подтверждаем на отпускании, как кнопка-стрелка. Одна клавиша —
                    // одно действие, даже когда фокус возвращается на стрелку.
                    cx.stop_propagation();
                    view.choose(&session, window, cx);
                }
            }))
            .on_action(cx.listener(|view, _: &Close, window, cx| view.dismiss_menu(window, cx)))
            .on_action(|_: &KeepFocus, _, cx| cx.stop_propagation())
    }
}

impl Render for TopBar {
    /// Ставит «Чаты» в геометрический центр окна и оставляет меню доступным при сужении.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(ThemeMode::from(window.appearance()));
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
            .rounded_b(px(8.))
            .border_b_1()
            .border_color(palette.border)
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
                                    app.new(|_| AccountTooltip(palette, "Выбрать аккаунт".into()))
                                        .into()
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
                    .rounded(px(6.))
                    .border_1()
                    .border_color(palette.border)
                    .bg(palette.card_selected)
                    .child(Icon::Chat.element(px(chat.min(24.)), palette.text))
                    .tooltip(move |_, app| {
                        app.new(|_| AccountTooltip(palette, "Чаты".into())).into()
                    }),
            )
            .children(
                self.accounts
                    .iter()
                    .find(|account| account.selected)
                    .map(|account| active_identity(account, px(side), palette)),
            )
            .when(self.highlight.is_some(), |bar| {
                // Слой перекрывает прокрутку и другие действия окна, пока меню открыто.
                let backdrop = div()
                    .id("workspace-account-menu-backdrop")
                    .w(viewport.width)
                    .h(viewport.height)
                    .occlude();
                self.menu_keys(bar, cx)
                    .child(
                        deferred(anchored().position(point(px(0.), px(0.))).child(backdrop))
                            .with_priority(1),
                    )
                    .child(
                        deferred(
                            anchored()
                                .position(point(
                                    px(trigger_x),
                                    px((HEIGHT + TRIGGER_SIZE) / 2. + 6.),
                                ))
                                .child(self.menu(palette, viewport, cx)),
                        )
                        .with_priority(2),
                    )
            })
    }
}

/// Показывает подпись активного аккаунта, даже если он скрыт среди быстрых кнопок.
fn active_identity(account: &TopBarAccount, side: gpui::Pixels, palette: Palette) -> Div {
    let initials: String = account.label.chars().next().into_iter().collect();
    let mut avatar = Avatar::from_initials(initials);
    avatar.size = AvatarSize::Small;
    div()
        .w(side)
        .pr(px(END_INSET))
        .pl_2()
        .min_w_0()
        .flex_none()
        .flex()
        .items_center()
        .justify_end()
        .gap_2()
        .overflow_hidden()
        .child(avatar.render(palette))
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
        .rounded(px(6.))
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

/// Подсказка с именем входа или подписью иконки раздела.
struct AccountTooltip(Palette, SharedString); // Палитра окна и текст подсказки.

impl Render for AccountTooltip {
    /// Показывает подпись на общем фоне всплывающей панели.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        label(self.0, TextRole::Caption, self.1.clone())
            .max_w(px(320.))
            .px_2()
            .py_1()
            .rounded(px(6.))
            .border_1()
            .border_color(self.0.border)
            .bg(self.0.surface)
    }
}
