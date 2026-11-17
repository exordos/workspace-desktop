//! Отправляет очередь владельца, пока его поток событий подключён.
//! Здесь закрепляется первый POST и проверяются неизвестные исходы через GET.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use tokio::{
    sync::{broadcast, watch},
    time::{Instant, sleep_until},
};
use uuid::Uuid;

use crate::{
    auth::{Service, SessionSnapshot},
    context::{OwnerContext, SessionGeneration},
    runtime::SourceError,
    storage::{DeliveryStatus, PreparedSend, SendBlock},
    sync::{Sync, SyncChange, SyncError},
};

use super::{
    MessengerSource,
    send::{DirectSendError, SendError},
};

const CHECKS_PER_CONNECTION: u8 = 3;
const FIRST_CHECK_DELAY: Duration = Duration::from_millis(500);
const PREFLIGHT_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Следит за очередью одного активного входа. Runtime отменяет её вместе с потоком.
pub(super) async fn run(
    source: &MessengerSource,
    sync: Arc<Sync>,
    session: SessionSnapshot,
    mut ready: watch::Receiver<bool>,
) {
    let owner = &session.context;
    let generation = session.generation;
    let mut updates = sync.subscribe();
    let mut worker = OutboxWorker {
        source,
        sync: &sync,
        session: &session,
        checks: HashMap::new(),
        preflight_retries: HashMap::new(),
    };
    loop {
        if ready.has_changed().unwrap_or(false) {
            worker.reset_retries();
        }
        let online = *ready.borrow_and_update();
        let next_wake = if online {
            match worker.process(&ready).await {
                Ok(next) => next,
                Err(SyncError::Storage(error)) => {
                    log::warn!("Не удалось прочитать или обновить очередь: {error}");
                    Some(Instant::now() + PREFLIGHT_RETRY_DELAY)
                }
                // Если сменилось поколение, Runtime вскоре остановит эту задачу.
                Err(SyncError::Auth(_)) => Some(Instant::now() + PREFLIGHT_RETRY_DELAY),
            }
        } else {
            // Незавершённый POST при смене токена или перезапуске остаётся неизвестным и без сети.
            let _ = sync.recover_sends(owner, generation);
            None
        };

        tokio::select! {
            changed = ready.changed() => {
                if changed.is_err() {
                    return;
                }
                worker.reset_retries();
            }
            changed = wait_for_outbox(&mut updates, owner, generation) => if !changed { return; },
            () = async { sleep_until(next_wake.expect("есть срок проверки")).await }, if next_wake.is_some() => {}
        }
    }
}

/// Будит задачу для своей очереди или после пропущенных событий подписки.
async fn wait_for_outbox(
    updates: &mut broadcast::Receiver<crate::sync::SyncUpdate>,
    owner: &OwnerContext,
    generation: SessionGeneration,
) -> bool {
    loop {
        match updates.recv().await {
            Ok(update)
                if update.context == *owner
                    && update.generation == generation
                    && matches!(update.change, SyncChange::OutboxChanged { .. }) =>
            {
                return true;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => return true,
            Err(broadcast::error::RecvError::Closed) => return false,
            Ok(_) => {}
        }
    }
}

/// Обрабатывает один аккаунт последовательно, сохраняя паузы для повторных чтений.
struct OutboxWorker<'a> {
    source: &'a MessengerSource,  // HTTP-запросы мессенджера.
    sync: &'a Sync,               // Запись только в действующем поколении.
    session: &'a SessionSnapshot, // Владелец очереди и версия токена потока.
    checks: HashMap<Uuid, Check>, // Бюджет GET для публичных UUID в текущем соединении.
    preflight_retries: HashMap<(Uuid, Uuid), Instant>, // Пауза проверки исходных стрима и темы перед POST.
}

/// Хранит число неуспешных сверок и время следующего GET.
struct Check {
    attempted: u8, // Число чтений с момента последнего переподключения.
    next: Instant, // Когда разрешено следующее чтение по публичному UUID.
}

impl OutboxWorker<'_> {
    /// После переподключения можно снова проверить адреса и неизвестные исходы.
    fn reset_retries(&mut self) {
        self.checks.clear();
        self.preflight_retries.clear();
    }

    /// Обрабатывает готовые записи и находит ближайшую следующую попытку.
    async fn process(
        &mut self,
        ready: &watch::Receiver<bool>,
    ) -> Result<Option<Instant>, SyncError> {
        let owner = &self.session.context;
        let generation = self.session.generation;
        // Если запись результата прервалась после захвата, следующий проход уже сверяет её.
        self.sync.recover_sends(owner, generation)?;
        let entries = self.sync.read_outbox(owner, generation, None)?;
        let unresolved: HashSet<_> = entries
            .iter()
            .filter(|entry| entry.status == DeliveryStatus::Unknown)
            .map(|entry| entry.snapshot.public_id)
            .collect();
        self.checks.retain(|id, _| unresolved.contains(id));
        let now = Instant::now();
        self.preflight_retries.retain(|_, retry_at| *retry_at > now);

        for entry in entries {
            if !*ready.borrow() {
                break;
            }
            let now = Instant::now();
            match entry.status {
                DeliveryStatus::Unknown => self.reconcile(&entry.snapshot, now).await?,
                DeliveryStatus::Waiting => {
                    let address = (entry.snapshot.stream_id, entry.snapshot.topic_id);
                    if self
                        .preflight_retries
                        .get(&address)
                        .is_some_and(|retry_at| *retry_at > now)
                    {
                        continue;
                    }
                    self.preflight_retries.remove(&address);
                    if !self.attempt(&entry.snapshot, ready).await? {
                        self.preflight_retries
                            .insert(address, Instant::now() + PREFLIGHT_RETRY_DELAY);
                    }
                }
                DeliveryStatus::Sending
                | DeliveryStatus::Blocked(_)
                | DeliveryStatus::Rejected(_) => {}
            }
        }
        Ok(self
            .checks
            .values()
            .filter(|check| check.attempted < CHECKS_PER_CONNECTION)
            .map(|check| check.next)
            .chain(self.preflight_retries.values().copied())
            .min())
    }

    /// Проверяет адрес и получает право на один POST. Временный сбой оставляет запись в очереди.
    async fn attempt(
        &self,
        snapshot: &PreparedSend,
        ready: &watch::Receiver<bool>,
    ) -> Result<bool, SyncError> {
        let owner = &self.session.context;
        let generation = self.session.generation;
        let Some(token) = self.token().await else {
            return Ok(false);
        };
        let preflight = self
            .source
            .rest
            .direct_send_topic(owner, &token, snapshot.stream_id, Some(snapshot.topic_id))
            .await;
        let blocked = match preflight {
            Ok(_) => None,
            Err(DirectSendError::Source(SourceError::Unauthorized)) => {
                self.sync
                    .auth()
                    .reject_access_token(owner, generation, self.session.token_version);
                return Ok(false);
            }
            Err(DirectSendError::Source(SourceError::Temporary) | DirectSendError::Auth(_)) => {
                return Ok(false);
            }
            Err(DirectSendError::AccessLost | DirectSendError::TopicUnavailable) => {
                Some(DeliveryStatus::Blocked(SendBlock::AccessLost))
            }
            Err(DirectSendError::EncryptionUnavailable) => {
                Some(DeliveryStatus::Blocked(SendBlock::EncryptionUnavailable))
            }
            Err(DirectSendError::Source(_)) => Some(DeliveryStatus::Rejected(None)),
        };
        if let Some(status) = blocked {
            self.sync
                .set_send_status(owner, generation, snapshot, false, status)?;
            return Ok(true);
        }
        if !*ready.borrow() {
            return Ok(false);
        }
        let Some(claimed) = self
            .sync
            .claim_send(owner, generation, snapshot.canonical_id)?
        else {
            return Ok(true);
        };
        self.sync
            .auth()
            .with_active_generation(owner, generation, || ())?;
        let status = match self
            .source
            .post_prepared_message(owner, &token, &claimed)
            .await
        {
            Ok(message) if confirms_snapshot(owner, &claimed, &message) => {
                self.sync.confirm_http_send(owner, generation, &message)?;
                return Ok(true);
            }
            Err(SendError::Rejected(code)) => DeliveryStatus::Rejected(Some(code)),
            Err(SendError::InvalidInput) => DeliveryStatus::Rejected(None),
            Ok(_) | Err(SendError::Unknown) => DeliveryStatus::Unknown,
        };
        self.sync
            .set_send_status(owner, generation, &claimed, true, status)?;
        Ok(true)
    }

    /// Даёт журналу время на подтверждение и при необходимости ищет сообщение по UUID.
    async fn reconcile(&mut self, snapshot: &PreparedSend, now: Instant) -> Result<(), SyncError> {
        let check = self.checks.entry(snapshot.public_id).or_insert(Check {
            attempted: 0,
            next: now + FIRST_CHECK_DELAY,
        });
        if now < check.next || check.attempted >= CHECKS_PER_CONNECTION {
            return Ok(());
        }
        check.attempted += 1;
        check.next = now + FIRST_CHECK_DELAY * (1 << check.attempted);
        let Some(token) = self.token().await else {
            return Ok(());
        };
        let owner = &self.session.context;
        if let Ok(message) = self
            .source
            .lookup_prepared_message(owner, &token, snapshot)
            .await
        {
            self.sync
                .confirm_http_send(owner, self.session.generation, &message)?;
            self.checks.remove(&snapshot.public_id);
        }
        Ok(())
    }

    /// Получает токен именно того владельца и версии, с которой подключён источник.
    async fn token(&self) -> Option<String> {
        let auth = self.sync.auth();
        let token = auth
            .access_token_for_generation(
                &self.session.context,
                Service::Messenger,
                self.session.generation,
            )
            .await
            .ok()?;
        auth.snapshot()
            .into_iter()
            .any(|session| {
                session.context == self.session.context
                    && session.generation == self.session.generation
                    && session.token_version == self.session.token_version
            })
            .then_some(token)
    }
}

/// Проверяет, что успешный POST вернул исходный маршрут, UUID и автора.
fn confirms_snapshot(
    owner: &OwnerContext,
    snapshot: &PreparedSend,
    message: &crate::storage::HistoryMessage,
) -> bool {
    message.id == snapshot.public_id
        && message.stream_id == snapshot.stream_id
        && message.topic_id == snapshot.topic_id
        && message.metadata.author_id == owner.user_id.parse().ok()
        && message.metadata.author_id.is_some()
}
