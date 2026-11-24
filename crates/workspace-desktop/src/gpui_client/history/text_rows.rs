//! Переиспользует готовые GPUI-представления обычных текстовых баблов при обновлении E2EE.
//! Размеры остаются у виртуального списка. При новой ширине текст измеряется заново.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::Arc,
};

use gpui::{
    AnyElement, AnyView, App, Context, Entity, ImageSource, Pixels, Render, StyleRefinement,
    TextLayout, Window, canvas, prelude::*,
};
use uuid::Uuid;

use super::{HistoryControls, PeerLabel, Row, RowBodyExtras, message_row};
use crate::gpui_client::ui::Palette;

/// Видимое обычное сообщение с размером последнего кадра.
struct Bubble {
    row: Row,                                       // Состав и текст обычного бабла.
    palette: Palette,                               // Тема и видимость рамок фокуса.
    peer: Option<PeerLabel>,                        // Имя автора и подпись запасного аватара.
    photo: Option<ImageSource>,                     // Снимок аватара собственной группы.
    controls: Rc<HistoryControls>,                  // Действия того же экрана истории.
    dimensions: Rc<Cell<Option<(Pixels, Pixels)>>>, // Ширина списка и измеренная высота бабла.
    layout: Rc<RefCell<Option<TextLayout>>>,        // Текстовая геометрия последнего прохода.
    width: Rc<Cell<Pixels>>,                        // Актуальная ширина списка перед отрисовкой.
    #[cfg(all(test, feature = "gpui-test"))]
    renders: Rc<Cell<usize>>, // Вызовы настоящего Render бабла в тесте.
}

impl Render for Bubble {
    /// Собирает тело только для нового вида, размеров или исходных данных.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        #[cfg(all(test, feature = "gpui-test"))]
        self.renders.set(self.renders.get() + 1);
        let (message, layout) = message_row(
            &self.row,
            self.palette,
            self.peer.as_ref(),
            (&[], &HashMap::new()),
            &self.controls,
            self.photo.clone(),
            RowBodyExtras {
                media: None,
                accessory: None,
                fragments: None,
                app: None,
            },
        );
        *self.layout.borrow_mut() = layout;
        let dimensions = self.dimensions.clone();
        let width = self.width.get();
        message.relative().child(
            canvas(
                |_, _, _| (),
                move |bounds, (), _, _| {
                    dimensions.set(Some((width, bounds.size.height)));
                },
            )
            .absolute()
            .inset_0(),
        )
    }
}

/// Входы обычного бабла, которые могут поменять представление и размер.
pub(super) struct BubbleInput<'a> {
    pub row: &'a Row,                  // Снимок строки рабочего списка.
    pub palette: Palette,              // Тема окна.
    pub peer: Option<&'a PeerLabel>,   // Подпись автора.
    pub photo: Option<ImageSource>,    // Новый аватар меняет представление.
    pub controls: Rc<HistoryControls>, // Действия открытого экрана.
    pub width: Rc<Cell<Pixels>>,       // Ширина после расчёта родительской раскладки.
}

/// Готовый элемент и геометрия текста, которой пользуется внешняя опора списка.
pub(super) struct CachedBubble {
    pub element: AnyElement, // Собрано или переиспользовано средствами GPUI.
    pub layout: Rc<RefCell<Option<TextLayout>>>, // Обновляется перед paint, если требовался Render.
}

/// Держит представления недавно показанных обычных сообщений по стабильным UUID.
#[derive(Default)]
pub(super) struct TextRows {
    entries: HashMap<Uuid, Entity<Bubble>>, // Кеш однотипных баблов без вложений и цитат.
}

impl TextRows {
    /// Отпускает представления, которые больше не относятся к живому окну.
    pub(super) fn retain(&mut self, visible: impl Fn(Uuid) -> bool) {
        self.entries.retain(|&id, _| visible(id));
    }

    /// Даёт GPUI уже измеренный элемент, если условия отрисовки обычного бабла совпадают.
    pub(super) fn bubble(&mut self, input: BubbleInput<'_>, cx: &mut App) -> CachedBubble {
        let same = self.entries.get(&input.row.id).filter(|entity| {
            let view = entity.read(cx);
            view.row == *input.row
                && view.palette == input.palette
                && view.peer.as_ref() == input.peer
                && same_photo(view.photo.as_ref(), input.photo.as_ref())
                && view.controls.locale == input.controls.locale
                && view.controls.reply.is_some() == input.controls.reply.is_some()
                && view.controls.quick_reply == input.controls.quick_reply
                && view.controls.quick_hidden == input.controls.quick_hidden
                && match (&view.controls.reactions, &input.controls.reactions) {
                    (Some(previous), Some(current)) => previous.same_row(current, input.row.id),
                    (None, None) => true,
                    _ => false,
                }
        });
        let entity = if let Some(same) = same {
            let view = same.read(cx);
            view.width.set(input.width.get());
            same.clone()
        } else {
            let view = cx.new(|_| Bubble {
                row: input.row.clone(),
                palette: input.palette,
                peer: input.peer.cloned(),
                photo: input.photo,
                controls: input.controls,
                dimensions: Rc::default(),
                layout: Rc::default(),
                width: input.width.clone(),
                #[cfg(all(test, feature = "gpui-test"))]
                renders: Rc::default(),
            });
            self.entries.insert(input.row.id, view.clone());
            view
        };
        let view = entity.read(cx);
        let size = view.dimensions.get();
        let layout = view.layout.clone();
        let element = match size {
            Some((width, height)) if width == view.width.get() => {
                let style = StyleRefinement::default().w_full().h(height);
                AnyView::from(entity).cached(style).into_any_element()
            }
            _ => entity.into_any_element(),
        };
        CachedBubble { element, layout }
    }

    /// Считывает счётчик фактических вызовов Render для контрольного сообщения.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn renders(&self, id: Uuid, cx: &App) -> Option<usize> {
        self.entries
            .get(&id)
            .map(|view| view.read(cx).renders.get())
    }
}

/// Сравнивает именно изображение, которое отдаёт текущий кэш аватаров.
fn same_photo(a: Option<&ImageSource>, b: Option<&ImageSource>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(ImageSource::Render(a)), Some(ImageSource::Render(b))) => Arc::ptr_eq(a, b),
        _ => false,
    }
}
