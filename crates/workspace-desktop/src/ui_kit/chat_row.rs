//! Собирает строку списка чатов с аватаром, превью и метаданными.
//! Сначала описаны данные строки и её вид, затем обрезка длинного текста.

use std::{borrow::Cow, cell::Cell, rc::Rc};

use iced::advanced::{
    Layout, Widget, layout, mouse, renderer,
    text::{self as paragraph, Paragraph, Renderer as _, paragraph::Plain},
    widget::{Operation, Tree, tree},
};
use iced::alignment::{Horizontal, Vertical};
use iced::font::Weight;
use iced::widget::{button, column, container, row, space, text};
use iced::{Border, Color, Element, Fill, Font, Length, Pixels, Rectangle, Renderer, Size, Theme};
use unicode_segmentation::UnicodeSegmentation;

use super::button::keyboard_button;
use super::theme::{button_colors, text_muted, text_secondary};
use super::{BadgeSize, BadgeTone, CONTROL_RADIUS, mention_badge, unread_count_badge};

const LINE_HEIGHT: f32 = 20.0;
const CARD_PADDING: u16 = 10;

/// Данные превью последнего сообщения, подготовленные экраном.
pub struct ChatRowPreview<'a> {
    pub sender: Option<&'a str>, // Имя автора. При `None` показываем только текст.
    pub text: &'a str,           // Краткий текст сообщения.
}

/// Содержимое строки чата и действие, которое получает экран.
pub struct ChatRowOptions<'a, Message> {
    pub avatar: Element<'a, Message>, // Аватар или другая картинка слева, обычно 40 px.
    pub title: &'a str,               // Имя собеседника или название канала.
    pub preview: Option<ChatRowPreview<'a>>, // Превью внизу под названием.
    pub time: Option<&'a str>,        // Подпись времени, уже отформатированная экраном.
    pub unread: Option<(u64, BadgeTone)>, // Число и цвет счётчика. Ноль скрывает бейдж.
    pub mention: bool,                // Показывать ли бейдж упоминания.
    pub selected: bool,               // Относится ли строка к открытому чату.
    pub on_press: Option<Message>,    // Действие при активации. При `None` строка статична.
}

/// Рисует широкую строку с аватаром, двумя строками текста и зоной метаданных справа.
pub fn chat_row<'a, Message: Clone + 'static>(
    options: ChatRowOptions<'a, Message>,
) -> Element<'a, Message> {
    let ChatRowOptions {
        avatar,
        title,
        preview,
        time,
        unread,
        mention,
        selected,
        on_press,
    } = options;
    let labels = column![
        single_line(one_line(title), Fill, 14.0, Weight::Medium, |theme| {
            theme.palette().text
        }),
        preview_line(preview),
    ]
    .width(Fill);
    let mut content = row![avatar, labels].spacing(12).width(Fill);
    let unread = unread.filter(|(count, _)| *count > 0);
    if time.is_some() || unread.is_some() || mention {
        content = content.push(metadata(time, unread, mention));
    }
    let content = container(content).width(Fill).clip(true);
    let Some(on_press) = on_press else {
        return container(content)
            .padding(CARD_PADDING)
            .width(Fill)
            .style(move |theme| container::Style {
                background: Some(card_color(theme, selected, false).into()),
                border: Border::default().rounded(CONTROL_RADIUS),
                ..container::Style::default()
            })
            .into();
    };

    let keyboard_pressed = Rc::new(Cell::new(false));
    let pressed_for_style = Rc::clone(&keyboard_pressed);
    let inner = button(content)
        .padding(CARD_PADDING)
        .width(Fill)
        .on_press(on_press.clone())
        .style(move |theme, status| button::Style {
            background: Some(
                card_color(
                    theme,
                    selected,
                    status == button::Status::Pressed
                        || pressed_for_style.get()
                        || status == button::Status::Hovered,
                )
                .into(),
            ),
            text_color: theme.palette().text,
            border: Border::default().rounded(CONTROL_RADIUS),
            ..button::Style::default()
        });
    keyboard_button(inner, Some(on_press), keyboard_pressed)
}

/// Подбирает фон карточки для обычной, выбранной и подсвеченной строки.
fn card_color(theme: &Theme, selected: bool, highlighted: bool) -> Color {
    let colors = button_colors(theme);
    if selected || highlighted {
        colors.card_active
    } else {
        colors.card
    }
}

/// Размещает счётчики над временем. Правый край остаётся общим у обеих строк.
fn metadata<Message: 'static>(
    time: Option<&str>,
    unread: Option<(u64, BadgeTone)>,
    mention: bool,
) -> Element<'_, Message> {
    let mut badges = row![].height(LINE_HEIGHT).spacing(4);
    if mention {
        badges = badges.push(mention_badge(BadgeSize::Regular));
    }
    if let Some((count, tone)) = unread {
        badges = badges.push(unread_count_badge(count, tone, BadgeSize::Regular));
    }
    let time = text(time.unwrap_or_default())
        .size(12)
        .line_height(Pixels(LINE_HEIGHT))
        .wrapping(paragraph::Wrapping::None)
        .style(|theme| text::Style {
            color: Some(text_muted(theme)),
        });
    column![
        space().width(40).height(0),
        badges,
        container(time).height(LINE_HEIGHT),
    ]
    .align_x(Horizontal::Right)
    .into()
}

/// Рисует имя автора и текст в одной линии, сохраняя высоту пустого превью.
fn preview_line<'a, Message: 'a>(preview: Option<ChatRowPreview<'a>>) -> Element<'a, Message> {
    let Some(preview) = preview else {
        return space().height(LINE_HEIGHT).into();
    };
    let body = one_line(preview.text);
    let sender = preview.sender.map(one_line).filter(|name| !name.is_empty());
    if let Some(sender) = sender {
        let sender = if body.is_empty() {
            sender
        } else {
            Cow::Owned(format!("{sender}:"))
        };
        let sender = single_line(sender, Length::Shrink, 12.0, Weight::Normal, |theme| {
            theme.palette().primary
        });
        if body.is_empty() {
            return sender;
        }
        return row![
            sender,
            single_line(body, Fill, 12.0, Weight::Normal, text_secondary),
        ]
        .spacing(3)
        .width(Fill)
        .into();
    }
    single_line(body, Fill, 12.0, Weight::Normal, text_secondary)
}

/// Убирает переводы строк в заголовке и превью, чтобы текст занимал одну строку.
fn one_line(content: &str) -> Cow<'_, str> {
    let content = content.trim();
    match content.contains(['\n', '\r', '\t', '\u{2028}', '\u{2029}']) {
        true => Cow::Owned(content.split_whitespace().collect::<Vec<_>>().join(" ")),
        false => Cow::Borrowed(content),
    }
}

type MeasuredParagraph = <Renderer as paragraph::Renderer>::Paragraph;

/// Строка текста, которая использует реальную ширину символов для многоточия.
struct SingleLine<'a> {
    content: Cow<'a, str>,      // Полный текст, который получен от экрана.
    width: Length,              // Занять остаток строки или только нужное место.
    size: Pixels,               // Размер шрифта в логических пикселях.
    weight: Weight,             // Начертание заголовка или второстепенного текста.
    color: fn(&Theme) -> Color, // Цвет этой подписи из текущей темы.
}

/// Кэш измерения для уже подготовленного текста и доступной ширины.
#[derive(Default)]
struct LineLayout {
    content: String,                  // Полный текст, для которого выполнен расчёт.
    fitted: Plain<MeasuredParagraph>, // Видимый текст и его измеренный абзац.
}

/// Создаёт строку с заданным стилем и обрезкой по фактической ширине.
fn single_line<'a, Message: 'a>(
    content: Cow<'a, str>,
    width: Length,
    size: f32,
    weight: Weight,
    color: fn(&Theme) -> Color,
) -> Element<'a, Message> {
    Element::new(SingleLine {
        content,
        width,
        size: Pixels(size),
        weight,
        color,
    })
}

impl<Message> Widget<Message, Theme, Renderer> for SingleLine<'_> {
    /// Хранит кэш текста отдельно от дерева стандартных виджетов.
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<LineLayout>()
    }

    /// Начинает с пустого кэша, который заполнится при первой разметке.
    fn state(&self) -> tree::State {
        tree::State::new(LineLayout::default())
    }

    /// Резервирует одну строку, даже если в неё пока нечего поместить.
    fn size(&self) -> Size<Length> {
        Size::new(self.width, Length::Fixed(LINE_HEIGHT))
    }

    /// Измеряет текст заново при смене шрифта, подписи или доступной ширины.
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<LineLayout>();
        let width = limits.max().width.max(0.0);
        let format = paragraph::Text {
            content: (),
            // Высоту строки задаёт виджет. При измерении движок должен видеть все глифы.
            bounds: Size::new(width, f32::INFINITY),
            size: self.size,
            line_height: LINE_HEIGHT.into(),
            font: Font {
                weight: self.weight,
                ..renderer.default_font()
            },
            align_x: paragraph::Alignment::Left,
            align_y: Vertical::Center,
            shaping: paragraph::Shaping::Advanced,
            wrapping: paragraph::Wrapping::None,
        };
        if state.content != self.content
            || state.fitted.raw().compare(format) != paragraph::Difference::None
        {
            state.fitted = fit_line(&self.content, width, format);
            self.content.as_ref().clone_into(&mut state.content);
        }
        layout::Node::new(limits.resolve(
            self.width,
            Length::Fixed(LINE_HEIGHT),
            Size::new(state.fitted.min_width(), LINE_HEIGHT),
        ))
    }

    /// Рисует подготовленный текст в пределах отведённого ему места.
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if let Some(visible) = bounds.intersection(viewport) {
            let state = tree.state.downcast_ref::<LineLayout>();
            text::draw(
                renderer,
                style,
                bounds,
                state.fitted.raw(),
                text::Style {
                    color: Some((self.color)(theme)),
                },
                &visible,
            );
        }
    }

    /// Оставляет полную подпись доступной для обхода виджетов Iced.
    fn operate(
        &mut self,
        _tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.text(None, layout.bounds(), &self.content);
    }
}

/// Измеряет строку в шрифте виджета и добавляет многоточие по границам видимых символов.
fn fit_line(
    content: &str,
    width: f32,
    format: paragraph::Text<(), Font>,
) -> Plain<MeasuredParagraph> {
    let measure = |value| Plain::new(format.with_content(value));
    let full = measure(content.to_owned());
    if full.min_width() <= width {
        return full;
    }
    let mut fitted = measure("…".to_owned());
    if fitted.min_width() > width {
        return measure(String::new());
    }

    // Многоточие измеряем вместе с текстом: в шрифте соседние символы влияют на ширину.
    let cuts: Vec<_> = content
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .collect();
    let (mut first, mut last) = (1, cuts.len());
    while first < last {
        let middle = first + (last - first) / 2;
        let candidate = measure(format!("{}…", content[..cuts[middle]].trim_end()));
        if candidate.min_width() <= width {
            fitted = candidate;
            first = middle + 1;
        } else {
            last = middle;
        }
    }
    fitted
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Собирает подписи со штатных и собственных виджетов, чтобы сравнить их границы.
    #[derive(Default)]
    struct Labels(Vec<(String, Rectangle)>); // Текст и отведённое ему место.

    impl Operation for Labels {
        /// Продолжает обход вложенных виджетов.
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }

        /// Сохраняет подпись с её положением в карточке.
        fn text(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            bounds: Rectangle,
            content: &str,
        ) {
            self.0.push((content.to_owned(), bounds));
        }
    }

    /// Находит видимый текст подготовленной строки по полной подписи.
    fn fitted_text<'a>(tree: &'a Tree, content: &str) -> Option<&'a str> {
        if tree.tag == tree::Tag::of::<LineLayout>() {
            let state = tree.state.downcast_ref::<LineLayout>();
            return (state.content == content).then(|| state.fitted.content());
        }
        tree.children
            .iter()
            .find_map(|child| fitted_text(child, content))
    }

    /// Проверяет сужение и расширение карточки с длинными подписями и счётчиками.
    #[test]
    fn long_chat_text_ellipsizes_without_covering_metadata() {
        let title = "Разработка 👩‍💻 и интеграция мессенджера";
        let body = "Прове\u{301}рили сборку, можно открывать";
        let mut card = chat_row::<()>(ChatRowOptions {
            avatar: space().width(40).height(40).into(),
            title,
            preview: Some(ChatRowPreview {
                sender: Some("Ли"),
                text: body,
            }),
            time: Some("12:48"),
            unread: Some((1254, BadgeTone::Unread)),
            mention: true,
            selected: false,
            on_press: Some(()),
        });
        let renderer = Renderer::new(Font::DEFAULT, Pixels(16.0));
        let mut tree = Tree::new(card.as_widget());
        let node = card.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(225.0, 100.0)),
        );
        assert_eq!(node.size(), Size::new(225.0, 60.0));
        for text in [title, body] {
            let visible = fitted_text(&tree, text).unwrap();
            let prefix = visible
                .strip_suffix('…')
                .unwrap_or_else(|| panic!("Длинная подпись {text:?} видна как {visible:?}"));
            assert!(!prefix.is_empty());
            assert!(text.starts_with(prefix));
            assert!(
                text.grapheme_indices(true)
                    .any(|(offset, _)| offset == prefix.len())
            );
        }
        let mut labels = Labels::default();
        card.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut labels);
        let bounds = |label: &str| labels.0.iter().find(|(value, _)| value == label).unwrap().1;
        for label in ["12:48", "1k+", "@"] {
            let meta = bounds(label);
            let title = bounds(title);
            assert!(meta.x >= title.x + title.width);
            assert!(meta.x + meta.width <= 215.0);
        }

        card.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(620.0, 100.0)),
        );
        assert_eq!(fitted_text(&tree, title), Some(title));
        assert_eq!(fitted_text(&tree, body), Some(body));
    }
}
