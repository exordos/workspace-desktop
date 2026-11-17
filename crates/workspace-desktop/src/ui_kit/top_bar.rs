//! Собирает шапку приложения с переключателем аккаунтов и разделом «Чаты».
//! В начале описана раскладка и оформление, ниже — работа меню мышью и клавиатурой.

use std::{cell::Cell, rc::Rc, sync::LazyLock};

use iced::advanced::renderer::{self, Renderer as _};
use iced::advanced::widget::{Id, Operation, Tree, operation::Focusable, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, overlay};
use iced::keyboard::{self, Key, key::Named};
use iced::widget::button::{Status, Style};
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, responsive, row, space, stack, svg, text, tooltip};
use iced::{
    Border, Center, Element, Event, Fill, Length, Rectangle, Renderer, Size, Theme, Vector, font,
    mouse, touch, window,
};

use super::button::keyboard_button_with_id;
use super::chat_row::{one_line, single_line};
use super::listbox::{anchored_modal_popup, popup_list};
use super::theme::{button_colors, text_secondary, unread_badge_color};
use super::{BadgeSize, BadgeTone, IconName, IconTone, icon, unread_count_badge};

#[cfg(test)]
mod tests;

/// Позволяет Iced найти кнопку меню, в том числе при проверке фокуса.
pub const TOP_BAR_ACCOUNT_MENU_ID: &str = "top-bar-account-menu";
/// Дополняется идентификатором аккаунта, чтобы найти его строку в меню.
pub const TOP_BAR_ACCOUNT_ROW_ID_PREFIX: &str = "top-bar-account-";
/// Указывает на строку выбранного аккаунта в открытом меню.
pub const TOP_BAR_SELECTED_ACCOUNT_ID: &str = "top-bar-selected-account";
/// Дополняется идентификатором аккаунта, чтобы найти точку непрочитанного на быстрой кнопке.
pub const TOP_BAR_UNREAD_DOT_ID_PREFIX: &str = "top-bar-unread-dot-";

const HEIGHT: f32 = 64.0;
const SECTION_SIZE: f32 = 48.0;
const QUICK_SIZE: f32 = 40.0;
const MENU_TRIGGER_SIZE: f32 = 32.0;
const ACCOUNT_SPACING: f32 = 6.0;
const OVERFLOW_WIDTH: f32 = 34.0;
const MENU_WIDTH: f32 = 320.0;
const LOGO: &[u8] = include_bytes!("../../assets/organization-fallback.svg");
static LOGO_HANDLE: LazyLock<svg::Handle> = LazyLock::new(|| svg::Handle::from_memory(LOGO));

/// Данные одного пункта переключателя, которые передаёт окно приложения.
pub struct TopBarAccount<'a, Message> {
    pub id: &'a str, // Стабильный ключ, по которому меню удерживает подсвеченную строку.
    pub generation: u64, // Номер входа, на который действует кнопка этого аккаунта.
    pub label: &'a str, // Основная подпись аккаунта.
    pub detail: String, // Вторая строка, например сервер и ID проекта.
    pub selected: bool, // Сейчас открыт контекст этого аккаунта.
    pub unread: Option<u32>, // Подтверждённое число непрочитанных или `None` до загрузки.
    pub on_select: Message, // Событие выбора, которому окно сопоставляет аккаунт.
}

impl<Message> TopBarAccount<'_, Message> {
    /// Возвращает ключ конкретного входа, чтобы не переносить нажатие на другой вход.
    fn interaction_key(&self) -> (String, u64) {
        (self.id.to_owned(), self.generation)
    }
}

/// Рисует панель на ширину окна и оставляет место для всех аккаунтов в меню.
pub fn top_bar<'a, Message: Clone + 'static>(
    accounts: Vec<TopBarAccount<'a, Message>>,
) -> Element<'a, Message> {
    let accounts: Rc<[TopBarAccount<'a, Message>]> = accounts.into();
    responsive(move |size| {
        // Равные области по краям удерживают разделы по центру всего окна.
        let section_width = SECTION_SIZE.min((size.width - 2.0 * MENU_TRIGGER_SIZE).max(0.0));
        let side_width = (size.width - section_width) / 2.0;
        let left_padding = (side_width - MENU_TRIGGER_SIZE).clamp(0.0, 24.0);
        let available = side_width - left_padding;
        let visible_count = quick_count(accounts.len(), available);
        let hidden_count = accounts.len() - visible_count;
        let mut quick = row![].spacing(ACCOUNT_SPACING).align_y(Center);
        for account in &accounts[..visible_count] {
            quick = quick.push(quick_account(account));
        }
        let uses_overflow =
            hidden_count > 0 && available >= MENU_TRIGGER_SIZE + ACCOUNT_SPACING + OVERFLOW_WIDTH;
        if uses_overflow {
            quick = quick.push(overflow_count(hidden_count));
        }
        let account_controls = if accounts.is_empty() {
            row![]
        } else {
            row![
                quick,
                Element::new(AccountMenu {
                    accounts: Rc::clone(&accounts),
                }),
            ]
            .spacing(if visible_count > 0 || uses_overflow {
                ACCOUNT_SPACING
            } else {
                0.0
            })
            .align_y(Center)
        };

        let sides = row![
            container(account_controls)
                .width(side_width)
                .padding(iced::Padding::default().left(left_padding)),
            chat_section(section_width),
            space().width(side_width),
        ]
        .height(HEIGHT)
        .align_y(Center);
        container(sides)
            .width(Fill)
            .style(|theme: &Theme| {
                let colors = button_colors(theme);
                container::Style {
                    background: Some(colors.elevated.into()),
                    border: Border::default()
                        .rounded(iced::border::radius(0).bottom(8))
                        .width(1)
                        .color(colors.border_subtle),
                    ..container::Style::default()
                }
            })
            .into()
    })
    .height(HEIGHT)
    .into()
}

/// Считает, сколько быстрых кнопок поместится рядом с меню и числом скрытых аккаунтов.
fn quick_count(total: usize, width: f32) -> usize {
    (1..=total.min(3))
        .rev()
        .find(|&count| {
            let overflow = if count < total {
                OVERFLOW_WIDTH + ACCOUNT_SPACING
            } else {
                0.0
            };
            count as f32 * (QUICK_SIZE + ACCOUNT_SPACING) + MENU_TRIGGER_SIZE + overflow <= width
        })
        .unwrap_or(0)
}

/// Рисует одновременно доступный и выбранный раздел с подсказкой по наведению.
fn chat_section<Message: 'static>(width: f32) -> Element<'static, Message> {
    let glyph = icon(IconName::ChatBubble, 32, IconTone::Accent, None);
    let tile = container(glyph)
        .id("top-bar-chats")
        .center_x(width)
        .center_y(SECTION_SIZE)
        .style(|theme: &Theme| {
            let colors = button_colors(theme);
            container::Style {
                background: Some(colors.card_active.into()),
                border: Border::default()
                    .rounded(12)
                    .width(1)
                    .color(colors.border_subtle),
                ..container::Style::default()
            }
        });
    tooltip(tile, tooltip_text("Чаты"), tooltip::Position::Bottom).into()
}

/// Рисует быструю кнопку с рамкой активного аккаунта и точкой непрочитанного.
fn quick_account<'a, Message: Clone + 'a>(
    account: &TopBarAccount<'a, Message>,
) -> Element<'a, Message> {
    let logo = container(account_logo(account.selected)).center(QUICK_SIZE);
    let mut layers = stack![logo];
    if account.unread.is_some_and(|count| count > 0) {
        let dot = container(space())
            .id(format!("{TOP_BAR_UNREAD_DOT_ID_PREFIX}{}", account.id))
            .width(10)
            .height(10)
            .style(|theme: &Theme| container::Style {
                background: Some(unread_badge_color(theme).into()),
                border: Border::default()
                    .rounded(5)
                    .width(1)
                    .color(button_colors(theme).elevated),
                ..container::Style::default()
            });
        layers = layers.push(container(dot).width(QUICK_SIZE).align_right(QUICK_SIZE));
    }

    let keyboard_pressed = Rc::new(Cell::new(false));
    let pressed = Rc::clone(&keyboard_pressed);
    let selected = account.selected;
    let message = account.on_select.clone();
    let inner = button(layers)
        .padding(0)
        .on_press(message.clone())
        .style(move |theme, status| account_button_style(theme, status, selected, pressed.get()));
    tooltip(
        keyboard_button_with_id(
            inner,
            Some(message),
            keyboard_pressed,
            Some(Id::from(format!(
                "top-bar-quick-account:{}:{}",
                account.generation, account.id
            ))),
        ),
        tooltip_text(account.label),
        tooltip::Position::Bottom,
    )
    .into()
}

/// Даёт быстрым кнопкам и строкам одинаковые цвета активного и наведённого состояния.
fn account_button_style(theme: &Theme, status: Status, selected: bool, active: bool) -> Style {
    let colors = button_colors(theme);
    Style {
        background: (selected || active || matches!(status, Status::Hovered | Status::Pressed))
            .then_some(colors.card_active.into()),
        text_color: theme.palette().text,
        border: Border::default()
            .rounded(8)
            .width(if active { 1 } else { 0 })
            .color(theme.palette().primary),
        ..Style::default()
    }
}

/// Рисует штатный логотип организации в рамке, выделяя текущий аккаунт.
fn account_logo<'a, Message: 'a>(selected: bool) -> Element<'a, Message> {
    container(svg(LOGO_HANDLE.clone()).width(36).height(36))
        .center(36)
        .style(move |theme: &Theme| {
            let colors = button_colors(theme);
            container::Style {
                background: Some(theme.palette().background.into()),
                border: Border::default()
                    .rounded(6)
                    .width(if selected { 2 } else { 0 })
                    .color(colors.border_subtle),
                ..container::Style::default()
            }
        })
        .into()
}

/// Показывает, сколько аккаунтов доступно только в меню.
fn overflow_count<'a, Message: 'a>(hidden_count: usize) -> Element<'a, Message> {
    container(
        text(format!("+{hidden_count}"))
            .size(12)
            .wrapping(Wrapping::None),
    )
    .center_x(OVERFLOW_WIDTH)
    .height(20)
    .align_y(Center)
    .clip(true)
    .style(|theme: &Theme| container::Style {
        background: Some(button_colors(theme).card.into()),
        border: Border::default().rounded(5),
        text_color: Some(text_secondary(theme)),
        ..container::Style::default()
    })
    .into()
}

/// Оформляет короткое пояснение, которое появляется при наведении на кнопку.
fn tooltip_text<'a, Message: 'a>(label: &'a str) -> Element<'a, Message> {
    container(text(label).size(12))
        .max_width(260)
        .padding([4, 8])
        .style(|theme: &Theme| {
            let colors = button_colors(theme);
            container::Style {
                background: Some(colors.elevated.into()),
                border: Border::default()
                    .rounded(6)
                    .width(1)
                    .color(colors.border_subtle),
                ..container::Style::default()
            }
        })
        .into()
}

/// Собирает строку всплывающего меню из логотипа со счётчиком и двух подписей.
fn account_menu_row<'a, Message: Clone + 'static>(
    account: &'a TopBarAccount<'_, Message>,
    active: bool,
) -> Element<'a, Message> {
    let logo = container(account_logo(account.selected)).center(40);
    let mut layers = stack![logo];
    if let Some(count) = account.unread.filter(|count| *count > 0) {
        layers = layers.push(
            container(unread_count_badge(
                u64::from(count),
                BadgeTone::Unread,
                BadgeSize::Small,
            ))
            .align_right(40),
        );
    }
    let title = single_line(
        one_line(account.label),
        Fill,
        14.0,
        font::Weight::Medium,
        |theme| theme.palette().text,
    );
    let detail = single_line(
        one_line(&account.detail),
        Fill,
        12.0,
        font::Weight::Normal,
        text_secondary,
    );
    let labels = column![title, detail].width(Fill);
    let content = row![layers, labels].align_y(Center).spacing(8).width(Fill);
    let selected = account.selected;
    let item = button(content)
        .width(Fill)
        .padding(4)
        .on_press(account.on_select.clone())
        .style(move |theme, status| account_button_style(theme, status, selected, active));
    let mut record = container(item).width(Fill);
    if selected {
        record = record.id(TOP_BAR_SELECTED_ACCOUNT_ID);
    }
    container(record)
        .id(format!("{TOP_BAR_ACCOUNT_ROW_ID_PREFIX}{}", account.id))
        .into()
}

/// Кнопка со стрелкой и список доступных для выбора аккаунтов.
struct AccountMenu<'a, Message> {
    accounts: Rc<[TopBarAccount<'a, Message>]>, // Один снимок для кнопки и всплывающих строк.
}

/// Локальное состояние меню между событиями Iced.
struct MenuState {
    focused: bool,                 // Стрелка принимает ввод с клавиатуры.
    pressed: bool,                 // Нажатие указателя началось на стрелке.
    open: bool,                    // Меню раскрыто в окне.
    active: Option<(String, u64)>, // Аккаунт и номер входа, на которые действует Enter.
    keys: Vec<(String, u64)>,      // Ключи строк в порядке последней отрисовки меню.
    popup: Tree,                   // Состояние строк и прокрутки раскрытого меню.
}

impl MenuState {
    /// Закрывает список и сбрасывает подсветку к следующему открытию.
    fn close(&mut self) {
        self.open = false;
        self.active = None;
        self.popup = Tree::empty();
    }
}

impl Focusable for MenuState {
    /// Возвращает признак фокуса триггера.
    fn is_focused(&self) -> bool {
        self.focused
    }

    /// Передаёт фокус кнопке раскрытия.
    fn focus(&mut self) {
        self.focused = true;
    }

    /// При уходе с кнопки снимает фокус и закрывает список.
    fn unfocus(&mut self) {
        self.focused = false;
        self.pressed = false;
        self.close();
    }
}

impl<Message: Clone> AccountMenu<'_, Message> {
    /// Возвращает индекс подсвеченного аккаунта в текущем порядке.
    fn active_index(&self, state: &MenuState) -> Option<usize> {
        let (id, generation) = state.active.as_ref()?;
        self.accounts
            .iter()
            .position(|account| account.id == id && account.generation == *generation)
    }

    /// Открывает список и подсвечивает выбранный аккаунт или первый доступный.
    fn show(&self, state: &mut MenuState) {
        state.open = true;
        state.active = self
            .accounts
            .iter()
            .find(|account| account.selected)
            .or_else(|| self.accounts.first())
            .map(TopBarAccount::interaction_key);
    }

    /// Обрабатывает клавиши открытого списка. Возвращает `false` для событий другого виджета.
    fn handle_key(
        &self,
        state: &mut MenuState,
        key: Named,
        repeat: bool,
        shell: &mut Shell<'_, Message>,
    ) -> bool {
        match key {
            Named::ArrowUp | Named::ArrowDown => {
                if state.open {
                    let last = self.accounts.len() - 1;
                    let next = match (self.active_index(state), key) {
                        (Some(i), Named::ArrowUp) => i.saturating_sub(1),
                        (Some(i), _) => (i + 1).min(last),
                        (None, Named::ArrowUp) => last,
                        (None, _) => 0,
                    };
                    state.active = Some(self.accounts[next].interaction_key());
                } else {
                    self.show(state);
                }
            }
            Named::Home | Named::End if state.open => {
                let index = if key == Named::Home {
                    0
                } else {
                    self.accounts.len() - 1
                };
                state.active = Some(self.accounts[index].interaction_key());
            }
            Named::Enter | Named::Space => {
                if !repeat {
                    if !state.open {
                        self.show(state);
                    } else if let Some(index) = self.active_index(state) {
                        shell.publish(self.accounts[index].on_select.clone());
                        state.close();
                    }
                }
            }
            Named::Escape if state.open => state.close(),
            _ => return false,
        }
        true
    }
}

impl<Message: Clone + 'static> Widget<Message, Theme, Renderer> for AccountMenu<'_, Message> {
    /// Отличает состояние кнопки меню от состояния соседних виджетов.
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<MenuState>()
    }

    /// Начинает с закрытого списка и без фокуса.
    fn state(&self) -> tree::State {
        tree::State::new(MenuState {
            focused: false,
            pressed: false,
            open: false,
            active: None,
            keys: self
                .accounts
                .iter()
                .map(TopBarAccount::interaction_key)
                .collect(),
            popup: Tree::empty(),
        })
    }

    /// Сбрасывает начатое нажатие, когда порядок или состав строк изменился.
    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<MenuState>();
        let keys = self
            .accounts
            .iter()
            .map(|account| (account.id, account.generation));
        if keys.ne(state
            .keys
            .iter()
            .map(|(id, generation)| (id.as_str(), *generation)))
        {
            state.keys = self
                .accounts
                .iter()
                .map(TopBarAccount::interaction_key)
                .collect();
            // Iced хранит нажатие в позиции строки. В эту позицию мог попасть другой вход.
            state.popup = Tree::empty();
        }
        if state.open && self.active_index(state).is_none() {
            state.active = None;
        }
    }

    /// Задаёт размер кнопки раскрытия в логических пикселях.
    fn size(&self) -> Size<Length> {
        Size::new(
            Length::Fixed(MENU_TRIGGER_SIZE),
            Length::Fixed(MENU_TRIGGER_SIZE),
        )
    }

    /// Размещает кнопку в доступном месте, сохраняя область для клика.
    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(MENU_TRIGGER_SIZE, MENU_TRIGGER_SIZE, Size::ZERO))
    }

    /// Добавляет кнопку раскрытия в порядок обхода Tab.
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let id = Id::new(TOP_BAR_ACCOUNT_MENU_ID);
        operation.focusable(
            Some(&id),
            layout.bounds(),
            tree.state.downcast_mut::<MenuState>(),
        );
    }

    /// Открывает список указателем и обрабатывает клавиши, пока фокус на кнопке.
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<MenuState>();
        let position = match event {
            Event::Touch(
                touch::Event::FingerPressed { position, .. }
                | touch::Event::FingerLifted { position, .. },
            ) => Some(*position),
            _ => cursor.position(),
        };
        let inside = position.is_some_and(|position| {
            layout.bounds().contains(position) && viewport.contains(position)
        });
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerPressed { .. }) => {
                if inside {
                    state.pressed = true;
                    state.focus();
                    shell.capture_event();
                } else {
                    state.unfocus();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerLifted { .. })
                if state.pressed =>
            {
                state.pressed = false;
                if inside {
                    if state.open {
                        state.close();
                    } else {
                        self.show(state);
                    }
                    shell.capture_event();
                    shell.invalidate_layout();
                }
            }
            Event::Touch(touch::Event::FingerLost { .. })
            | Event::Window(window::Event::Unfocused) => state.unfocus(),
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {}
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: Key::Named(key),
                modifiers,
                repeat,
                ..
            }) if state.focused
                && !modifiers.control()
                && !modifiers.alt()
                && !modifiers.logo() =>
            {
                if *key == Named::Tab && state.open {
                    shell.capture_event();
                    return;
                } else if self.handle_key(state, *key, *repeat, shell) {
                    shell.capture_event();
                    shell.invalidate_layout();
                } else {
                    return;
                }
            }
            _ => return,
        }
        shell.request_redraw();
    }

    /// Рисует стрелку и выделяет кнопку при вводе с клавиатуры, наведении или раскрытии.
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
        let bounds = layout.bounds();
        let state = tree.state.downcast_ref::<MenuState>();
        let colors = button_colors(theme);
        let hovered = cursor.is_over(bounds) && cursor.is_over(*viewport);
        let background = if hovered || state.open || state.pressed {
            colors.card_active
        } else {
            iced::Color::TRANSPARENT
        };
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: Border::default()
                    .rounded(8)
                    .width(if state.focused { 2 } else { 0 })
                    .color(theme.palette().primary),
                ..renderer::Quad::default()
            },
            background,
        );
        let mut glyph = icon::<Message>(
            if state.open {
                IconName::ChevronUp
            } else {
                IconName::ChevronDown
            },
            14,
            IconTone::Muted,
            None,
        );
        let mut glyph_tree = Tree::new(&glyph);
        let glyph_layout =
            glyph
                .as_widget_mut()
                .layout(&mut glyph_tree, renderer, &layout::Limits::NONE);
        let offset = Vector::new(
            bounds.x + (bounds.width - 14.0) / 2.0,
            bounds.y + (bounds.height - 14.0) / 2.0,
        );
        glyph.as_widget().draw(
            &glyph_tree,
            renderer,
            theme,
            style,
            Layout::with_offset(offset, &glyph_layout),
            cursor,
            viewport,
        );
    }

    /// Показывает указатель над кнопкой меню.
    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) && cursor.is_over(*viewport) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::None
        }
    }

    /// Привязывает список к кнопке и закрывает его после выбора или клика снаружи.
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        _renderer: &Renderer,
        _viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        let state = tree.state.downcast_mut::<MenuState>();
        if !state.open {
            return None;
        }
        let active = self.active_index(state);
        let mut items = column![].width(Fill);
        for (index, account) in self.accounts.iter().enumerate() {
            items = items.push(account_menu_row(account, active == Some(index)));
        }
        let panel = popup_list(items, active);
        Some(anchored_modal_popup(
            layout.bounds() + translation,
            MENU_WIDTH,
            panel,
            &mut state.popup,
            || {
                state.open = false;
                state.active = None;
            },
        ))
    }
}
