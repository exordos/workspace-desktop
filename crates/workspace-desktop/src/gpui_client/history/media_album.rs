//! Раскладывает фото и видео на всю ширину пузыря и скругляет только внешние углы.

use gpui::{Corners, Div, Pixels, Stateful, div, prelude::*, px};
use workspace_core::messenger::references::MediaReference;

use crate::gpui_client::ui::MESSAGE_BUBBLE_MAX_WIDTH;

const GAP: Pixels = px(2.);
const MAX_SINGLE_HEIGHT: Pixels = px(430.);
const MAX_GROUP_HEIGHT: Pixels = px(520.);

/// Пропорции кадра и скругления конкретной плитки.
#[derive(Clone, Copy)]
pub(super) struct Tile {
    pub(super) ratio: f32,               // Ширина к высоте после кадрирования.
    pub(super) corners: Corners<Pixels>, // Скругления со стороны внешних краёв пузыря.
}

impl Tile {
    /// Применяет одни и те же внешние углы к фону и готовому кадру.
    pub(super) fn round<E: Styled>(self, element: E) -> E {
        element
            .rounded_tl(self.corners.top_left)
            .rounded_tr(self.corners.top_right)
            .rounded_bl(self.corners.bottom_left)
            .rounded_br(self.corners.bottom_right)
    }
}

/// Позиция ссылки в исходном сообщении, нужная для действий по плитке.
type Item = (usize, f32);

/// Делит большую подборку на группы и рисует их в исходном порядке.
pub(super) fn render(
    items: &[Item],
    corners: Corners<Pixels>,
    mut card: impl FnMut(usize, Tile) -> Stateful<Div>,
) -> Div {
    let mut album = div().w_full().flex().flex_col().gap(GAP);
    let groups = items.chunks(4);
    let last = groups.len().saturating_sub(1);
    for (index, group) in groups.enumerate() {
        let corners = Corners {
            top_left: if index == 0 { corners.top_left } else { px(0.) },
            top_right: if index == 0 {
                corners.top_right
            } else {
                px(0.)
            },
            bottom_left: if index == last {
                corners.bottom_left
            } else {
                px(0.)
            },
            bottom_right: if index == last {
                corners.bottom_right
            } else {
                px(0.)
            },
        };
        let group = match group {
            [first, rest @ ..] if rest.len() >= 2 && first.1 < 1. => {
                portrait_group(*first, rest, corners, &mut card)
            }
            [a, b, c, d] => {
                let upper = pair_ratio(a.1) + pair_ratio(b.1);
                let lower = pair_ratio(c.1) + pair_ratio(d.1);
                let scale = row_height_scale(upper, lower);
                div()
                    .flex()
                    .flex_col()
                    .gap(GAP)
                    .child(row(&[*a, *b], upper / scale, top(corners), &mut card))
                    .child(row(&[*c, *d], lower / scale, bottom(corners), &mut card))
            }
            [a, b, c] => {
                let upper = a.1.clamp(1.7, 3.2);
                let lower = pair_ratio(b.1) + pair_ratio(c.1);
                let scale = row_height_scale(upper, lower);
                div()
                    .flex()
                    .flex_col()
                    .gap(GAP)
                    .child(single(*a, upper / scale, top(corners), &mut card))
                    .child(row(&[*b, *c], lower / scale, bottom(corners), &mut card))
            }
            [a, b] => row(
                &[*a, *b],
                (pair_ratio(a.1) + pair_ratio(b.1)).max(min_ratio(MAX_GROUP_HEIGHT)),
                corners,
                &mut card,
            ),
            [a] => single(
                *a,
                a.1.clamp(min_ratio(MAX_SINGLE_HEIGHT), 3.2),
                corners,
                &mut card,
            ),
            _ => continue,
        };
        album = album.child(group);
    }
    album
}

/// Оставляет портрету место слева, остальные кадры складывает справа.
fn portrait_group(
    first: Item,
    rest: &[Item],
    corners: Corners<Pixels>,
    card: &mut impl FnMut(usize, Tile) -> Stateful<Div>,
) -> Div {
    let outer_ratio = (first.1 / 0.6).clamp(min_ratio(MAX_GROUP_HEIGHT), 1.5);
    let left = Tile {
        ratio: outer_ratio * 0.6,
        corners: Corners {
            top_left: corners.top_left,
            bottom_left: corners.bottom_left,
            ..Corners::default()
        },
    };
    let right_ratio = outer_ratio * 0.4 * rest.len() as f32;
    let right = weighted(div().h_full().flex().flex_col().gap(GAP), 2.).children(
        rest.iter().enumerate().map(|(index, (position, _))| {
            let corners = Corners {
                top_right: if index == 0 {
                    corners.top_right
                } else {
                    px(0.)
                },
                bottom_right: if index + 1 == rest.len() {
                    corners.bottom_right
                } else {
                    px(0.)
                },
                ..Corners::default()
            };
            card(
                *position,
                Tile {
                    ratio: right_ratio,
                    corners,
                },
            )
            .w_full()
            .flex_1()
            .min_h_0()
        }),
    );
    sized(
        outer_ratio,
        div()
            .absolute()
            .inset_0()
            .flex()
            .gap(GAP)
            .child(weighted(card(first.0, left).h_full(), 3.))
            .child(right),
    )
}

/// Выравнивает два кадра по высоте и отдаёт более широкому больше места.
fn row(
    items: &[Item; 2],
    outer_ratio: f32,
    corners: Corners<Pixels>,
    card: &mut impl FnMut(usize, Tile) -> Stateful<Div>,
) -> Div {
    let [(left, a), (right, b)] = *items;
    let a_ratio = pair_ratio(a);
    let b_ratio = pair_ratio(b);
    let scale = outer_ratio / (a_ratio + b_ratio);
    let a = card(
        left,
        Tile {
            ratio: a_ratio * scale,
            corners: Corners {
                top_left: corners.top_left,
                bottom_left: corners.bottom_left,
                ..Corners::default()
            },
        },
    );
    let b = card(
        right,
        Tile {
            ratio: b_ratio * scale,
            corners: Corners {
                top_right: corners.top_right,
                bottom_right: corners.bottom_right,
                ..Corners::default()
            },
        },
    );
    sized(
        outer_ratio,
        div()
            .absolute()
            .inset_0()
            .flex()
            .gap(GAP)
            .child(weighted(a.h_full(), a_ratio))
            .child(weighted(b.h_full(), b_ratio)),
    )
}

/// Ограничивает разницу ширины соседних плиток при одной высоте.
fn pair_ratio(ratio: f32) -> f32 {
    ratio.clamp(0.65, 1.6)
}

/// Даёт минимальные пропорции, при которых полный пузырь укладывается в предел высоты.
fn min_ratio(max_height: Pixels) -> f32 {
    f32::from(MESSAGE_BUBBLE_MAX_WIDTH) / f32::from(max_height)
}

/// Пропорционально уменьшает высоту двух рядов, сохраняя ширину пузыря.
fn row_height_scale(upper: f32, lower: f32) -> f32 {
    let width = f32::from(MESSAGE_BUBBLE_MAX_WIDTH);
    (f32::from(MAX_GROUP_HEIGHT - GAP) / (width / upper + width / lower)).min(1.)
}

/// Растягивает одиночный кадр от края до края.
fn single(
    (position, _): Item,
    ratio: f32,
    corners: Corners<Pixels>,
    card: &mut impl FnMut(usize, Tile) -> Stateful<Div>,
) -> Div {
    sized(
        ratio,
        card(position, Tile { ratio, corners }).absolute().inset_0(),
    )
}

/// Резервирует высоту от доступной ширины до загрузки байтов.
fn sized(ratio: f32, child: impl IntoElement) -> Div {
    let mut frame = div().w_full().relative();
    frame.style().aspect_ratio = Some(ratio);
    frame.child(child)
}

/// Делит оставшуюся после зазоров ширину по весу плитки.
fn weighted<E: Styled>(mut element: E, weight: f32) -> E {
    element = element.flex_basis(px(0.)).min_w_0();
    element.style().flex_grow = Some(weight);
    element
}

/// Читает пропорции из URL. Старые ссылки получают стабильное место в альбоме.
pub(super) fn reference_ratio(reference: &MediaReference) -> f32 {
    reference
        .metadata
        .width
        .zip(reference.metadata.height)
        .map_or(1.5, |(width, height)| width as f32 / height as f32)
}

/// Сохраняет скругления верхнего ряда.
fn top(corners: Corners<Pixels>) -> Corners<Pixels> {
    Corners {
        bottom_left: px(0.),
        bottom_right: px(0.),
        ..corners
    }
}

/// Сохраняет скругления нижнего ряда.
fn bottom(corners: Corners<Pixels>) -> Corners<Pixels> {
    Corners {
        top_left: px(0.),
        top_right: px(0.),
        ..corners
    }
}
