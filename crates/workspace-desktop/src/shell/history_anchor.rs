//! Снимает координаты строк после разметки Iced и рассчитывает прокрутку после обновления.
//! Сохраняет отступ видимой строки или ближайшего соседа, если строку удалили.

use std::collections::HashMap;

use iced::advanced::widget::{
    Id, Operation,
    operation::{
        Outcome, Scrollable,
        scrollable::{AbsoluteOffset, RelativeOffset},
    },
};
use iced::{Rectangle, Task, Vector};
use uuid::Uuid;

/// Снимок геометрии списка в логических пикселях.
#[derive(Clone, Debug)]
pub struct HistoryLayout {
    scroll: f32,          // Сдвиг содержимого от его начала.
    viewport_height: f32, // Высота видимой области.
    content_height: f32,  // Полная высота содержимого, включая разделители и отступы.
    rows: Vec<Row>,       // Измеренные строки в порядке от верхней границы к нижней.
}

/// Координаты строки, которую можно найти в следующей разметке по UUID.
#[derive(Clone, Debug)]
struct Row {
    id: Uuid,    // Стабильный UUID строки внутри списка.
    offset: f32, // Отступ верха строки от верха видимой области. Может быть отрицательным.
    height: f32, // Высота строки в логических пикселях.
}

impl HistoryLayout {
    /// Возвращает прокрутку, которую Iced применил к видимому списку.
    pub(super) fn scroll(&self) -> f32 {
        self.scroll
    }

    /// Проверяет положение у конца списка с учётом округления Iced.
    pub(super) fn at_end(&self) -> bool {
        self.scroll_limit() - self.scroll <= 1.0
    }

    /// Возвращает прокрутку, при которой прежняя видимая строка сохранит свой отступ.
    pub(super) fn restored_scroll(&self, after: &Self) -> Option<f32> {
        let anchor = self
            .rows
            .iter()
            .position(|row| row.offset + row.height > 0.0 && row.offset < self.viewport_height)?;
        let after_offsets: HashMap<_, _> =
            after.rows.iter().map(|row| (row.id, row.offset)).collect();

        // Если опора исчезла, сравниваем соседей в исходном порядке.
        // При равном расстоянии берём следующую строку, которая была ближе к видимой части.
        let (_, (before, after_offset)) = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let offset = after_offsets.get(&row.id)?;
                Some(((index.abs_diff(anchor), index < anchor), (row, offset)))
            })
            .min_by_key(|(distance, _)| *distance)?;
        let scroll = f64::from(after.scroll) + f64::from(*after_offset) - f64::from(before.offset);
        Some(scroll.clamp(0.0, f64::from(after.scroll_limit())) as f32)
    }

    /// Вычисляет самый дальний допустимый сдвиг для текущей разметки.
    fn scroll_limit(&self) -> f32 {
        (self.content_height - self.viewport_height).max(0.0)
    }
}

/// Считывает геометрию вертикального списка и его строк из одной разметки.
pub(super) fn measure(list: Id, rows: Vec<(Id, Uuid)>) -> Task<Option<HistoryLayout>> {
    iced_runtime::task::widget(MeasureHistory {
        list,
        rows: rows.into_iter().collect(),
        viewport: None,
        content_height: 0.0,
        scroll: 0.0,
        found: Vec::new(),
    })
}

/// Коррекция либо сохраняет отступ опоры, либо следует за последним сообщением.
pub(super) enum ScrollCorrection {
    Offset(f32), // Абсолютная прокрутка к прежнему отступу строки.
    Tail,        // Читатель был в конце истории до обновления.
}

/// В момент выполнения проверяет, что ручной ввод и новая разметка ещё не изменили список.
pub(super) fn correct<T>(list: Id, after: HistoryLayout, correction: ScrollCorrection) -> Task<T> {
    iced_runtime::task::effect(iced_runtime::Action::Widget(Box::new(CorrectHistory {
        list,
        after,
        correction,
    })))
}

/// Применяет уже рассчитанное смещение только к подтверждённой геометрии.
struct CorrectHistory {
    list: Id,                     // ID прокручиваемой области текущего маршрута.
    after: HistoryLayout,         // Фактическая геометрия до выполнения операции.
    correction: ScrollCorrection, // Желаемая прокрутка при совпадении.
}

impl Operation for CorrectHistory {
    /// Передаёт операцию вложенным виджетам.
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }

    /// Сравнивает фактический сдвиг и размеры прямо перед коррекцией.
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        viewport: Rectangle,
        content: Rectangle,
        translation: Vector,
        state: &mut dyn Scrollable,
    ) {
        let same = |expected: f32, actual: f32| (expected - actual).abs() < 0.1;
        if id == Some(&self.list)
            && same(self.after.scroll, translation.y)
            && same(self.after.viewport_height, viewport.height)
            && same(self.after.content_height, content.height)
        {
            match self.correction {
                ScrollCorrection::Offset(y) => state.scroll_to(AbsoluteOffset {
                    x: None,
                    y: Some(y),
                }),
                ScrollCorrection::Tail => state.snap_to(RelativeOffset {
                    x: None,
                    y: Some(1.0),
                }),
            }
        }
    }
}

/// Собирает размеры списка и необработанные координаты нужных контейнеров.
struct MeasureHistory {
    list: Id,                      // ID вертикального списка.
    rows: HashMap<Id, Uuid>,       // Соответствие ID контейнера его UUID.
    viewport: Option<Rectangle>,   // None, если список не найден в текущей разметке.
    content_height: f32,           // Высота содержимого, включая невидимую часть.
    scroll: f32,                   // Сдвиг содержимого, переданный списком Iced.
    found: Vec<(Uuid, Rectangle)>, // Строки в координатах разметки до сдвига списком.
}

impl Operation<Option<HistoryLayout>> for MeasureHistory {
    /// Передаёт операцию вложенным виджетам списка.
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<Option<HistoryLayout>>)) {
        visit(self);
    }

    /// Запоминает размеры контейнеров строк по переданным ID.
    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if let Some(uuid) = id.and_then(|id| self.rows.get(id)) {
            self.found.push((*uuid, bounds));
        }
    }

    /// Читает границы видимой области, высоту содержимого и фактический сдвиг.
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        viewport: Rectangle,
        content: Rectangle,
        translation: Vector,
        _: &mut dyn Scrollable,
    ) {
        if id == Some(&self.list) {
            self.viewport = Some(viewport);
            self.content_height = content.height;
            self.scroll = translation.y;
        }
    }

    /// Переводит координаты строк в экранные отступы от верхнего края списка.
    fn finish(&self) -> Outcome<Option<HistoryLayout>> {
        Outcome::Some(self.viewport.and_then(|viewport| {
            if !viewport.y.is_finite()
                || !viewport.height.is_finite()
                || viewport.height <= 0.0
                || !self.content_height.is_finite()
                || self.content_height < 0.0
                || !self.scroll.is_finite()
                || self.scroll < 0.0
            {
                return None;
            }
            let mut rows = self
                .found
                .iter()
                .filter_map(|(id, bounds)| {
                    let offset = bounds.y - self.scroll - viewport.y;
                    (offset.is_finite() && bounds.height.is_finite() && bounds.height > 0.0)
                        .then_some(Row {
                            id: *id,
                            offset,
                            height: bounds.height,
                        })
                })
                .collect::<Vec<_>>();
            // Высоты меняются независимо, поэтому порядок берём из новой разметки.
            rows.sort_by(|left, right| left.offset.total_cmp(&right.offset));
            Some(HistoryLayout {
                scroll: self.scroll,
                viewport_height: viewport.height,
                content_height: self.content_height,
                rows,
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use iced::advanced::widget::operation::scrollable::{AbsoluteOffset, scroll_to};
    use iced::widget::{column, container, scrollable, space};
    use iced::{Element, Font, Pixels, Size};
    use iced_runtime::{
        Action,
        task::into_stream,
        user_interface::{Cache, UserInterface},
    };

    /// Создаёт отсортированный снимок из экранных координат.
    fn layout(scroll: f32, content_height: f32, rows: &[(u128, f32, f32)]) -> HistoryLayout {
        HistoryLayout {
            scroll,
            viewport_height: 80.0,
            content_height,
            rows: rows
                .iter()
                .map(|&(id, offset, height)| Row {
                    id: Uuid::from_u128(id),
                    offset,
                    height,
                })
                .collect(),
        }
    }

    /// Сохраняет видимую часть строки после роста сообщения выше неё.
    #[test]
    fn keeps_partially_visible_message_after_edit_above() {
        let before = layout(60.0, 240.0, &[(1, -60.0, 45.0), (2, -12.0, 30.0)]);
        let after = layout(60.0, 275.0, &[(1, -60.0, 80.0), (2, 23.0, 30.0)]);
        assert_eq!(before.restored_scroll(&after), Some(95.0));
    }

    /// Сохраняет UUID, даже когда вставка меняет его порядковое место.
    #[test]
    fn positions_original_row_after_insertion() {
        let before = layout(40.0, 180.0, &[(2, -8.0, 25.0), (3, 20.0, 30.0)]);
        let after = layout(
            40.0,
            215.0,
            &[(1, -8.0, 32.0), (2, 27.0, 25.0), (3, 55.0, 30.0)],
        );
        assert_eq!(before.restored_scroll(&after), Some(75.0));
    }

    /// Сначала ищет ближайшего соседа по исходному списку, затем держит его отступ.
    #[test]
    fn falls_back_to_nearest_surviving_row() {
        let before = layout(
            50.0,
            240.0,
            &[(1, -42.0, 28.0), (2, -10.0, 25.0), (3, 18.0, 30.0)],
        );
        let both = layout(50.0, 212.0, &[(1, -42.0, 28.0), (3, -10.0, 30.0)]);
        let predecessor = layout(50.0, 180.0, &[(1, -30.0, 28.0), (4, 10.0, 35.0)]);
        let none = layout(50.0, 180.0, &[(4, 10.0, 35.0)]);
        assert_eq!(before.restored_scroll(&both), Some(22.0));
        assert_eq!(before.restored_scroll(&predecessor), Some(62.0));
        assert_eq!(before.restored_scroll(&none), None);
    }

    /// Ограничивает прокрутку диапазоном нового списка с учётом дробных размеров.
    #[test]
    fn clamps_to_scroll_range() {
        let before = layout(20.0, 240.0, &[(1, 10.0, 20.0)]);
        let above = layout(0.0, 240.0, &[(1, 5.0, 20.0)]);
        let below = layout(5.0, 95.0, &[(1, 50.0, 20.0)]);
        let fits = layout(0.0, 60.0, &[(1, 25.0, 20.0)]);
        let end = layout(15.0, 95.4, &[]);
        assert_eq!(before.restored_scroll(&above), Some(0.0));
        assert_eq!(before.restored_scroll(&below), Some(15.0));
        assert_eq!(before.restored_scroll(&fits), Some(0.0));
        assert!(end.at_end());
        assert!(!before.at_end());
    }

    /// Игнорирует непригодные строки и сохраняет порядок обхода при одинаковой координате.
    #[test]
    fn sorts_equal_offsets_stably_and_skips_non_finite_rows() {
        let mut operation = MeasureHistory {
            list: Id::from("anchor-test-list"),
            rows: HashMap::new(),
            viewport: Some(Rectangle {
                x: 0.0,
                y: 20.0,
                width: 200.0,
                height: 80.0,
            }),
            content_height: 180.0,
            scroll: 30.0,
            found: [(1, 80.0), (2, 60.0), (3, f32::NAN), (4, 80.0)]
                .into_iter()
                .map(|(id, y)| {
                    (
                        Uuid::from_u128(id),
                        Rectangle {
                            x: 0.0,
                            y,
                            width: 200.0,
                            height: 20.0,
                        },
                    )
                })
                .collect(),
        };
        let Outcome::Some(Some(layout)) = operation.finish() else {
            panic!("ожидалась пригодная разметка");
        };
        assert_eq!(
            layout
                .rows
                .iter()
                .map(|row| row.id.as_u128())
                .collect::<Vec<_>>(),
            [2, 1, 4]
        );
        operation.scroll = f32::NAN;
        assert!(matches!(operation.finish(), Outcome::Some(None)));
    }

    /// Измеряет координаты штатного списка со сдвигом в реальной разметке Iced.
    #[tokio::test]
    async fn measures_widget_translation_and_rows_after_layout() {
        let list = Id::from("anchor-test-list");
        let first = Id::from("anchor-test-first");
        let second = Id::from("anchor-test-second");
        let third = Id::from("anchor-test-third");
        let view: Element<'_, ()> = column![
            space().height(17),
            scrollable(column![
                container(space().height(40)).id(first.clone()),
                container(space().height(50)).id(second.clone()),
                container(space().height(90)).id(third.clone()),
            ])
            .id(list.clone())
            .height(100),
        ]
        .into();
        let mut renderer = iced::Renderer::new(Font::DEFAULT, Pixels(16.0));
        let mut ui =
            UserInterface::build(view, Size::new(380.0, 260.0), Cache::new(), &mut renderer);
        ui.operate(
            &renderer,
            &mut scroll_to(
                list.clone(),
                AbsoluteOffset {
                    x: None,
                    y: Some(30.0),
                },
            ),
        );
        let mut actions = into_stream(measure(
            list.clone(),
            vec![
                (third, Uuid::from_u128(3)),
                (first, Uuid::from_u128(1)),
                (second, Uuid::from_u128(2)),
            ],
        ))
        .unwrap();
        let Action::Widget(mut operation) = actions.next().await.unwrap() else {
            panic!("ожидалась операция над виджетами");
        };
        ui.operate(&renderer, operation.as_mut());
        let _ = operation.finish();
        drop(operation);
        let Action::Output(Some(layout)) = actions.next().await.unwrap() else {
            panic!("ожидался снимок разметки");
        };
        assert_eq!(
            (
                layout.scroll(),
                layout.viewport_height,
                layout.content_height
            ),
            (30.0, 100.0, 180.0)
        );
        assert_eq!(
            layout
                .rows
                .iter()
                .map(|row| (row.id.as_u128(), row.offset, row.height))
                .collect::<Vec<_>>(),
            [(1, -30.0, 40.0), (2, 10.0, 50.0), (3, 60.0, 90.0)],
        );

        // Список сдвинулся после замера; уже подготовленная коррекция не должна сработать.
        ui.operate(
            &renderer,
            &mut scroll_to(
                list.clone(),
                AbsoluteOffset {
                    x: None,
                    y: Some(60.0),
                },
            ),
        );
        let mut actions = into_stream(correct::<()>(
            list.clone(),
            layout,
            ScrollCorrection::Offset(10.0),
        ))
        .unwrap();
        let Action::Widget(mut correction) = actions.next().await.unwrap() else {
            panic!("ожидалась коррекция прокрутки");
        };
        ui.operate(&renderer, correction.as_mut());
        let mut measured = MeasureHistory {
            list,
            rows: HashMap::new(),
            viewport: None,
            content_height: 0.0,
            scroll: 0.0,
            found: Vec::new(),
        };
        ui.operate(
            &renderer,
            &mut iced::advanced::widget::operation::black_box(&mut measured),
        );
        assert_eq!(measured.scroll, 60.0);
    }
}
