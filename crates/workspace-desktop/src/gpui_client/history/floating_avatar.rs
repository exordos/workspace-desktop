//! Рисует аватар видимой группы у нижней границы истории или её последнего сообщения.

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Edges, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Position, Style, Window, point,
    prelude::*, px, size,
};
use uuid::Uuid;

use super::BOTTOM_PADDING;
use crate::gpui_client::ui::{Avatar, Palette};

/// Аватар одной строки: видим только у конца группы или у нижней границы окна.
pub(super) struct FloatingAvatar {
    pub id: Uuid,         // UUID строки для отладочного селектора.
    pub avatar: Avatar,   // Фото или инициалы автора.
    pub palette: Palette, // Цвета текущей темы.
    pub ends_group: bool, // Строка завершает группу автора.
}

impl IntoElement for FloatingAvatar {
    type Element = Self;

    /// Передаёт элемент GPUI для вычисления позиции в текущем кадре.
    fn into_element(self) -> Self {
        self
    }
}

impl Element for FloatingAvatar {
    type RequestLayoutState = ();
    type PrepaintState = Option<AnyElement>;

    /// Состояние аватара принадлежит строке и не требует отдельного идентификатора.
    fn id(&self) -> Option<ElementId> {
        None
    }

    /// Отдельное положение в исходном коде для инспектора не требуется.
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    /// Занимает границы строки, не влияя на её размер.
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (
            window.request_layout(
                Style {
                    position: Position::Absolute,
                    inset: Edges::all(px(0.).into()),
                    ..Style::default()
                },
                None,
                cx,
            ),
            (),
        )
    }

    /// Прижимает аватар к низу видимой части группы, пока её конец ниже окна.
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // Список уже держит своё состояние во время prepaint; его маска содержит те же границы.
        let viewport = window.content_mask().bounds;
        let bottom = viewport.bottom() - BOTTOM_PADDING;
        if viewport.size.height <= px(0.)
            || bounds.bottom() <= viewport.top()
            || bounds.top() >= bottom
            || (!self.ends_group && bounds.bottom() < bottom)
        {
            return None;
        }
        let id = self.id;
        let avatar = Avatar {
            initials: self.avatar.initials.clone(),
            image: self.avatar.image.clone(),
            size: self.avatar.size,
            presence: self.avatar.presence,
        };
        let mut avatar = avatar
            .render(self.palette)
            .debug_selector(move || format!("history-avatar-{id}"))
            .into_any_element();
        avatar.prepaint_as_root(
            point(
                bounds.left() + px(12.),
                bounds.bottom().min(bottom) - px(40.),
            ),
            size(
                AvailableSpace::Definite(px(40.)),
                AvailableSpace::Definite(px(40.)),
            ),
            window,
            cx,
        );
        Some(avatar)
    }

    /// Рисует подготовленный аватар внутри обрезки списка.
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        avatar: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(avatar) = avatar {
            avatar.paint(window, cx);
        }
    }
}
