//! Создаёт кнопки Workspace на основе Iced.
//! Сначала идут параметры, сборка и цвета кнопок, затем обработка клавиатуры и анимация загрузки.

use std::{cell::Cell, f32::consts::TAU, rc::Rc};

use iced::advanced::renderer::{self, Renderer as _};
use iced::advanced::widget::{Operation, Tree, operation::Focusable, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, overlay};
use iced::keyboard::{self, Key, key::Named};
use iced::time::{Duration, Instant};
use iced::widget::button::{Button, Status, Style};
use iced::widget::{button as iced_button, canvas, container, row, text};
use iced::{
    Background, Border, Color, Element, Event, Fill, Font, Length, Pixels, Radians, Rectangle,
    Renderer, Size, Theme, Vector, font, mouse, window,
};

use super::CONTROL_RADIUS;
use super::theme::button_colors;

const LEADING_ICON_SIZE: u32 = 16;

/// Назначение кнопки, от которого зависят её цвета.
#[derive(Debug, Clone, Copy, Default)]
pub enum ButtonVariant {
    #[default]
    Primary, // Основное действие.
    Neutral, // Обычное действие без акцента.
    Danger,  // Действие с серьёзными последствиями, например удаление.
}

/// Способ оформления фона и границы кнопки.
#[derive(Debug, Clone, Copy, Default)]
pub enum ButtonAppearance {
    #[default]
    Filled, // Кнопка с заливкой.
    Outline, // Кнопка с контуром.
    Ghost,   // Кнопка с прозрачным фоном в обычном состоянии.
}

/// Размер кнопки, который определяет отступы и размер текста.
#[derive(Debug, Clone, Copy, Default)]
pub enum ButtonSize {
    Small, // Компактный размер.
    #[default]
    Medium, // Стандартный размер.
    Large, // Увеличенный размер.
}

/// Настройки кнопки для случаев, когда стандартных параметров мало.
pub struct ButtonOptions<'a, Message> {
    pub variant: ButtonVariant, // Назначение, по которому выбираются цвета.
    pub appearance: ButtonAppearance, // Оформление заливки и границы.
    pub size: ButtonSize,       // Размер текста и отступов.
    pub loading: bool,          // Признак загрузки. Показывает индикатор и отключает нажатие.
    pub full_width: bool,       // Признак, что кнопка должна занять доступную ширину.
    pub leading_icon: Option<Element<'a, Message>>, // Иконка в области 16 × 16. При загрузке её место занимает индикатор.
}

impl<Message> Default for ButtonOptions<'_, Message> {
    /// Возвращает стандартные параметры кнопки без иконки и загрузки.
    fn default() -> Self {
        Self {
            variant: ButtonVariant::default(),
            appearance: ButtonAppearance::default(),
            size: ButtonSize::default(),
            loading: false,
            full_width: false,
            leading_icon: None,
        }
    }
}

/// Создаёт кнопку с выбранным назначением и стандартным оформлением.
pub fn button<'a, Message: Clone + 'a>(
    label: &'a str,
    variant: ButtonVariant,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    button_with(
        label,
        ButtonOptions {
            variant,
            ..ButtonOptions::default()
        },
        on_press,
    )
}

/// Создаёт кнопку с заданными параметрами. `None` вместо сообщения отключает нажатие.
pub fn button_with<'a, Message: Clone + 'a>(
    label: &'a str,
    options: ButtonOptions<'a, Message>,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    let ButtonOptions {
        variant,
        appearance,
        size,
        loading,
        full_width,
        leading_icon,
    } = options;

    let (height, horizontal_padding, text_size, line_height) = match size {
        ButtonSize::Small => (32, 12, 12, 16.0),
        ButtonSize::Medium => (36, 16, 14, 20.0),
        ButtonSize::Large => (40, 16, 14, 20.0),
    };
    let weight = match (appearance, variant) {
        (ButtonAppearance::Filled, ButtonVariant::Primary | ButtonVariant::Danger) => {
            font::Weight::Semibold
        }
        _ => font::Weight::Medium,
    };
    let label = text(label)
        .size(text_size)
        .line_height(Pixels(line_height))
        .font(Font {
            weight,
            ..Font::DEFAULT
        });

    let icon = if loading {
        Some(
            canvas(Spinner {
                variant,
                appearance,
            })
            .width(LEADING_ICON_SIZE)
            .height(LEADING_ICON_SIZE)
            .into(),
        )
    } else {
        leading_icon
    };
    let mut content = row![].spacing(6).align_y(iced::Center);
    if let Some(icon) = icon {
        content = content.push(
            container(icon)
                .center_x(LEADING_ICON_SIZE)
                .center_y(LEADING_ICON_SIZE)
                .clip(true),
        );
    }
    let width = if full_width { Fill } else { Length::Shrink };
    let content = container(content.push(label))
        .center_y(Fill)
        .center_x(width);

    let on_press = on_press.filter(|_| !loading);
    let keyboard_pressed = Rc::new(Cell::new(false));
    let pressed_for_style = Rc::clone(&keyboard_pressed);
    let inner = iced_button(content)
        .height(height)
        .width(width)
        .padding([0, horizontal_padding])
        .on_press_maybe(on_press.clone())
        .style(move |theme, status| {
            let status = if pressed_for_style.get() && status != Status::Disabled {
                Status::Pressed
            } else {
                status
            };
            button_style(theme, variant, appearance, status)
        });

    Element::new(KeyboardButton {
        inner,
        on_press,
        keyboard_pressed,
    })
}

/// Подбирает фон, текст и границу для темы, оформления и состояния кнопки.
fn button_style(
    theme: &Theme,
    variant: ButtonVariant,
    appearance: ButtonAppearance,
    status: Status,
) -> Style {
    use ButtonAppearance::{Filled, Ghost, Outline};
    use ButtonVariant::{Danger, Neutral, Primary};

    let palette = theme.palette();
    let colors = button_colors(theme);
    let hover = status == Status::Hovered;
    let pressed = status == Status::Pressed;
    let active_alpha = if pressed {
        0.8
    } else if hover {
        0.9
    } else {
        1.0
    };
    let tint_alpha = if pressed {
        0.15
    } else if hover {
        0.1
    } else {
        0.0
    };

    let (background, text_color, border_color) = match (appearance, variant) {
        (Filled, Primary) => (
            palette.primary.scale_alpha(active_alpha),
            colors.on_accent,
            Color::TRANSPARENT,
        ),
        (Filled, Danger) => (
            palette.danger.scale_alpha(active_alpha),
            Color::WHITE,
            Color::TRANSPARENT,
        ),
        (Filled, Neutral) => {
            let (background, border) = if pressed {
                (colors.card, colors.accent_soft)
            } else if hover {
                (colors.elevated, colors.border_subtle)
            } else {
                (colors.card_active, Color::TRANSPARENT)
            };
            (background, palette.text, border)
        }
        (Outline, Neutral) => {
            let background = if pressed {
                colors.card
            } else if hover {
                colors.elevated
            } else {
                Color::TRANSPARENT
            };
            (background, palette.text, colors.border_subtle)
        }
        (Ghost, Neutral) => {
            let background = if pressed || hover {
                colors.elevated.scale_alpha(0.6)
            } else {
                Color::TRANSPARENT
            };
            let text = if pressed || hover {
                palette.text
            } else {
                colors.text_muted
            };
            (background, text, Color::TRANSPARENT)
        }
        (Outline, Primary) => (
            palette.primary.scale_alpha(tint_alpha),
            palette.primary,
            palette.primary,
        ),
        (Outline, Danger) => (
            palette.danger.scale_alpha(tint_alpha),
            palette.danger,
            palette.danger.scale_alpha(0.3),
        ),
        (Ghost, Primary) => (
            palette.primary.scale_alpha(tint_alpha),
            palette.primary,
            Color::TRANSPARENT,
        ),
        (Ghost, Danger) => (
            palette.danger.scale_alpha(tint_alpha),
            palette.danger,
            Color::TRANSPARENT,
        ),
    };

    let alpha = if status == Status::Disabled { 0.5 } else { 1.0 };
    Style {
        background: (background.a > 0.0).then(|| Background::Color(background.scale_alpha(alpha))),
        text_color: text_color.scale_alpha(alpha),
        border: Border {
            radius: CONTROL_RADIUS.into(),
            width: if border_color.a > 0.0 { 1.0 } else { 0.0 },
            color: border_color.scale_alpha(alpha),
        },
        ..Style::default()
    }
}

/// Обёртка над кнопкой Iced, которая добавляет фокус и нажатие с клавиатуры.
struct KeyboardButton<'a, Message> {
    inner: Button<'a, Message>, // Кнопка Iced, которая рисует содержимое и обрабатывает указатель.
    on_press: Option<Message>,  // Сообщение при нажатии. `None` означает, что кнопка отключена.
    keyboard_pressed: Rc<Cell<bool>>, // Общий признак нажатия клавишей, который читает стиль кнопки.
}

/// Состояние клавиатуры, которое Iced хранит между обновлениями виджета.
#[derive(Default)]
struct FocusState {
    enabled: bool,    // Признак, что кнопку можно нажать.
    focused: bool,    // Признак, что кнопка сейчас принимает ввод с клавиатуры.
    space_held: bool, // Признак, что пробел удерживается до отпускания или потери фокуса.
}

impl Focusable for FocusState {
    /// Сообщает Iced, находится ли кнопка в фокусе.
    fn is_focused(&self) -> bool {
        self.focused
    }

    /// Переводит клавиатурный фокус на кнопку.
    fn focus(&mut self) {
        self.focused = true;
    }

    /// Снимает фокус и сбрасывает удержание пробела.
    fn unfocus(&mut self) {
        self.focused = false;
        self.space_held = false;
    }
}

impl<'a, Message: Clone + 'a> Widget<Message, Theme, Renderer> for KeyboardButton<'a, Message> {
    /// Указывает Iced, какой тип состояния хранить для этой обёртки.
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<FocusState>()
    }

    /// Создаёт начальное состояние клавиатуры по доступности кнопки.
    fn state(&self) -> tree::State {
        tree::State::new(FocusState {
            enabled: self.on_press.is_some(),
            ..FocusState::default()
        })
    }

    /// Создаёт отдельную ветку состояния для вложенной кнопки.
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(
            &self.inner as &dyn Widget<Message, Theme, Renderer>,
        )]
    }

    /// Обновляет дерево Iced и сбрасывает фокус, когда меняется доступность кнопки.
    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<FocusState>();
        let enabled = self.on_press.is_some();
        if enabled != state.enabled {
            state.enabled = enabled;
            state.unfocus();
            tree.children = self.children();
        } else {
            tree.children[0].diff(&self.inner as &dyn Widget<Message, Theme, Renderer>);
        }
    }

    /// Возвращает размеры вложенной кнопки.
    fn size(&self) -> Size<Length> {
        self.inner.size()
    }

    /// Просит вложенную кнопку рассчитать расположение содержимого.
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.inner.layout(&mut tree.children[0], renderer, limits)
    }

    /// Регистрирует доступную кнопку для перехода фокуса и передаёт операцию внутрь.
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        if self.on_press.is_some() {
            operation.focusable(
                None,
                layout.bounds(),
                tree.state.downcast_mut::<FocusState>(),
            );
        }
        self.inner
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    /// Обрабатывает Enter и пробел, а остальные события передаёт вложенной кнопке.
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<FocusState>();
        if state.focused {
            match event {
                Event::Mouse(mouse::Event::ButtonPressed(_))
                | Event::Touch(iced::touch::Event::FingerPressed { .. }) => {
                    state.unfocus();
                    shell.request_redraw();
                }
                Event::Window(window::Event::Unfocused) if state.space_held => {
                    state.space_held = false;
                    shell.request_redraw();
                }
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: Key::Named(key @ (Named::Enter | Named::Space)),
                    modifiers,
                    repeat,
                    ..
                }) if !modifiers.command()
                    && !modifiers.control()
                    && !modifiers.alt()
                    && !modifiers.logo() =>
                {
                    if !repeat && let Some(message) = &self.on_press {
                        if *key == Named::Enter {
                            shell.publish(message.clone());
                        } else {
                            state.space_held = true;
                            shell.request_redraw();
                        }
                    }
                    shell.capture_event();
                    return;
                }
                Event::Keyboard(keyboard::Event::KeyReleased {
                    key: Key::Named(Named::Space),
                    ..
                }) if state.space_held => {
                    state.space_held = false;
                    if let Some(message) = &self.on_press {
                        shell.publish(message.clone());
                    }
                    shell.request_redraw();
                    shell.capture_event();
                    return;
                }
                _ => {}
            }
        }
        self.inner.update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    /// Рисует кнопку и рамку клавиатурного фокуса.
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
        let state = tree.state.downcast_ref::<FocusState>();
        self.keyboard_pressed.set(state.space_held);
        self.inner.draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        if state.focused {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds().expand(4),
                    border: Border {
                        radius: (CONTROL_RADIUS + 4.0).into(),
                        width: 2.0,
                        color: theme.palette().primary,
                    },
                    ..renderer::Quad::default()
                },
                Color::TRANSPARENT,
            );
        }
    }

    /// Спрашивает у вложенной кнопки, какой указатель мыши показать.
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.inner
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    /// Передаёт вложенной кнопке запрос на отрисовку поверх других виджетов.
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.inner.overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

/// Индикатор, который крутится на месте иконки во время загрузки.
struct Spinner {
    variant: ButtonVariant,       // Назначение кнопки для выбора цвета индикатора.
    appearance: ButtonAppearance, // Оформление кнопки для выбора цвета индикатора.
}

/// Параметры текущего кадра анимации.
#[derive(Default)]
struct SpinnerState {
    start: Option<Instant>, // Момент первого кадра. `None` до первого запроса перерисовки.
    angle: f32,             // Текущий угол поворота в радианах.
}

impl<Message> canvas::Program<Message> for Spinner {
    /// Состояние анимации, которое Iced хранит для индикатора.
    type State = SpinnerState;

    /// Вычисляет угол по времени и просит Iced нарисовать следующий кадр.
    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        let Event::Window(window::Event::RedrawRequested(now)) = event else {
            return None;
        };
        let start = state.start.get_or_insert(*now);
        state.angle = (now.duration_since(*start).as_secs_f32() % 0.8) / 0.8 * TAU;
        Some(canvas::Action::request_redraw_at(
            *now + Duration::from_millis(16),
        ))
    }

    /// Рисует дугу индикатора цветом текста отключённой кнопки.
    fn draw(
        &self,
        state: &Self::State,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let arc = canvas::Path::new(|path| {
            path.arc(canvas::path::Arc {
                center: frame.center(),
                radius: 6.0,
                start_angle: Radians(state.angle),
                end_angle: Radians(state.angle + 0.75 * TAU),
            });
        });
        let color = button_style(theme, self.variant, self.appearance, Status::Disabled).text_color;
        frame.stroke(
            &arc,
            canvas::Stroke::default()
                .with_color(color)
                .with_width(2.0)
                .with_line_cap(canvas::LineCap::Round),
        );
        vec![frame.into_geometry()]
    }
}
