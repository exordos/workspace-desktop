//! Собирает непрочитанные строки из готового кадра и показывает ход отправки отметки.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    time::Duration,
};

use gpui::{Context, Subscription, Task, Window};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        message_actions::MessageTarget,
        read::{READ_OBSERVED_LIMIT, ReadBoundary, ReadPhase, ReadRequestState},
    },
    storage::{HistoryTime, HistoryWindow},
};

use crate::gpui_client::runtime::RuntimeClient;

use super::{Conversation, Load, State};

const READ_FEEDBACK_DELAY: Duration = Duration::from_millis(160);

/// Прочтение, уже увиденное в кадре этого экрана и привязанное к точной теме.
#[derive(Default)]
struct TopicRead {
    seen: BTreeSet<(HistoryTime, Uuid)>, // Известные непрочитанные строки в серверном порядке.
    requested: Option<(Uuid, Vec<Uuid>)>, // Последний отправленный набор, чтобы не повторять его каждый кадр.
    blocked: Option<(HistoryTime, Uuid)>, // Первая непрочитанная строка, преграждающая более позднюю границу.
    pending: Option<Uuid>,                // Строка, которую сейчас подтверждает сервер.
    pending_target: Option<Uuid>,         // Граница запроса, ещё допускающая показ подсветки.
    pending_delay: Option<Task<()>>,      // Отменяемая пауза перед подсветкой медленного запроса.
    failed: bool,  // Неподтверждённая попытка может повториться после фокуса или сверки.
    attempt: Uuid, // Отдельная ревизия наблюдения за очередью этой темы.
    listener: Option<Task<()>>, // Чтение статуса до замены запроса или закрытия экрана.
}

impl TopicRead {
    /// Прерывает подсветку и отменяет ещё не истёкшую задержку.
    fn hide_feedback(&mut self) -> bool {
        self.pending_target = None;
        self.pending_delay = None;
        self.pending.take().is_some()
    }
}

/// Локальные признаки видимости, отделённые от серверной очереди и отметок истории.
#[derive(Default)]
pub(super) struct Reading {
    pub runtime: Option<RuntimeClient>, // Отсутствует у изолированных испытательных экранов.
    pub visible: bool, // Разговор показан в рабочей области без перекрывающей панели.
    pub fresh: bool,   // Экран получил окно из сети, показывает историю текущего входа.
    pub knows_unread: bool, // Текущее окно включает проверенную границу или подтверждённый пустой хвост.
    topics: HashMap<Uuid, TopicRead>, // Наблюдения каждой точной темы видимой истории.
    activation: Option<Subscription>, // Сигнал фокуса системного окна, даже без движения мыши.
}

impl Reading {
    /// Отбрасывает признаки прежнего окна. Уже принятые Core команды завершаются отдельно.
    pub(super) fn clear(&mut self) {
        self.fresh = false;
        self.knows_unread = false;
        self.clear_observations();
    }

    /// Сбрасывает доказательства непрерывного просмотра при явном переходе через историю.
    pub(super) fn clear_observations(&mut self) {
        self.topics.clear();
    }

    /// Убирает подтверждённо прочитанные строки из рабочего набора и снимает преграду.
    pub(super) fn accept_history(&mut self, history: &HistoryWindow) {
        for row in &history.messages {
            if row.read == Some(true)
                && let Some(topic) = self.topics.get_mut(&row.topic_id)
            {
                topic
                    .seen
                    .remove(&(row.metadata.created_at.clone(), row.id));
                if topic.pending_target == Some(row.id) {
                    topic.hide_feedback();
                }
                if topic.blocked.as_ref().is_some_and(|(_, id)| *id == row.id) {
                    topic.blocked = None;
                    topic.requested = None;
                }
            }
        }
    }

    /// Возвращает текущие границы запросов, которые можно подсветить без изменения разметки.
    pub(super) fn pending(&self) -> HashSet<Uuid> {
        self.topics
            .values()
            .filter_map(|topic| topic.pending)
            .collect()
    }

    /// После внешней отметки позволяет снова проверить префикс, даже если его не было в окне.
    pub(super) fn refresh_blocked(&mut self, topic: Option<Uuid>) {
        for (&id, reading) in &mut self.topics {
            if topic.is_none_or(|topic| topic == id) && reading.blocked.take().is_some() {
                reading.requested = None;
            }
        }
    }

    /// Позволяет снова отправить прежнюю видимую границу после восстановления связи или фокуса.
    pub(super) fn rearm_failed(&mut self) -> bool {
        let mut changed = false;
        for reading in self.topics.values_mut() {
            if std::mem::take(&mut reading.failed) {
                reading.requested = None;
                changed = true;
            }
        }
        changed
    }
}

impl Conversation {
    /// Сохраняет доступность рабочей области и запрашивает кадр при возврате к истории.
    pub(in crate::gpui_client) fn set_read_surface_visible(
        &mut self,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        if self.reading.visible != visible {
            self.reading.visible = visible;
            if visible {
                self.rearm_read_revalidation(cx);
                self.reading.rearm_failed();
            }
            cx.notify();
        }
    }

    /// Перепроверяет видимые строки при активации окна после сворачивания или смены приложения.
    pub(super) fn observe_read_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.reading.activation.is_none() && self.reading.runtime.is_some() {
            self.reading.activation =
                Some(cx.observe_window_activation(window, |view, window, cx| {
                    if window.is_window_active() {
                        view.rearm_read_revalidation(cx);
                        view.reading.rearm_failed();
                        cx.notify();
                    }
                }));
        }
    }

    /// Разрешает брать намерение только из живого кадра открытой истории.
    pub(super) fn can_observe_read(&self) -> bool {
        self.reading.visible
            && self.reading.fresh
            && self.reading.runtime.is_some()
            && self.heading.native_markdown
            && !self.active.is_some_and(|flight| {
                flight.load.opens() || matches!(flight.load, Load::RevalidateRead(_, _))
            })
            && !self.is_archived()
            && !self.attachments.is_open()
            && !self.message_menu.is_open()
            && !self.message_delete.is_open()
            && self.time_reference.is_none()
            && matches!(&self.state, State::Ready(history) if !history.needs_refresh)
    }

    /// Принимает видимые непрочитанные строки и передаёт серверу последний видимый адрес темы.
    pub(super) fn observe_read_frame(
        &mut self,
        visible: Vec<(usize, Uuid)>,
        cx: &mut Context<Self>,
    ) {
        if !self.can_observe_read() {
            return;
        }
        let Some(runtime) = self.reading.runtime.clone() else {
            return;
        };
        let State::Ready(history) = &self.state else {
            return;
        };
        let mut candidates = HashMap::new();
        for (index, id) in visible {
            let Some(row) = history.messages.get(index).filter(|row| row.id == id) else {
                continue;
            };
            if row.read != Some(false) || !row.read_eligible() {
                continue;
            }
            let topic = self.reading.topics.entry(row.topic_id).or_default();
            topic.seen.insert((row.metadata.created_at.clone(), row.id));
            if topic.seen.len() > READ_OBSERVED_LIMIT {
                topic.seen.pop_last();
            }
            if topic
                .seen
                .contains(&(row.metadata.created_at.clone(), row.id))
            {
                candidates.insert(
                    row.topic_id,
                    ReadBoundary {
                        target: MessageTarget {
                            scope: history.scope,
                            topic_id: row.topic_id,
                            message_id: row.id,
                        },
                        created_at: row.metadata.created_at.clone(),
                    },
                );
            }
        }
        for (topic_id, boundary) in candidates {
            let topic = self
                .reading
                .topics
                .get_mut(&topic_id)
                .expect("тема из видимой строки");
            let seen = topic
                .seen
                .iter()
                .take_while(|(time, id)| {
                    (time, *id) <= (&boundary.created_at, boundary.target.message_id)
                })
                .map(|(_, id)| *id)
                .collect::<Vec<_>>();
            if topic.blocked.as_ref().is_some_and(|(time, id)| {
                (&boundary.created_at, boundary.target.message_id) >= (time, *id)
                    && !seen.contains(id)
            }) || topic
                .requested
                .as_ref()
                .is_some_and(|(id, sent)| *id == boundary.target.message_id && *sent == seen)
            {
                continue;
            }
            topic.requested = Some((boundary.target.message_id, seen.clone()));
            topic.failed = false;
            let attempt = Uuid::new_v4();
            topic.attempt = attempt;
            topic.listener = Some(watch_request(
                runtime.clone(),
                self.address.clone(),
                boundary,
                seen,
                attempt,
                cx,
            ));
        }
    }

    /// Применяет состояние очереди только к попытке, которую ещё показывает этот экран.
    fn apply_read_request(
        &mut self,
        topic_id: Uuid,
        attempt: Uuid,
        state: ReadRequestState,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(topic) = self.reading.topics.get_mut(&topic_id) else {
            return false;
        };
        if topic.attempt != attempt {
            return false;
        }
        if let ReadRequestState::Blocked(anchor) = &state {
            topic.blocked = Some((
                anchor.boundary.created_at.clone(),
                anchor.boundary.target.message_id,
            ));
        }
        if matches!(state, ReadRequestState::Idle) {
            if let Some((_, submitted)) = &topic.requested {
                topic.seen.retain(|(_, id)| !submitted.contains(id));
            }
            topic.blocked = None;
        }
        topic.failed = matches!(state, ReadRequestState::Failed(_));
        let pending = match state {
            ReadRequestState::Pending(pending)
                if matches!(pending.phase, ReadPhase::Submitting | ReadPhase::Confirming)
                    && matches!(&self.state, State::Ready(window) if window.messages.iter().any(|row| {
                        row.topic_id == topic_id && row.id == pending.message_id && row.read == Some(false)
                    })) =>
            {
                Some(pending.message_id)
            }
            _ => None,
        };
        if topic.pending_target != pending {
            if topic.hide_feedback() {
                cx.notify();
            }
            if let Some(id) = pending {
                topic.pending_target = Some(id);
                let delay = cx.background_executor().timer(READ_FEEDBACK_DELAY);
                topic.pending_delay = Some(cx.spawn(async move |entity, cx| {
                    delay.await;
                    let _ = entity.update(cx, |view, cx| {
                        if let Some(topic) = view.reading.topics.get_mut(&topic_id)
                            && topic.pending_target == Some(id)
                        {
                            topic.pending = Some(id);
                            cx.notify();
                        }
                    });
                }));
            }
        }
        true
    }
}

/// Подписывает окно на ход операции Core. Отказ от подписки не отменяет принятую отметку.
fn watch_request(
    runtime: RuntimeClient,
    address: crate::gpui_client::contracts::ConversationAddress,
    boundary: ReadBoundary,
    seen: Vec<Uuid>,
    attempt: Uuid,
    cx: &mut Context<Conversation>,
) -> Task<()> {
    let topic = boundary.target.topic_id;
    let id = boundary.target.message_id;
    cx.spawn(async move |entity, cx| {
        let request = runtime
            .conversation(address.clone(), move |services, address| async move {
                services.messenger.enqueue_read(
                    services.sync.clone(),
                    &address.route.owner,
                    address.generation,
                    boundary,
                    seen,
                )
            })
            .await;
        let Ok(Ok(mut updates)) = request.value else {
            let _ = entity.update(cx, |view, cx| {
                if view.address == address {
                    view.apply_read_request(topic, attempt, ReadRequestState::Failed(id), cx);
                }
            });
            return;
        };
        loop {
            let state = updates.borrow_and_update().clone();
            let current = entity.update(cx, |view, cx| {
                view.address == address && view.apply_read_request(topic, attempt, state, cx)
            });
            if !matches!(current, Ok(true)) || updates.changed().await.is_err() {
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use workspace_core::storage::{HistoryEdge, HistoryScope};

    use super::*;
    use crate::gpui_client::conversation::history_fixtures::fixture_rows;

    /// Другой клиент уже подтвердил чтение цели, хотя локальная квитанция ещё ожидается.
    #[test]
    fn confirmed_row_removes_pending_feedback() {
        let scope = HistoryScope {
            stream_id: Uuid::from_u128(1),
            topic_id: None,
        };
        let mut messages = fixture_rows(scope, 2, 2);
        let (id, topic) = (messages[0].id, messages[0].topic_id);
        messages[0].read = Some(true);
        let mut reading = Reading::default();
        reading.topics.insert(
            topic,
            TopicRead {
                pending: Some(id),
                pending_target: Some(id),
                ..TopicRead::default()
            },
        );

        reading.accept_history(&HistoryWindow {
            id: 1,
            scope,
            messages,
            older: HistoryEdge::End,
            newer: HistoryEdge::End,
            needs_refresh: false,
        });
        assert!(reading.pending().is_empty());
        assert!(reading.topics[&topic].pending_target.is_none());
    }
}
