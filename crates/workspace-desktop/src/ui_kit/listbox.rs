//! Показывает список вариантов для селекторов. Здесь собраны варианты, переходы по ним,
//! оформление списка и позиционирование всплывающей панели относительно поля.

use iced::advanced::widget::{Id, Operation, Tree, operation, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, overlay, renderer};
use iced::widget::button::{Status, Style};
use iced::widget::text::Wrapping;
use iced::widget::{Space, button, column, container, row, scrollable, text};
use iced::{
    Border, Center, Element, Event, Fill, Length, Pixels, Point, Rectangle, Renderer, Shadow, Size,
    Theme, Vector, mouse, touch,
};

use super::theme::{button_colors, text_secondary};
use super::{CONTROL_RADIUS, CONTROL_TEXT_SIZE, IconName, IconTone, icon};

const POPUP_MARGIN: f32 = 8.0;
const POPUP_GAP: f32 = 6.0;
const POPUP_MAX_HEIGHT: f32 = 320.0;

/// Описывает один вариант в списке.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListboxOption {
    pub value: String,               // Уникальное значение, которое получает экран.
    pub label: String,               // Подпись варианта в списке.
    pub description: Option<String>, // Дополнительная подпись под названием.
    pub disabled: bool,              // Запрещает выбрать этот вариант.
}

/// Возвращает соседний доступный вариант. На краю списка оставляет текущий вариант.
/// Если текущего варианта больше нет, начинает с края по направлению движения.
pub(super) fn listbox_next<'a>(
    options: &'a [ListboxOption],
    active: Option<&str>,
    backwards: bool,
) -> Option<&'a str> {
    let Some(index) = options
        .iter()
        .position(|option| !option.disabled && Some(option.value.as_str()) == active)
    else {
        return listbox_edge(options, backwards);
    };

    let next = if backwards {
        options[..index]
            .iter()
            .rev()
            .find(|option| !option.disabled)
    } else {
        options[index + 1..].iter().find(|option| !option.disabled)
    };
    Some(next.unwrap_or(&options[index]).value.as_str())
}

/// Возвращает первый доступный вариант, либо последний при `last`.
pub(super) fn listbox_edge(options: &[ListboxOption], last: bool) -> Option<&str> {
    let mut enabled = options.iter().filter(|option| !option.disabled);
    let option = if last {
        enabled.next_back()
    } else {
        enabled.next()
    };
    option.map(|option| option.value.as_str())
}

/// Собирает прокручиваемый список. `selected` рисует галочку, а `active` подсвечивает строку.
/// Стрелки и Enter обрабатывает поле, которое открыло список. Здесь строки реагируют на указатель.
pub fn listbox<'a, Message: Clone + 'a>(
    options: &[ListboxOption],
    selected: Option<&str>,
    active: Option<&str>,
    empty: &str,
    on_select: impl Fn(&str) -> Message,
) -> Element<'a, Message> {
    let mut content = column![].width(Fill);
    if options.is_empty() {
        content = content.push(
            container(
                text(empty.to_owned())
                    .size(CONTROL_TEXT_SIZE)
                    .style(|theme: &Theme| text::Style {
                        color: Some(text_secondary(theme)),
                    }),
            )
            .padding([12, 10]),
        );
    }
    for option in options {
        let value = Some(option.value.as_str());
        content = content.push(option_row(
            option,
            selected == value,
            active == value && !option.disabled,
            (!option.disabled).then(|| on_select(&option.value)),
        ));
    }

    let active_index = options
        .iter()
        .position(|option| !option.disabled && Some(option.value.as_str()) == active);
    let scroll = Element::new(ListboxScroll {
        inner: scrollable(content).width(Fill),
        active_index,
    });
    container(scroll)
        .width(Fill)
        .max_height(POPUP_MAX_HEIGHT)
        .padding(4)
        .style(|theme: &Theme| {
            let colors = button_colors(theme);
            container::Style {
                background: Some(colors.elevated.into()),
                border: Border::default()
                    .rounded(12)
                    .width(1)
                    .color(colors.border_subtle),
                shadow: Shadow {
                    color: iced::Color::BLACK.scale_alpha(0.20),
                    offset: Vector::new(0.0, 3.0),
                    blur_radius: 10.0,
                },
                ..container::Style::default()
            }
        })
        .into()
}

/// Сохраняет штатную прокрутку Iced и показывает подсвеченный с клавиатуры вариант.
struct ListboxScroll<'a, Message> {
    inner: scrollable::Scrollable<'a, Message>, // Обычная прокрутка со своим состоянием.
    active_index: Option<usize>, // Номер подсвеченного варианта сверху, начиная с нуля.
}

/// Хранит последний вариант, для которого прокрутка уже проверяла видимость.
#[derive(Default)]
struct ScrollState {
    previous: Option<usize>, // Номер варианта при предыдущей раскладке или `None` без подсветки.
}

impl<Message> Widget<Message, Theme, Renderer> for ListboxScroll<'_, Message> {
    /// Отличает состояние списка от состояния его дочерней прокрутки.
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<ScrollState>()
    }

    /// Начинает без ранее подсвеченного варианта.
    fn state(&self) -> tree::State {
        tree::State::new(ScrollState::default())
    }

    /// Создаёт состояние обычной прокрутки.
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(
            &self.inner as &dyn Widget<Message, Theme, Renderer>,
        )]
    }

    /// Обновляет состояние прокрутки при изменении вариантов.
    fn diff(&self, tree: &mut Tree) {
        tree.children[0].diff(&self.inner as &dyn Widget<Message, Theme, Renderer>);
    }

    /// Берёт размеры у штатной прокрутки.
    fn size(&self) -> Size<Length> {
        self.inner.size()
    }

    /// Прокручивает ровно настолько, чтобы новый подсвеченный вариант попал в видимую область.
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let child = &mut tree.children[0];
        let node = self.inner.layout(child, renderer, limits);
        let state = tree.state.downcast_mut::<ScrollState>();
        let previous = std::mem::replace(&mut state.previous, self.active_index);
        if previous != self.active_index
            && let Some(index) = self.active_index
        {
            let viewport = Layout::new(&node);
            let rows = viewport
                .children()
                .next()
                .expect("у списка есть колонка вариантов");
            if let Some(option) = rows.children().nth(index) {
                self.inner.operate(
                    child,
                    viewport,
                    renderer,
                    &mut RevealOption {
                        bounds: option.bounds(),
                    },
                );
            }
        }
        node
    }

    /// Делегирует события штатной прокрутке.
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

    /// Берёт форму курсора у прокрутки и её строк.
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

    /// Рисует штатную прокрутку с учётом смещения.
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
        self.inner.draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
}

/// Подбирает смещение, при котором активная строка становится видна.
struct RevealOption {
    bounds: Rectangle, // Границы строки в раскладке до прокрутки.
}

impl Operation for RevealOption {
    /// Останавливает обход после внешней прокрутки списка.
    fn traverse(&mut self, _: &mut dyn FnMut(&mut dyn Operation)) {}

    /// Сдвигает список вверх или вниз, если строка вышла за границу.
    fn scrollable(
        &mut self,
        _: Option<&Id>,
        viewport: Rectangle,
        _: Rectangle,
        translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        let top = self.bounds.y - viewport.y;
        let bottom = top + self.bounds.height;
        let target = if top < translation.y {
            top
        } else if bottom > translation.y + viewport.height {
            (bottom - viewport.height).min(top)
        } else {
            translation.y
        };
        if target != translation.y {
            state.scroll_to(operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(target.max(0.0)),
            });
        }
    }
}

/// Рисует строку с подписью и местом для галочки. Недоступная строка ничего не отправляет.
fn option_row<'a, Message: Clone + 'a>(
    option: &ListboxOption,
    selected: bool,
    active: bool,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    let label = text(option.label.lines().next().unwrap_or_default().to_owned())
        .size(CONTROL_TEXT_SIZE)
        .line_height(Pixels(20.0))
        .wrapping(Wrapping::None);
    let mut labels = column![label].spacing(2).width(Fill);
    if let Some(description) = option
        .description
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        labels = labels.push(
            text(description.lines().next().unwrap_or_default().to_owned())
                .size(12)
                .line_height(Pixels(16.0))
                .wrapping(Wrapping::None)
                .style(|theme: &Theme| text::Style {
                    color: Some(text_secondary(theme)),
                }),
        );
    }
    let check: Element<'_, Message> = if selected {
        icon(IconName::Check, 16, IconTone::Accent, None)
    } else {
        container(Space::new()).width(16).height(16).into()
    };
    let content = row![container(labels).width(Fill).clip(true), check]
        .spacing(12)
        .align_y(Center);
    let disabled = option.disabled;
    button(content)
        .width(Fill)
        .padding([8, 10])
        .on_press_maybe(on_press)
        .style(move |theme, status| {
            let colors = button_colors(theme);
            let highlighted =
                !disabled && (active || matches!(status, Status::Hovered | Status::Pressed));
            Style {
                background: highlighted.then(|| colors.card_active.into()),
                text_color: if disabled {
                    colors.text_muted
                } else {
                    theme.palette().text
                },
                border: Border::default().rounded(CONTROL_RADIUS),
                ..Style::default()
            }
        })
        .into()
}

/// Привязывает список к полю в координатах окна. `tree` сохраняет прокрутку и нажатие между кадрами.
pub(super) fn anchored_popup<'a, Message: 'a>(
    anchor: Rectangle,
    popup: Element<'a, Message>,
    tree: &'a mut Tree,
    on_dismiss: impl FnMut() + 'a,
) -> overlay::Element<'a, Message, Theme, Renderer> {
    tree.diff(popup.as_widget());
    overlay::Element::new(Box::new(AnchoredPopup {
        anchor,
        popup,
        tree,
        on_dismiss,
        dismissed: false,
    }))
}

/// Общая всплывающая панель селекторов. Она маршрутизирует мышь и закрывается после выбора.
struct AnchoredPopup<'a, Message, Dismiss> {
    anchor: Rectangle,           // Границы поля в координатах окна.
    popup: Element<'a, Message>, // Список вариантов или подпись пустого списка.
    tree: &'a mut Tree,          // Состояние содержимого между событиями Iced.
    on_dismiss: Dismiss,         // Закрывает панель в состоянии поля.
    dismissed: bool,             // Текущая панель уже закрылась в этом наборе событий.
}

impl<Message, Dismiss: FnMut()> overlay::Overlay<Message, Theme, Renderer>
    for AnchoredPopup<'_, Message, Dismiss>
{
    /// Выбирает место сверху или снизу и ограничивает список свободной высотой окна.
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        let width = self
            .anchor
            .width
            .clamp(0.0, (bounds.width - POPUP_MARGIN * 2.0).max(0.0));
        let below_y = self.anchor.y + self.anchor.height + POPUP_GAP;
        let above_y = self.anchor.y - POPUP_GAP;
        let below = (bounds.height - below_y - POPUP_MARGIN).clamp(0.0, POPUP_MAX_HEIGHT);
        let above = (above_y - POPUP_MARGIN).clamp(0.0, POPUP_MAX_HEIGHT);
        let popup = self.popup.as_widget_mut();
        let limits = |height| layout::Limits::new(Size::new(width, 0.0), Size::new(width, height));
        let mut node = popup.layout(self.tree, renderer, &limits(below.max(above)));
        let place_above = node.size().height > below && above > below;
        if !place_above && node.size().height > below {
            node = popup.layout(self.tree, renderer, &limits(below));
        }

        let x = self.anchor.x.clamp(
            POPUP_MARGIN,
            (bounds.width - width - POPUP_MARGIN).max(POPUP_MARGIN),
        );
        let y = if place_above {
            above_y - node.size().height
        } else {
            below_y
        };
        node.move_to(Point::new(x, y))
    }

    /// Рисует панель поверх остального окна.
    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        if !self.dismissed {
            self.popup.as_widget().draw(
                self.tree,
                renderer,
                theme,
                style,
                layout,
                cursor,
                &layout.bounds(),
            );
        }
    }

    /// Передаёт указатель панели. Нажатие снаружи закрывает её и проходит в основное окно.
    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        if self.dismissed || matches!(event, Event::Keyboard(_)) {
            return;
        }

        let cursor = match event {
            Event::Touch(
                touch::Event::FingerPressed { position, .. }
                | touch::Event::FingerMoved { position, .. }
                | touch::Event::FingerLifted { position, .. }
                | touch::Event::FingerLost { position, .. },
            ) => mouse::Cursor::Available(*position),
            _ => cursor,
        };
        let bounds = layout.bounds();
        let press = match event {
            Event::Mouse(mouse::Event::ButtonPressed(_)) => cursor.position(),
            Event::Touch(touch::Event::FingerPressed { position, .. }) => Some(*position),
            _ => None,
        };
        if press
            .is_some_and(|position| !bounds.contains(position) && !self.anchor.contains(position))
        {
            self.dismiss(shell);
            return;
        }

        let mut messages = Vec::new();
        let mut local = Shell::new(&mut messages);
        self.popup.as_widget_mut().update(
            self.tree, event, layout, cursor, renderer, clipboard, &mut local, &bounds,
        );
        let chosen = !local.is_empty();
        if press.is_some_and(|position| bounds.contains(position)) {
            // Нажатие на пустое место списка не должно менять фокус поля позади.
            local.capture_event();
        }
        shell.merge(local, std::convert::identity);
        if chosen {
            self.dismiss(shell);
        }
    }

    /// Показывает указатель содержимого, а над пустым местом не пропускает его к окну позади.
    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.dismissed || !cursor.is_over(layout.bounds()) {
            return mouse::Interaction::None;
        }
        self.popup
            .as_widget()
            .mouse_interaction(self.tree, layout, cursor, &layout.bounds(), renderer)
            .max(mouse::Interaction::Idle)
    }
}

impl<Message, Dismiss: FnMut()> AnchoredPopup<'_, Message, Dismiss> {
    /// Закрывает панель один раз и запрашивает кадр без неё.
    fn dismiss(&mut self, shell: &mut Shell<'_, Message>) {
        (self.on_dismiss)();
        self.dismissed = true;
        *self.tree = Tree::empty();
        // Iced передаёт оставшиеся события окну. Перераскладка здесь может оборвать
        // передачу набора событий, например отпускание кнопки после нажатия снаружи.
        shell.request_redraw();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use iced::advanced::clipboard;
    use iced::{Font, window};

    use super::*;

    /// Создаёт вариант для проверки списка.
    fn option(value: &str, disabled: bool) -> ListboxOption {
        ListboxOption {
            value: value.to_owned(),
            label: value.to_owned(),
            description: None,
            disabled,
        }
    }

    #[test]
    /// Стрелки пропускают недоступные варианты и останавливаются на краях.
    fn keyboard_navigation_skips_disabled_and_stops_at_ends() {
        let options = [option("a", false), option("b", true), option("c", false)];
        assert_eq!(listbox_next(&options, Some("a"), false), Some("c"));
        assert_eq!(listbox_next(&options, Some("c"), true), Some("a"));
        assert_eq!(listbox_next(&options, Some("c"), false), Some("c"));
        assert_eq!(listbox_next(&options, Some("a"), true), Some("a"));
        assert_eq!(listbox_next(&options, Some("b"), false), Some("a"));
        assert_eq!(listbox_edge(&options, true), Some("c"));
        assert_eq!(listbox_next(&options[1..2], None, false), None);
        assert_eq!(listbox_edge(&[], false), None);
    }

    #[test]
    /// Список сохраняет нажатие между кадрами, выбирает только доступный вариант и закрывается.
    fn clicking_available_option_while_open_publishes_once_and_dismisses() {
        let options = [option("a", true), option("b", false)];
        let renderer = Renderer::new(Font::DEFAULT, Pixels(14.0));
        let mut tree = Tree::empty();
        let mut messages = Vec::new();
        let dismissed = Cell::new(0);
        let anchor = Rectangle::new(Point::new(30.0, 20.0), Size::new(200.0, 32.0));
        let emit = |value: &str| value.to_owned();

        for (point_y, pressed) in [(80.0, true), (80.0, false), (116.0, true), (116.0, false)] {
            let mut overlay = anchored_popup(
                anchor,
                listbox(&options, None, None, "Empty", emit),
                &mut tree,
                || dismissed.set(dismissed.get() + 1),
            );
            let overlay = overlay.as_overlay_mut();
            let node = overlay.layout(&renderer, Size::new(330.0, 300.0));
            let mut shell = Shell::new(&mut messages);
            let cursor = mouse::Cursor::Available(Point::new(45.0, point_y));
            let event = if pressed {
                mouse::Event::ButtonPressed(mouse::Button::Left)
            } else {
                mouse::Event::ButtonReleased(mouse::Button::Left)
            };
            overlay.update(
                &Event::Mouse(event),
                Layout::new(&node),
                cursor,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
            );
            if pressed || point_y == 80.0 {
                assert!(shell.is_empty());
            }
        }
        assert_eq!(messages, ["b"]);
        assert_eq!(dismissed.get(), 1);
    }

    #[test]
    /// Список помещается над полем у нижнего края. Нажатие вне поля закрывает список и проходит дальше.
    fn popup_flips_above_and_passes_through_outside_click() {
        let renderer = Renderer::new(Font::DEFAULT, Pixels(14.0));
        let mut tree = Tree::empty();
        let mut messages = Vec::new();
        let dismissed = Cell::new(false);
        let anchor = Rectangle::new(Point::new(40.0, 180.0), Size::new(120.0, 30.0));
        let mut overlay = anchored_popup(
            anchor,
            listbox(&[option("a", false)], None, None, "Empty", |_| ()),
            &mut tree,
            || dismissed.set(true),
        );
        let overlay = overlay.as_overlay_mut();
        let node = overlay.layout(&renderer, Size::new(220.0, 230.0));
        let layout = Layout::new(&node);
        assert_eq!(layout.bounds().width, anchor.width);
        assert!(layout.bounds().y + layout.bounds().height < anchor.y);
        let mut shell = Shell::new(&mut messages);
        for position in [Point::new(80.0, 190.0), Point::new(10.0, 10.0)] {
            overlay.update(
                &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                layout,
                mouse::Cursor::Available(position),
                &renderer,
                &mut clipboard::Null,
                &mut shell,
            );
            assert!(!shell.is_event_captured());
            assert_eq!(dismissed.get(), position.x == 10.0);
        }
        assert_eq!(shell.redraw_request(), window::RedrawRequest::NextFrame);
        assert!(!shell.is_layout_invalid());

        // Отпускание из того же набора событий остаётся доступно кнопке позади списка.
        overlay.update(
            &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            layout,
            mouse::Cursor::Available(Point::new(10.0, 10.0)),
            &renderer,
            &mut clipboard::Null,
            &mut shell,
        );
        assert!(!shell.is_event_captured());
    }

    #[test]
    /// После перехода в конец списка последняя строка становится видимой и её можно нажать.
    fn keyboard_highlight_scrolls_the_target_into_view() {
        let options: Vec<_> = (0..20)
            .map(|index| option(&index.to_string(), false))
            .collect();
        let renderer = Renderer::new(Font::DEFAULT, Pixels(14.0));
        let mut tree = Tree::empty();
        let mut messages = Vec::new();
        let anchor = Rectangle::new(Point::new(20.0, 20.0), Size::new(200.0, 32.0));
        let window = Size::new(260.0, 160.0);
        let emit = |value: &str| value.to_owned();
        for (active, pressed) in [("0", None), ("19", Some(true)), ("19", Some(false))] {
            let mut overlay = anchored_popup(
                anchor,
                listbox(&options, None, Some(active), "Empty", emit),
                &mut tree,
                || {},
            );
            let overlay = overlay.as_overlay_mut();
            let node = overlay.layout(&renderer, window);
            if let Some(pressed) = pressed {
                let layout = Layout::new(&node);
                let last_row = layout.bounds().y + layout.bounds().height - 10.0;
                let cursor = mouse::Cursor::Available(Point::new(45.0, last_row));
                let event = if pressed {
                    mouse::Event::ButtonPressed(mouse::Button::Left)
                } else {
                    mouse::Event::ButtonReleased(mouse::Button::Left)
                };
                overlay.update(
                    &Event::Mouse(event),
                    layout,
                    cursor,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                );
            }
        }
        assert_eq!(messages, ["19"]);
    }
}
