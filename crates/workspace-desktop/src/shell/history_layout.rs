//! Применяет новые строки между замерами Iced и возвращает прежнюю видимую позицию.

use std::sync::Arc;

use iced::Task;
use uuid::Uuid;
use workspace_core::auth::SessionStatus;
use workspace_core::storage::{HistoryEdge, HistoryWindow};

use super::history::{self, HistoryPatch, HistoryState};
use super::{Message, Shell, history_anchor};
use history_anchor::{HistoryLayout, ScrollCorrection};

impl Shell {
    /// Проводит замену готового окна свежим хвостом через тот же замер, что и события.
    pub(super) fn queue_refreshed_window(&mut self, window: HistoryWindow) -> Task<Message> {
        let screen = self.history.as_mut().expect("экран истории уже открыт");
        if matches!(&screen.state, HistoryState::Ready(previous) if previous.id != window.id) {
            screen.cancel_layout_update();
            screen.older_pending = false;
            screen.older_failed = false;
        }
        let outbox = self
            .sources
            .0
            .sync
            .outbox(&screen.route.owner, screen.generation, window.scope)
            .unwrap_or_else(|_| screen.send.outbox.clone());
        screen.layout.pending = Some(HistoryPatch {
            window: Arc::new(window),
            outbox,
            older_completed: false,
        });
        self.measure_before_change()
    }

    /// Готовит перестройку после записи в очередь или прихода старой страницы.
    pub(super) fn queue_history_patch(&mut self, older_completed: bool) -> Task<Message> {
        self.queue_history_change(older_completed, None)
    }

    /// Читает текущее состояние разговора и откладывает перестройку до замера старых строк.
    pub(super) fn queue_history_change(
        &mut self,
        older_completed: bool,
        created: Option<Uuid>,
    ) -> Task<Message> {
        let Some(screen) = self.visible_history() else {
            return Task::none();
        };
        let HistoryState::Ready(window) = &screen.state else {
            return Task::none();
        };
        let sync = &self.sources.0.sync;
        let owner = &screen.route.owner;
        let generation = screen.generation;
        let incoming = screen
            .layout
            .pending
            .as_ref()
            .map_or(window, |patch| &patch.window);
        // Сначала читаем очередь. Если между чтениями придёт подтверждение, оно попадёт в
        // историю, а совпадающий UUID временной строки вид не продублирует.
        let outbox = sync
            .outbox(owner, generation, window.scope)
            .unwrap_or_else(|_| screen.send.outbox.clone());
        let mut fresh =
            match sync.refresh_history_window(owner, generation, incoming.scope, incoming.id) {
                Ok(Some(fresh)) => fresh,
                Ok(None) => {
                    // Отозванное или устаревшее окно нельзя оставлять на экране.
                    return self.open_history(screen.route.clone(), generation);
                }
                Err(_) => {
                    if older_completed {
                        let screen = self.history.as_mut().unwrap();
                        screen.older_pending = false;
                        screen.older_failed = true;
                    }
                    return Task::none();
                }
            };
        let older_completed = older_completed
            || screen
                .layout
                .pending
                .as_ref()
                .is_some_and(|patch| patch.older_completed);
        if incoming.id == window.id
            && screen.older_pending
            && !older_completed
            && let Some(first) = window.messages.first()
        {
            // Старая страница могла записаться до сигнала об окончании её запроса.
            let boundary = (&first.metadata.created_at, first.id);
            fresh
                .messages
                .retain(|row| (&row.metadata.created_at, row.id) >= boundary);
            fresh.older = window.older.clone();
        }
        if !older_completed
            && outbox == screen.send.outbox
            && (fresh == **window
                || created.is_some_and(|id| !fresh.messages.iter().any(|row| row.id == id)))
            && screen.layout.pending.is_none()
        {
            return Task::none();
        }
        self.history.as_mut().unwrap().layout.pending = Some(HistoryPatch {
            window: Arc::new(fresh),
            outbox,
            older_completed,
        });
        self.measure_before_change()
    }

    /// Запускает замер, если предыдущая перестройка уже завершилась.
    fn measure_before_change(&mut self) -> Task<Message> {
        let revision = self.navigation_revision;
        let Some(screen) = self.history.as_mut() else {
            return Task::none();
        };
        if screen.layout.measuring || screen.layout.pending.is_none() {
            return Task::none();
        }
        screen.layout.measuring = true;
        screen.layout.revision = screen.layout.revision.wrapping_add(1);
        let layout_revision = screen.layout.revision;
        let scroll_revision = screen.scroll_revision;
        history_anchor::measure(history::list_id(revision), screen.layout_rows(revision)).map(
            move |position| Message::HistoryBeforeInsert {
                revision,
                layout_revision,
                scroll_revision,
                position,
            },
        )
    }

    /// Применяет самый новый снимок и запрашивает координаты тех же UUID после разметки.
    pub(super) fn apply_history_patch(
        &mut self,
        revision: u64,
        layout_revision: u64,
        scroll_revision: u64,
        position: Option<HistoryLayout>,
    ) -> Task<Message> {
        let Some(screen) = self.active_layout(revision, layout_revision) else {
            return Task::none();
        };
        let Some(patch) = screen.layout.pending.take() else {
            screen.layout.measuring = false;
            return Task::none();
        };
        let follow_tail = !patch.older_completed
            && matches!(&screen.state, HistoryState::Ready(window) if window.newer == HistoryEdge::End)
            && patch.window.newer == HistoryEdge::End;
        if patch.older_completed {
            screen.older_pending = false;
            screen.older_failed = false;
        }
        screen.state = HistoryState::Ready(patch.window);
        screen.send.outbox = patch.outbox;
        let Some(before) = position.filter(|_| scroll_revision == screen.scroll_revision) else {
            screen.layout.measuring = false;
            return Task::none();
        };
        history_anchor::measure(history::list_id(revision), screen.layout_rows(revision)).map(
            move |after| Message::HistoryAfterInsert {
                revision,
                layout_revision,
                scroll_revision,
                before: before.clone(),
                after,
                follow_tail: follow_tail && before.at_end(),
            },
        )
    }

    /// Восстанавливает положение, пока пользователь и маршрут не изменились.
    pub(super) fn restore_history_position(
        &mut self,
        revision: u64,
        layout_revision: u64,
        scroll_revision: u64,
        before: HistoryLayout,
        after: Option<HistoryLayout>,
        follow_tail: bool,
    ) -> Task<Message> {
        let Some(screen) = self.active_layout(revision, layout_revision) else {
            return Task::none();
        };
        screen.layout.measuring = false;
        let task = after
            .filter(|_| scroll_revision == screen.scroll_revision)
            .and_then(|after| {
                let correction = if follow_tail {
                    Some(ScrollCorrection::Tail)
                } else {
                    before
                        .restored_scroll(&after)
                        .filter(|y| (y - after.scroll()).abs() >= 0.1)
                        .map(ScrollCorrection::Offset)
                }?;
                Some(history_anchor::correct(
                    history::list_id(revision),
                    after,
                    correction,
                ))
            })
            .unwrap_or_else(Task::none);
        task.chain(self.measure_before_change())
    }

    /// Не принимает замеры прежнего окна, даже если измерительный виджет ещё ответил.
    fn active_layout(
        &mut self,
        revision: u64,
        layout_revision: u64,
    ) -> Option<&mut history::HistoryScreen> {
        if self.navigation_revision != revision || self.visible_history().is_none() {
            return None;
        }
        let screen = self.history.as_mut()?;
        if !self.sources.0.auth.snapshot().iter().any(|session| {
            session.context == screen.route.owner
                && session.generation == screen.generation
                && session.status == SessionStatus::Active
        }) {
            screen.cancel_layout_update();
            return None;
        }
        (screen.layout.revision == layout_revision && screen.layout.measuring).then_some(screen)
    }
}
