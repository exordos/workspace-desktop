//! Моделирует историю чата с разной высотой строк, измеряет окно после отрисовки и проверяет позицию чтения.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    time::Duration,
};

use gpui::{
    Bounds, Context, ListAlignment, ListOffset, ListState, Pixels, Window, canvas, div, list,
    prelude::*, px, rgb,
};
use uuid::Uuid;

const FIRST_SERIAL: i64 = 101;
const INITIAL_ROWS: i64 = 600;
const OLDER_ROWS: i64 = 50;
const GROWTH_DELAY: Duration = Duration::from_millis(250);
const IMAGE_HEIGHT: f32 = 112.;

/// UUID одного из сообщений в середине начальной истории.
pub const JUMP_TARGET: Uuid = Uuid::from_u128(380);

/// Переходы, которые можно вызвать из окна с историей и из общего сценария.
#[derive(Clone, Copy)]
pub enum HistoryAction {
    Prepend,    // Добавить страницу из 50 старых сообщений.
    Jump(Uuid), // Показать сообщение с переданным UUID.
    GrowAbove,  // Начать загрузку картинки в строке над текущей опорой.
    Append,     // Получить одно новое сообщение.
    ToBottom,   // Вернуться к последнему сообщению.
}

/// Положение строки относительно верхней границы списка после отрисовки.
#[derive(Clone, Copy, Debug)]
pub struct RowGeometry {
    pub uuid: Uuid,           // UUID сообщения, которое действительно появилось в окне.
    pub y: f32,               // Координата верхней границы в логических пикселях.
    pub height: f32,          // Высота строки в логических пикселях.
    pub viewport_height: f32, // Высота области прокрутки в логических пикселях.
}

/// Синтетическое сообщение с устойчивой идентичностью при вставке страниц.
#[derive(Clone)]
struct Row {
    serial: i64,        // Порядковый номер для демонстрационных данных.
    uuid: Uuid,         // Идентификатор, который не зависит от индекса в списке.
    image_loaded: bool, // Нужно ли отрисовать появившуюся картинку.
}

impl Row {
    /// Создаёт сообщение с повторяемым идентификатором для заданного номера.
    fn new(serial: i64) -> Self {
        Self {
            serial,
            uuid: Uuid::from_u128(serial as u128),
            image_loaded: false,
        }
    }
}

/// Положение опорной строки и смещение до верхней границы видимой области.
struct ReadingAnchor {
    uuid: Uuid,    // Сообщение в начале видимой области.
    inset: Pixels, // Сколько логических пикселей строки скрыто сверху.
}

/// Наблюдения за построением и рисованием строк одного кадра.
#[derive(Default)]
struct Frame {
    built: HashSet<Uuid>, // Какие строки GPUI запросил, в том числе для переразметки рядом с окном.
    painted: HashMap<Uuid, Bounds<Pixels>>, // Какие строки дошли до отрисовки и где они оказались.
}

/// История владеет порядком сообщений, индексом по UUID и положением списка GPUI.
pub struct HistoryProbe {
    rows: Rc<Vec<Row>>,          // Снимок для отложенного построения строк в GPUI.
    index: HashMap<Uuid, usize>, // Текущая позиция каждого UUID в снимке.
    list: ListState,             // Виртуальный список и его логическая позиция.
    frame: Rc<RefCell<Frame>>,   // Последний построенный кадр для проверки геометрии.
}

impl HistoryProbe {
    /// Создаёт историю из 600 сообщений и показывает её нижний край.
    pub fn new() -> Self {
        let rows: Vec<_> = (FIRST_SERIAL..FIRST_SERIAL + INITIAL_ROWS)
            .map(Row::new)
            .collect();
        let index = Self::build_index(&rows);
        Self {
            list: ListState::new(rows.len(), ListAlignment::Bottom, px(120.)),
            rows: Rc::new(rows),
            index,
            frame: Rc::default(),
        }
    }

    /// Возвращает число сообщений в текущем снимке.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Возвращает геометрию сообщения, которое было нарисовано в последнем кадре.
    pub fn geometry(&self, uuid: Uuid) -> Option<RowGeometry> {
        let viewport = self.list.viewport_bounds();
        self.frame
            .borrow()
            .painted
            .get(&uuid)
            .map(|row| RowGeometry {
                uuid,
                y: (row.top() - viewport.top()).into(),
                height: row.size.height.into(),
                viewport_height: viewport.size.height.into(),
            })
    }

    /// Показывает геометрию опоры, пока пользователь читает середину истории.
    pub fn anchor(&self) -> Option<RowGeometry> {
        self.reading_anchor()
            .and_then(|anchor| self.geometry(anchor.uuid))
    }

    /// Выполняет команду истории и запрашивает новый кадр при изменении снимка.
    pub fn apply(&mut self, action: HistoryAction, cx: &mut Context<Self>) {
        match action {
            HistoryAction::Prepend => self.prepend(),
            HistoryAction::Jump(uuid) => {
                if let Some(&item_ix) = self.index.get(&uuid) {
                    self.list.scroll_to(ListOffset {
                        item_ix,
                        offset_in_item: px(0.),
                    });
                }
            }
            HistoryAction::GrowAbove => {
                self.schedule_growth(cx);
                return;
            }
            HistoryAction::Append => self.append(),
            HistoryAction::ToBottom => self.list.scroll_to(ListOffset {
                item_ix: self.rows.len(),
                offset_in_item: px(0.),
            }),
        }
        cx.notify();
    }

    /// Сопоставляет идентификаторы с индексами после изменения порядка истории.
    fn build_index(rows: &[Row]) -> HashMap<Uuid, usize> {
        rows.iter()
            .enumerate()
            .map(|(index, row)| (row.uuid, index))
            .collect()
    }

    /// Запоминает UUID и смещение верхней строки. У нижнего края опора не нужна.
    fn reading_anchor(&self) -> Option<ReadingAnchor> {
        let top = self.list.logical_scroll_top();
        self.rows.get(top.item_ix).map(|row| ReadingAnchor {
            uuid: row.uuid,
            inset: top.offset_in_item,
        })
    }

    /// Возвращает опору на прежнюю высоту по UUID в новом порядке строк.
    fn restore_anchor(&self, anchor: Option<ReadingAnchor>) {
        if let Some(anchor) = anchor {
            self.list.scroll_to(ListOffset {
                item_ix: self.index[&anchor.uuid],
                offset_in_item: anchor.inset,
            });
        }
    }

    /// Вставляет старую страницу и оставляет текущую строку на её высоте в окне.
    fn prepend(&mut self) {
        let anchor = self.reading_anchor();
        let first = self.rows[0].serial;
        Rc::make_mut(&mut self.rows).splice(..0, (first - OLDER_ROWS..first).map(Row::new));
        self.index = Self::build_index(&self.rows);
        self.list.splice(0..0, OLDER_ROWS as usize);
        self.restore_anchor(anchor);
    }

    /// Добавляет свежую строку. Нижняя привязка GPUI следует за ней при прокрутке до конца.
    fn append(&mut self) {
        let end = self.rows.len();
        let row = Row::new(self.rows[end - 1].serial + 1);
        self.index.insert(row.uuid, end);
        Rc::make_mut(&mut self.rows).push(row);
        self.list.splice(end..end, 1);
    }

    /// Назначает загрузку строки прямо перед опорой с воспроизводимой задержкой.
    fn schedule_growth(&self, cx: &mut Context<Self>) {
        let top = self.list.logical_scroll_top().item_ix;
        if top >= self.rows.len() {
            return;
        }
        let Some(above) = top.checked_sub(1) else {
            return;
        };
        let uuid = self.rows[above].uuid;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(GROWTH_DELAY).await;
            let _ = this.update(cx, |history, cx| {
                history.finish_growth(uuid);
                cx.notify();
            });
        })
        .detach();
    }

    /// Увеличивает высоту той же строки, даже если до загрузки пришла старая страница.
    fn finish_growth(&mut self, uuid: Uuid) {
        let anchor = self.reading_anchor();
        let at = self.index[&uuid];
        let row = &mut Rc::make_mut(&mut self.rows)[at];
        if !row.image_loaded {
            row.image_loaded = true;
            // GPUI может держать измеренную высоту строки за видимой частью окна.
            self.list.splice(at..at + 1, 1);
            self.restore_anchor(anchor);
        }
    }

    /// Печатает измерение текущей опоры или последней видимой строки.
    fn print_geometry(&self) {
        let row = self
            .anchor()
            .or_else(|| self.geometry(self.rows[self.rows.len() - 1].uuid));
        if let Some(row) = row {
            eprintln!(
                "uuid={} y={} height={} viewport_height={} rows={} built={}",
                row.uuid,
                row.y,
                row.height,
                row.viewport_height,
                self.row_count(),
                self.frame.borrow().built.len()
            );
        }
    }
}

/// Строит текст и картинку, если задержанная загрузка этой строки завершилась.
fn message_row(row: &Row, frame: Rc<RefCell<Frame>>) -> impl IntoElement {
    let uuid = row.uuid;
    frame.borrow_mut().built.insert(uuid);
    let extra_lines = row.serial.rem_euclid(4);
    div()
        .relative()
        .w_full()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(rgb(0x353b48))
        .child(format!("Сообщение {} · {}", row.serial, uuid))
        .children((0..extra_lines).map(|_| div().child("Пример текста сообщения")))
        .when(row.image_loaded, |row| {
            row.child(
                div()
                    .mt_2()
                    .h(px(IMAGE_HEIGHT))
                    .w(px(180.))
                    .rounded_md()
                    .bg(rgb(0x345482))
                    .p_2()
                    .child("Картинка загружена"),
            )
        })
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, (), _, _| {
                    // В нижнем режиме GPUI хранит позицию после последней строки:
                    // здесь доступны фактические границы и у этого кадра.
                    frame.borrow_mut().painted.insert(uuid, bounds);
                },
            )
            .absolute()
            .inset_0(),
        )
}

/// Рисует и выделяет при наведении команду окна.
fn control(label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(label)
        .rounded_md()
        .px_2()
        .py_1()
        .bg(rgb(0x303747))
        .hover(|style| style.bg(rgb(0x445574)))
        .cursor_pointer()
        .child(label)
}

impl Render for HistoryProbe {
    /// Рисует управление сценарием и строит только строки рядом с областью прокрутки.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frame = Rc::default();
        let frame = self.frame.clone();
        let rows = self.rows.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x17191e))
            .text_color(rgb(0xf7f8fa))
            .text_sm()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .flex_shrink_0()
                    .gap_1()
                    .p_2()
                    .children(
                        [
                            ("50 старых", HistoryAction::Prepend),
                            ("К UUID", HistoryAction::Jump(JUMP_TARGET)),
                            ("Картинка выше", HistoryAction::GrowAbove),
                            ("Новое", HistoryAction::Append),
                            ("Вниз", HistoryAction::ToBottom),
                        ]
                        .map(|(label, action)| {
                            control(label)
                                .on_click(cx.listener(move |view, _, _, cx| view.apply(action, cx)))
                        }),
                    )
                    .child(
                        control("Геометрия")
                            .on_click(cx.listener(|view, _, _, _| view.print_geometry())),
                    ),
            )
            .child(
                list(self.list.clone(), move |index, _, _| {
                    message_row(&rows[index], frame.clone()).into_any_element()
                })
                .flex_1()
                .min_h_0()
                .w_full(),
            )
    }
}

#[cfg(all(test, feature = "gpui-test"))]
mod tests {
    use super::*;
    use gpui::{TestAppContext, point, size};

    /// Проверяет переход к UUID, вставку страницы, рост картинки и два режима новых сообщений.
    #[gpui::test]
    fn varying_rows_keep_the_reading_anchor_and_follow_the_end(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let history = cx.update(|_, cx| cx.new(|_| HistoryProbe::new()));
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.draw(point(px(0.), px(0.)), size(px(720.), px(380.)), |_, _| {
                history.clone()
            });
        };
        let apply = |cx: &mut gpui::VisualTestContext, action| {
            cx.update(|_, cx| history.update(cx, |view, cx| view.apply(action, cx)));
        };
        let geometry = |cx: &mut gpui::VisualTestContext, uuid| {
            cx.update(|_, cx| {
                history
                    .read(cx)
                    .geometry(uuid)
                    .unwrap_or_else(|| panic!("Строка {uuid} не нарисована"))
            })
        };
        let last = |cx: &mut gpui::VisualTestContext| cx.update(|_, cx| last_uuid(&history, cx));

        draw(cx);
        let last_id = last(cx);
        assert_at_bottom(geometry(cx, last_id));

        let above = Uuid::from_u128(JUMP_TARGET.as_u128() - 1);
        apply(cx, HistoryAction::Jump(above));
        draw(cx);
        let height_before = geometry(cx, above).height;

        apply(cx, HistoryAction::Jump(JUMP_TARGET));
        draw(cx);
        let initial = geometry(cx, JUMP_TARGET);
        assert!(
            initial.y.abs() <= 1. && initial.height < initial.viewport_height,
            "Целевой UUID должен появиться у начала области: {initial:?}"
        );

        // При реальном чтении верхняя строка может быть частично скрыта.
        cx.update(|_, cx| history.update(cx, |view, _| view.list.scroll_by(px(11.))));
        draw(cx);
        let before = geometry(cx, JUMP_TARGET);
        assert!((before.y + 11.).abs() <= 1., "{before:?}");

        apply(cx, HistoryAction::Prepend);
        draw(cx);
        assert_stable("вставка 50 строк", before, geometry(cx, JUMP_TARGET));
        cx.update(|_, cx| {
            let history = history.read(cx);
            assert_eq!(history.row_count(), INITIAL_ROWS as usize + 50);
            let frame = history.frame.borrow();
            eprintln!(
                "Строки, построенные в кадре: {} из {}",
                frame.built.len(),
                history.row_count()
            );
            assert!(
                frame.built.contains(&JUMP_TARGET) && frame.built.len() < 60,
                "В кадре построено {} из {} строк",
                frame.built.len(),
                history.row_count()
            );
        });

        apply(cx, HistoryAction::GrowAbove);
        cx.run_until_parked();
        cx.update(|_, cx| {
            let history = history.read(cx);
            assert!(
                !history.rows[history.index[&above]].image_loaded,
                "Картинка появилась до истечения задержки"
            );
        });
        cx.background_executor.advance_clock(GROWTH_DELAY);
        cx.run_until_parked();
        draw(cx);
        assert_stable("рост строки выше опоры", before, geometry(cx, JUMP_TARGET));

        apply(cx, HistoryAction::Append);
        draw(cx);
        assert_stable(
            "новая строка во время чтения",
            before,
            geometry(cx, JUMP_TARGET),
        );
        cx.update(|_, cx| assert!(history.read(cx).geometry(last_uuid(&history, cx)).is_none()));

        apply(cx, HistoryAction::Jump(above));
        draw(cx);
        let resized = geometry(cx, above);
        eprintln!("Высота строки с картинкой: до {height_before}px, после {resized:?}");
        assert!(
            resized.height - height_before >= IMAGE_HEIGHT,
            "Картинка не увеличила высоту строки: до {height_before}px, после {resized:?}"
        );

        apply(cx, HistoryAction::ToBottom);
        draw(cx);
        let last_id = last(cx);
        assert_at_bottom(geometry(cx, last_id));
        let old_last = last(cx);
        apply(cx, HistoryAction::Append);
        draw(cx);
        assert_ne!(last(cx), old_last);
        let last_id = last(cx);
        assert_at_bottom(geometry(cx, last_id));
    }

    /// Возвращает UUID последнего сообщения без входа в контекст окна.
    fn last_uuid(history: &gpui::Entity<HistoryProbe>, cx: &gpui::App) -> Uuid {
        history.read(cx).rows.last().unwrap().uuid
    }

    /// Проверяет UUID и высоту его верхней границы с допуском в один логический пиксель.
    fn assert_stable(step: &str, before: RowGeometry, after: RowGeometry) {
        let delta = after.y - before.y;
        eprintln!("{step}: смещение {delta}px, до {before:?}, после {after:?}");
        assert!(
            before.uuid == after.uuid && delta.abs() <= 1.,
            "{step}: смещение {delta}px, до {before:?}, после {after:?}"
        );
    }

    /// Проверяет, что последняя строка видна и касается нижней границы списка.
    fn assert_at_bottom(last: RowGeometry) {
        eprintln!("Последняя строка у нижнего края: {last:?}");
        assert!(
            last.y >= 0. && (last.y + last.height - last.viewport_height).abs() <= 1.,
            "Нижняя строка не у нижнего края: {last:?}"
        );
    }
}
