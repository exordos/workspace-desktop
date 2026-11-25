//! Запрашивает первую непрочитанную строку и ведёт фоновые отметки по темам.
//! Очередь объединяет наблюдения, проверяет непрерывность префикса и подтверждает серверный итог.

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    sync::{Arc, Mutex},
    time::Duration,
};

use thiserror::Error;
use tokio::{
    runtime::Handle,
    sync::watch,
    time::{Instant, sleep, timeout},
};
use uuid::Uuid;

use crate::{
    auth::{AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    storage::{HistoryReadFence, HistoryScope, HistoryTime},
    sync::{Sync, SyncError},
};

use super::{
    MessengerSource,
    history::OpenHistoryError,
    message_actions::{
        MessageTarget, MutationReceipt, MutationStatus, MutationStatusError, MutationTicket,
    },
};

#[cfg(test)]
mod tests;

/// Сколько видимых UUID можно подтвердить одной проверкой серверной страницы.
pub const READ_OBSERVED_LIMIT: usize = 128;
const COALESCE_DELAY: Duration = Duration::from_millis(100);
const RETRY_DELAY: Duration = Duration::from_millis(500);
const RECEIPT_DEADLINE: Duration = Duration::from_secs(8);
const INITIAL_STATUS_DELAY: Duration = Duration::from_millis(250);
const MAX_STATUS_DELAY: Duration = Duration::from_secs(2);
const SUBMIT_ATTEMPTS: usize = 2;

/// Фактическое размещение видимой строки с точным временем из истории.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadBoundary {
    pub target: MessageTarget,   // UUID сообщения, стрима и фактической темы.
    pub created_at: HistoryTime, // Серверное время, первый компонент порядка чтения.
}

impl ReadBoundary {
    /// Возвращает точную тему, даже когда экран лички показывает несколько тем.
    pub(super) fn scope(&self) -> HistoryScope {
        HistoryScope {
            stream_id: self.target.scope.stream_id,
            topic_id: Some(self.target.topic_id),
        }
    }

    /// Сравнивает границы так же, как сервер сравнивает размещения в теме.
    pub(super) fn order(&self) -> (&HistoryTime, Uuid) {
        (&self.created_at, self.target.message_id)
    }
}

/// Первая непрочитанная строка или препятствие перед выбранной границей.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnreadAnchor {
    pub boundary: ReadBoundary, // Адрес и порядок даже для неподдерживаемой строки.
    pub eligible: bool,         // Клиент может отметить эту строку после её просмотра.
}

/// Этап текущего задания, пригодный для индикации в окне истории.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadPhase {
    Queued,      // Собираются соседние наблюдения или проверяется видимый префикс.
    Submitting,  // Выполняется POST для этого сообщения.
    Confirming,  // Сервер принял команду, проверяется квитанция.
    Reconciling, // Исход неизвестен, перечитывается серверное непрочитанное.
}

/// Текущая граница задания в исходной теме.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingRead {
    pub message_id: Uuid, // UUID строки, на которую отправляется отметка.
    pub phase: ReadPhase, // Этап запроса или проверки его итога.
}

/// Обновления фоновой очереди для одного наблюдателя точной темы.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadRequestState {
    Pending(PendingRead),  // Клиент ещё устанавливает серверный итог.
    Blocked(UnreadAnchor), // Найдена строка без подтверждённой видимости или поддержки.
    Failed(Uuid),          // Итог запроса не подтверждён для указанной строки.
    Idle,                  // Все принятые заявки в теме подтверждены.
}

/// Причина, по которой не удалось принять или подтвердить отметку.
#[derive(Debug, Error)]
pub enum ReadError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Вход завершился, изменился или не выдал токен.
    #[error("invalid read target or observed messages")]
    InvalidInput, // Адрес или список ранее видимых UUID не подходит для запроса.
    #[error("server returned HTTP {0}")]
    Http(u16), // Сервер ответил ошибкой с указанным статусом.
    #[error("read outcome is unknown")]
    Unknown, // Ответ потерян или не подтверждает ожидаемую область и сообщение.
    #[error("the read target is unavailable")]
    Unavailable, // Граница не подтверждена в локальной истории либо право было отозвано.
    #[error("the server rejected the read mutation")]
    Rejected, // Журнал сообщил об отказе уже принятой операции.
    #[error("the read receipt has not settled")]
    Confirming, // Известная серверу команда ещё может примениться.
    #[error(transparent)]
    History(#[from] OpenHistoryError), // Не удалось перечитать серверное непрочитанное.
    #[error(transparent)]
    Sync(#[from] SyncError), // Не удалось проверить или записать подтверждённое чтение.
}

/// Идентичность очереди, которая не меняется вместе с открытым экраном.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ReadKey {
    owner: OwnerContext,           // Исходный сервер, проект и пользователь.
    generation: SessionGeneration, // Вход, который принял наблюдение.
    scope: HistoryScope,           // Стрим и обязательная точная тема очереди.
}

/// Одна заявка, сохранённая вместе с доказательством видимости.
#[derive(Clone)]
struct ReadIntent {
    boundary: ReadBoundary, // Самая поздняя видимая граница в теме.
    observed: Vec<Uuid>,    // Подтверждённые экраном UUID в этой теме.
    access_revision: i64,   // Локальная версия доступа до приёма заявки.
    id: Uuid,               // Идентификатор для трассировки одного задания.
}

impl ReadIntent {
    /// Соединяет доказательства в действующем доступе, сохраняя большую границу.
    fn merge(mut self, mut other: Self) -> Self {
        if self.access_revision != other.access_revision {
            return if self.access_revision > other.access_revision {
                self
            } else {
                other
            };
        }
        if other.boundary.order() > self.boundary.order() {
            std::mem::swap(&mut self, &mut other);
        }
        let mut known: HashSet<_> = self.observed.iter().copied().collect();
        for id in other.observed {
            if self.observed.len() == READ_OBSERVED_LIMIT {
                break;
            }
            if known.insert(id) {
                self.observed.push(id);
            }
        }
        self
    }

    /// Определяет, уже покрывает ли задание границу и все её наблюдения.
    fn covers(&self, other: &Self) -> bool {
        self.access_revision == other.access_revision
            && self.boundary.order() >= other.boundary.order()
            && other.observed.iter().all(|id| self.observed.contains(id))
    }

    /// Готовит состояние для экрана, не сохраняя его в истории.
    fn phase(&self, phase: ReadPhase) -> ReadRequestState {
        ReadRequestState::Pending(PendingRead {
            message_id: self.boundary.target.message_id,
            phase,
        })
    }
}

/// Состояние запуска для одной темы. Только один работник обращается к серверу.
struct TopicQueue {
    waiting: Option<ReadIntent>, // Самое позднее наблюдение, ожидающее отправки.
    running: Option<ReadIntent>, // Заявка, для которой работает проверка или запрос.
    state: watch::Sender<ReadRequestState>, // Состояние для всех наблюдателей этой темы.
    scheduled: bool,             // Работник запущен или ждёт окончания короткого окна объединения.
    receipt: Option<HeldRead>,   // Принятая сервером команда блокирует следующую отправку.
}

/// Квитанция, которую сервер не завершил в течение обычного времени ожидания.
#[derive(Clone)]
struct HeldRead {
    intent: ReadIntent,       // Исходный префикс, даже если просмотр пошёл дальше.
    ticket: MutationTicket,   // Срок ожидания нового запроса не меняет эту команду.
    before: HistoryReadFence, // Момент отправки для защиты более свежих флагов.
}

/// Результат ограниченной проверки уже принятой сервером команды.
#[derive(Clone, Copy)]
enum ReceiptState {
    Applied,     // Сервер подтвердил исходный префикс.
    Rejected,    // Сервер отказал в изменении.
    Waiting,     // За срок ожидания сервер не сообщил конечный результат.
    Unavailable, // Сервер вернул 404 владельцу с действующим доступом.
}

/// Очереди действующих входов, принадлежащие источнику, а не экрану.
#[derive(Default)]
pub(super) struct ReadRequests {
    topics: Mutex<HashMap<ReadKey, TopicQueue>>, // Защищает замену ожидающих заявок.
}

impl ReadRequests {
    /// Принимает видимость в очередь и сообщает, нужно ли запустить работника.
    fn enqueue(
        &self,
        key: ReadKey,
        mut intent: ReadIntent,
    ) -> (watch::Receiver<ReadRequestState>, bool) {
        let mut topics = self
            .topics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        topics.retain(|existing, queue| {
            queue.scheduled
                || existing.owner.account_id != key.owner.account_id
                || existing.generation.0 >= key.generation.0
        });
        match topics.entry(key) {
            Entry::Vacant(slot) => {
                let (state, receiver) = watch::channel(intent.phase(ReadPhase::Queued));
                slot.insert(TopicQueue {
                    waiting: Some(intent),
                    running: None,
                    state,
                    scheduled: true,
                    receipt: None,
                });
                (receiver, true)
            }
            Entry::Occupied(mut slot) => {
                let queue = slot.get_mut();
                if !queue
                    .running
                    .as_ref()
                    .is_some_and(|running| running.covers(&intent))
                    && !queue
                        .waiting
                        .as_ref()
                        .is_some_and(|waiting| waiting.covers(&intent))
                {
                    if let Some(running) = &queue.running {
                        intent = running.clone().merge(intent);
                    }
                    queue.waiting = Some(match queue.waiting.take() {
                        Some(waiting) => waiting.merge(intent),
                        None => intent,
                    });
                    if let Some(waiting) = &queue.waiting
                        && queue.running.is_none()
                    {
                        queue.state.send_replace(waiting.phase(ReadPhase::Queued));
                    }
                }
                let start = !queue.scheduled && queue.waiting.is_some();
                queue.scheduled |= start;
                (queue.state.subscribe(), start)
            }
        }
    }

    /// Читает квитанцию той же темы, которая ещё запрещает новую отправку.
    fn receipt(&self, key: &ReadKey) -> Option<HeldRead> {
        self.topics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)?
            .receipt
            .clone()
    }

    /// Удерживает принятую команду между ограниченными циклами проверки.
    fn set_receipt(&self, key: &ReadKey, receipt: Option<HeldRead>) {
        let mut topics = self
            .topics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(queue) = topics.get_mut(key) {
            queue.receipt = receipt;
        }
    }

    /// Передаёт работнику самое позднее наблюдение и публикует его текущий этап.
    fn take(&self, key: &ReadKey) -> Option<ReadIntent> {
        let mut topics = self
            .topics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let queue = topics.get_mut(key)?;
        let intent = queue.waiting.take()?;
        queue.state.send_replace(intent.phase(ReadPhase::Queued));
        queue.running = Some(intent.clone());
        Some(intent)
    }

    /// Показывает этап на строке, которая фактически стала целью запроса.
    fn publish(&self, key: &ReadKey, intent: &ReadIntent, phase: ReadPhase) {
        let topics = self
            .topics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(queue) = topics.get(key) {
            queue.state.send_replace(intent.phase(phase));
        }
    }

    /// Сохраняет ожидающее наблюдение или закрывает очередь с итогом последнего задания.
    fn finish(&self, key: &ReadKey, state: ReadRequestState) -> bool {
        let mut topics = self
            .topics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(queue) = topics.get_mut(key) else {
            return false;
        };
        queue.running = None;
        if queue.receipt.is_some() {
            // Новое наблюдение сможет перепроверить квитанцию, но не отправит второй POST.
            queue.scheduled = false;
            queue.state.send_replace(state);
            return false;
        }
        if queue.waiting.is_some() {
            return true;
        }
        queue.state.send_replace(state);
        topics.remove(key);
        false
    }
}

impl MessengerSource {
    /// Проверяет точный адрес явного действия и возвращает серверную квитанцию.
    pub async fn read_up_to(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        message_id: Uuid,
    ) -> Result<MutationReceipt, ReadError> {
        if scope.stream_id.is_nil()
            || scope.topic_id.is_none_or(|topic| topic.is_nil())
            || message_id.is_nil()
        {
            return Err(ReadError::InvalidInput);
        }
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let before = access_revision(sync, owner, generation, scope)?;
        let (target, _) = self
            .rest
            .load_read_target(owner, &token, scope, message_id)
            .await?;
        if access_revision(sync, owner, generation, scope)? != before {
            return Err(ReadError::Unavailable);
        }
        let result = self
            .rest
            .read_up_to(owner, &token, scope, message_id, Some(&target.created_at))
            .await;
        // Даже успешный ответ не относится к новому входу того же аккаунта.
        auth.with_active_generation(owner, generation, || ())?;
        Ok(read_receipt(owner, generation, result?))
    }

    /// Читает первую непрочитанную строку, включая форматы без поддержки на экране.
    pub async fn first_unread(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<Option<UnreadAnchor>, OpenHistoryError> {
        if scope.stream_id.is_nil() || scope.topic_id.is_some_and(|topic| topic.is_nil()) {
            return Err(OpenHistoryError::InvalidTarget);
        }
        let before = access_revision(sync, owner, generation, scope)?;
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self.rest.unread_page(owner, &token, scope, 1, None).await;
        if access_revision(sync, owner, generation, scope)? != before {
            return Err(OpenHistoryError::StaleWindow);
        }
        Ok(result?.pop())
    }

    /// Принимает подтверждённую видимость и запускает независимую от экрана заявку.
    pub fn enqueue_read(
        self: &Arc<Self>,
        sync: Arc<Sync>,
        owner: &OwnerContext,
        generation: SessionGeneration,
        boundary: ReadBoundary,
        seen: Vec<Uuid>,
    ) -> Result<watch::Receiver<ReadRequestState>, ReadError> {
        if !boundary.target.is_valid()
            || seen.len() > READ_OBSERVED_LIMIT
            || !seen.contains(&boundary.target.message_id)
            || seen.iter().any(Uuid::is_nil)
        {
            return Err(ReadError::InvalidInput);
        }
        let runtime = Handle::try_current().map_err(|_| ReadError::Unavailable)?;
        let scope = boundary.scope();
        let access_revision = access_revision(&sync, owner, generation, scope)?;
        let key = ReadKey {
            owner: owner.clone(),
            generation,
            scope,
        };
        let mut known = HashSet::new();
        let observed = seen.into_iter().filter(|id| known.insert(*id)).collect();
        let intent = ReadIntent {
            boundary,
            observed,
            access_revision,
            id: Uuid::new_v4(),
        };
        check_candidate(&sync, &key, &intent)?;
        let (receiver, start) = self.read_requests.enqueue(key.clone(), intent);
        if start {
            let source = self.clone();
            runtime.spawn(async move { source.run_read_queue(&sync, &key).await });
        }
        Ok(receiver)
    }

    /// Обрабатывает заявку темы и продолжает только при наличии нового наблюдения.
    async fn run_read_queue(&self, sync: &Sync, key: &ReadKey) {
        loop {
            sleep(COALESCE_DELAY).await;
            let Some(intent) = self.read_requests.take(key) else {
                return;
            };
            log::debug!(
                "messenger_read op_id={} event=started stream_id={} topic_id={:?} message_id={} generation={}",
                intent.id,
                key.scope.stream_id,
                key.scope.topic_id,
                intent.boundary.target.message_id,
                key.generation.0
            );
            let state = match self.process_read(sync, key, &intent).await {
                Ok(None) => ReadRequestState::Idle,
                Ok(Some(blocked)) => ReadRequestState::Blocked(blocked),
                Err(error) => {
                    log::debug!(
                        "messenger_read op_id={} event=failed error={error}",
                        intent.id
                    );
                    ReadRequestState::Failed(intent.boundary.target.message_id)
                }
            };
            match &state {
                ReadRequestState::Idle => {
                    log::debug!("messenger_read op_id={} event=confirmed", intent.id)
                }
                ReadRequestState::Blocked(anchor) => log::debug!(
                    "messenger_read op_id={} event=blocked message_id={} eligible={}",
                    intent.id,
                    anchor.boundary.target.message_id,
                    anchor.eligible
                ),
                _ => {}
            }
            if !self.read_requests.finish(key, state) {
                return;
            }
        }
    }

    /// Сверяет точный префикс и выполняет не более двух допустимых отправок.
    async fn process_read(
        &self,
        sync: &Sync,
        key: &ReadKey,
        intent: &ReadIntent,
    ) -> Result<Option<UnreadAnchor>, ReadError> {
        check_candidate(sync, key, intent)?;
        self.settle_read_receipt(sync, key, intent).await?;
        let mut effective = intent.clone();
        let mut blocked = None;
        let mut remaining = SUBMIT_ATTEMPTS;
        loop {
            check_candidate(sync, key, &effective)?;
            let before = sync.history_read_revision(&key.owner, key.generation)?;
            let token = sync
                .auth()
                .access_token_for_generation(&key.owner, Service::Messenger, key.generation)
                .await?;
            let (_, unread) = tokio::try_join!(
                self.rest
                    .verify_read_stream(&key.owner, &token, intent.boundary.target.scope),
                self.rest.unread_page(
                    &key.owner,
                    &token,
                    key.scope,
                    effective.observed.len() + 1,
                    Some(&effective.boundary.created_at),
                ),
            )?;
            check_candidate(sync, key, &effective)?;
            let (last_seen, gap) = observed_prefix(unread, &effective);
            if let Some(gap) = gap {
                let Some(last_seen) = &last_seen else {
                    return Ok(Some(gap));
                };
                // Можно подтвердить меньшую границу, не затрагивая первую невидимую строку.
                effective.boundary = last_seen.clone();
                blocked = Some(gap);
                check_candidate(sync, key, &effective)?;
            }
            if last_seen
                .as_ref()
                .is_none_or(|last| last.order() != effective.boundary.order())
            {
                self.rest
                    .verify_read_target(&key.owner, &token, &effective.boundary)
                    .await?;
                check_candidate(sync, key, &effective)?;
            }
            if last_seen.is_none() {
                sync.confirm_history_read(
                    &key.owner,
                    key.generation,
                    key.scope,
                    effective.boundary.target.message_id,
                    &effective.boundary.created_at,
                    before,
                )?;
                return Ok(blocked);
            }
            if remaining == 0 {
                return Err(ReadError::Unknown);
            }
            remaining -= 1;
            self.read_requests
                .publish(key, &effective, ReadPhase::Submitting);
            log::debug!(
                "messenger_read op_id={} event=submitting message_id={} attempt={}",
                effective.id,
                effective.boundary.target.message_id,
                SUBMIT_ATTEMPTS - remaining
            );
            let posted = self
                .rest
                .read_up_to(
                    &key.owner,
                    &token,
                    key.scope,
                    effective.boundary.target.message_id,
                    Some(&effective.boundary.created_at),
                )
                .await;
            let confirmed = match posted {
                Ok(None) => true,
                Ok(Some(ticket)) => {
                    self.read_requests.set_receipt(
                        key,
                        Some(HeldRead {
                            intent: effective.clone(),
                            ticket: MutationTicket::new(&key.owner, key.generation, ticket),
                            before,
                        }),
                    );
                    if self.settle_read_receipt(sync, key, &effective).await? {
                        return Ok(blocked);
                    }
                    false
                }
                Err(ReadError::Unknown | ReadError::Http(429 | 500..=599)) => false,
                Err(error) => return Err(error),
            };
            if confirmed {
                check_candidate(sync, key, &effective)?;
                sync.confirm_history_read(
                    &key.owner,
                    key.generation,
                    key.scope,
                    effective.boundary.target.message_id,
                    &effective.boundary.created_at,
                    before,
                )?;
                return Ok(blocked);
            }
            self.read_requests
                .publish(key, &effective, ReadPhase::Reconciling);
            sleep(RETRY_DELAY).await;
        }
    }

    /// Завершает прежнюю квитанцию перед первым или следующим POST этой темы.
    async fn settle_read_receipt(
        &self,
        sync: &Sync,
        key: &ReadKey,
        current: &ReadIntent,
    ) -> Result<bool, ReadError> {
        let Some(held) = self.read_requests.receipt(key) else {
            return Ok(false);
        };
        if held.intent.access_revision != current.access_revision {
            self.read_requests.set_receipt(key, None);
            return Ok(false);
        }
        self.read_requests
            .publish(key, &held.intent, ReadPhase::Confirming);
        let state = self.wait_read_status(sync, key, &held).await?;
        let before = match state {
            ReceiptState::Applied => held.before,
            ReceiptState::Rejected => {
                self.read_requests.set_receipt(key, None);
                return Err(ReadError::Rejected);
            }
            ReceiptState::Waiting | ReceiptState::Unavailable => {
                self.read_requests
                    .publish(key, &held.intent, ReadPhase::Reconciling);
                let before = sync.history_read_revision(&key.owner, key.generation)?;
                let token = sync
                    .auth()
                    .access_token_for_generation(&key.owner, Service::Messenger, key.generation)
                    .await?;
                let oldest = self
                    .rest
                    .unread_page(
                        &key.owner,
                        &token,
                        key.scope,
                        1,
                        Some(&held.intent.boundary.created_at),
                    )
                    .await?
                    .pop();
                check_read_access(sync, key, &held.intent)?;
                if oldest.is_some_and(|row| row.boundary.order() <= held.intent.boundary.order()) {
                    if matches!(state, ReceiptState::Unavailable) {
                        // Квитанции больше нет. Перед повтором обычная проверка подтвердит все UUID префикса.
                        self.read_requests.set_receipt(key, None);
                        return Ok(false);
                    }
                    return Err(ReadError::Confirming);
                }
                self.rest
                    .verify_read_target(&key.owner, &token, &held.intent.boundary)
                    .await?;
                before
            }
        };
        check_read_access(sync, key, &held.intent)?;
        sync.confirm_history_read(
            &key.owner,
            key.generation,
            key.scope,
            held.intent.boundary.target.message_id,
            &held.intent.boundary.created_at,
            before,
        )?;
        self.read_requests.set_receipt(key, None);
        Ok(true)
    }

    /// Ждёт конечный статус с ограничением времени, проверяя исходный допуск между запросами.
    async fn wait_read_status(
        &self,
        sync: &Sync,
        key: &ReadKey,
        held: &HeldRead,
    ) -> Result<ReceiptState, ReadError> {
        let deadline = Instant::now() + RECEIPT_DEADLINE;
        let mut delay = INITIAL_STATUS_DELAY;
        loop {
            sleep(delay.min(deadline.saturating_duration_since(Instant::now()))).await;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(ReceiptState::Waiting);
            }
            check_read_access(sync, key, &held.intent)?;
            match timeout(
                left.min(MAX_STATUS_DELAY),
                self.mutation_status(sync.auth(), &held.ticket),
            )
            .await
            {
                Ok(Ok(MutationStatus::Applied)) => return Ok(ReceiptState::Applied),
                Ok(Ok(MutationStatus::Rejected)) => return Ok(ReceiptState::Rejected),
                Ok(Err(MutationStatusError::Auth(error))) => return Err(error.into()),
                Ok(Err(MutationStatusError::Unavailable)) => return Ok(ReceiptState::Unavailable),
                _ => delay = (delay * 2).min(MAX_STATUS_DELAY),
            }
        }
    }
}

/// Находит наибольшую доказанную границу до первой невидимой или неподдерживаемой строки.
fn observed_prefix(
    unread: Vec<UnreadAnchor>,
    intent: &ReadIntent,
) -> (Option<ReadBoundary>, Option<UnreadAnchor>) {
    let mut last_seen = None;
    for row in unread
        .into_iter()
        .take_while(|row| row.boundary.order() <= intent.boundary.order())
    {
        if !row.eligible || !intent.observed.contains(&row.boundary.target.message_id) {
            return (last_seen, Some(row));
        }
        last_seen = Some(row.boundary);
    }
    (last_seen, None)
}

/// Читает версии отзыва темы и сброса журнала под проверкой действующего входа.
fn access_revision(
    sync: &Sync,
    owner: &OwnerContext,
    generation: SessionGeneration,
    scope: HistoryScope,
) -> Result<i64, SyncError> {
    Ok(sync.auth().with_active_generation(owner, generation, || {
        sync.storage().media_access_revision(owner, scope)
    })??)
}

/// Проверяет, что видимая строка всё ещё находится по исходному адресу и доступ не менялся.
fn check_candidate(sync: &Sync, key: &ReadKey, intent: &ReadIntent) -> Result<(), ReadError> {
    let message = sync
        .history_read_candidate(
            &key.owner,
            key.generation,
            key.scope,
            intent.boundary.target.message_id,
        )?
        .ok_or(ReadError::Unavailable)?;
    if !message.read_eligible() || message.metadata.created_at != intent.boundary.created_at {
        return Err(ReadError::Unavailable);
    }
    check_read_access(sync, key, intent)
}

/// Проверяет исходную сессию и право темы независимо от текущих строк окна.
fn check_read_access(sync: &Sync, key: &ReadKey, intent: &ReadIntent) -> Result<(), ReadError> {
    if access_revision(sync, &key.owner, key.generation, key.scope)? != intent.access_revision {
        return Err(ReadError::Unavailable);
    }
    Ok(())
}

/// Связывает серверную квитанцию с тем же входом, который отправил POST.
fn read_receipt(
    owner: &OwnerContext,
    generation: SessionGeneration,
    id: Option<Uuid>,
) -> MutationReceipt {
    id.map_or(MutationReceipt::Applied, |ticket| {
        MutationReceipt::Accepted(MutationTicket::new(owner, generation, ticket))
    })
}
