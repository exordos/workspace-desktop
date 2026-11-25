//! Связывает проверенную цитату с точным размещением и версией оригинала в истории.

use std::{
    cmp::Ordering,
    collections::HashMap,
    sync::{Arc, RwLock, Weak},
};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    context::{OwnerContext, SessionGeneration},
    storage::{HistoryChange, HistoryMessage, HistoryTime, MessageBody},
};

/// Снимки оригиналов, которые уже проверяли для цитат в каждом входе.
type Registry = RwLock<HashMap<OwnerContext, OwnerOrigins>>;

/// Хранит по одной проверенной версии на публичный UUID оригинала.
#[derive(Default)]
pub(super) struct QuoteOrigins(Arc<Registry>); // Таблица текущих версий оригиналов.

/// Версии оригиналов в одном поколении входа.
struct OwnerOrigins {
    generation: SessionGeneration,           // Поколение, принявшее снимки.
    messages: HashMap<Uuid, ObservedOrigin>, // Последнее известное состояние каждого UUID.
}

/// Последняя однозначная версия и состояние её разрешения.
struct ObservedOrigin {
    snapshot: QuoteOrigin, // Снимок для проверки адреса и порядка обновлений.
    status: OriginStatus,  // Разрешение или причина ожидания новой версии.
}

/// Указывает, какой ответ может снова разрешить чтение.
enum OriginStatus {
    Current(Weak<()>), // Текущая версия, даже если на неё сейчас нет ссылок.
    Conflict(Option<HistoryTime>), // Только более позднее известное время; None ждёт записи истории.
}

/// Отзываемое разрешение прочитать ровно одну версию исходника.
#[derive(Clone)]
pub(crate) struct QuoteOriginPermit {
    registry: Weak<Registry>,      // Текущие версии без удержания синхронизации.
    owner: OwnerContext,           // Владелец оригинала.
    generation: SessionGeneration, // Вход, который проверил оригинал.
    message: Uuid,                 // Публичный UUID исходного сообщения.
    token: Arc<()>,                // Идентификатор принятой версии.
}

/// Версия оригинала для реестра отображения. Показываемый открытый текст в реестр не попадает.
#[derive(Eq, PartialEq)]
pub(super) struct QuoteOrigin {
    pub id: Uuid,                    // Публичный UUID исходного сообщения.
    stream: Uuid,                    // Точный стрим после авторитетного чтения.
    topic: Uuid,                     // Точная тема после авторитетного чтения.
    updated_at: Option<HistoryTime>, // Серверное время правки, если оно известно.
    stamp: [u8; 32],                 // Хэш тела и метаданных исходного снимка.
}

impl QuoteOrigins {
    /// Принимает подтверждённый REST-снимок и отмечает, нужно ли скрыть прежние цитаты.
    pub(super) fn capture(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        snapshot: QuoteOrigin,
    ) -> (Option<QuoteOriginPermit>, bool) {
        let mut registry = self.0.write().unwrap();
        let current = registry
            .entry(owner.clone())
            .or_insert_with(|| OwnerOrigins {
                generation,
                messages: HashMap::new(),
            });
        if current.generation != generation {
            current.generation = generation;
            current.messages.clear();
        }
        let message = snapshot.id;
        let (token, revoked) = match current.messages.entry(message) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                let token = Arc::new(());
                slot.insert(ObservedOrigin {
                    snapshot,
                    status: OriginStatus::Current(Arc::downgrade(&token)),
                });
                (Some(token), false)
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                slot.get_mut().observe(snapshot)
            }
        };
        let permit = token.map(|token| QuoteOriginPermit {
            registry: Arc::downgrade(&self.0),
            owner: owner.clone(),
            generation,
            message,
            token,
        });
        (permit, revoked)
    }

    /// Согласует затронутые версии только с успешной записью истории.
    pub(super) fn commit_matching<T, E>(
        &self,
        owner: &OwnerContext,
        write: impl FnOnce() -> Result<T, E>,
        mut revoked: impl FnMut(&QuoteOrigin, bool, &T) -> bool,
    ) -> Result<T, E> {
        let mut registry = self.0.write().unwrap();
        let result = write()?;
        if let Some(current) = registry.get_mut(owner) {
            current.messages.retain(|_, origin| {
                !revoked(
                    &origin.snapshot,
                    matches!(origin.status, OriginStatus::Conflict(_)),
                    &result,
                )
            });
        }
        Ok(result)
    }
}

impl ObservedOrigin {
    /// Оставляет один действующий снимок, а при спорном порядке закрывает текущий оригинал.
    fn observe(&mut self, snapshot: QuoteOrigin) -> (Option<Arc<()>>, bool) {
        let revoked = match &mut self.status {
            OriginStatus::Current(version) if self.snapshot == snapshot => {
                let token = version.upgrade().unwrap_or_else(|| {
                    let token = Arc::new(());
                    *version = Arc::downgrade(&token);
                    token
                });
                return (Some(token), false);
            }
            OriginStatus::Current(version) => {
                let order = snapshot.compare_time(&self.snapshot);
                let revoked = version.strong_count() != 0;
                match order {
                    Some(Ordering::Less) => return (None, false),
                    Some(Ordering::Greater) => revoked,
                    _ => {
                        // При одинаковом известном времени восстановит чтение лишь
                        // строго более поздняя правка. Иначе ждём записи истории.
                        self.status = OriginStatus::Conflict(
                            snapshot
                                .updated_at
                                .filter(|_| order == Some(Ordering::Equal)),
                        );
                        return (None, revoked);
                    }
                }
            }
            OriginStatus::Conflict(Some(since))
                if snapshot.same_placement(&self.snapshot)
                    && snapshot
                        .updated_at
                        .as_ref()
                        .is_some_and(|time| time > since) =>
            {
                false
            }
            OriginStatus::Conflict(_) => return (None, false),
        };
        let token = Arc::new(());
        self.snapshot = snapshot;
        self.status = OriginStatus::Current(Arc::downgrade(&token));
        (Some(token), revoked)
    }
}

impl QuoteOriginPermit {
    /// Возвращает исходный снимок, пока его версия остаётся в реестре.
    fn current<'a>(&self, owner: &'a OwnerOrigins) -> Option<&'a QuoteOrigin> {
        let origin = owner.messages.get(&self.message)?;
        let OriginStatus::Current(current) = &origin.status else {
            return None;
        };
        (owner.generation == self.generation && current.ptr_eq(&Arc::downgrade(&self.token)))
            .then_some(&origin.snapshot)
    }

    /// Проверяет, что повторный ответ об оригинале совпал с версией ссылки.
    pub(crate) fn matches_message(&self, message: &HistoryMessage) -> bool {
        let expected = QuoteOrigin::from_message(message);
        let Some(registry) = self.registry.upgrade() else {
            return false;
        };
        let versions = registry.read().unwrap();
        versions
            .get(&self.owner)
            .and_then(|owner| self.current(owner))
            .is_some_and(|snapshot| *snapshot == expected)
    }

    /// Одной блокировкой удерживает исходник и цитирующую строку до завершения чтения.
    pub(crate) fn with_linked<T>(&self, other: &Self, read: impl FnOnce() -> T) -> Option<T> {
        if !self.registry.ptr_eq(&other.registry) || self.owner != other.owner {
            return None;
        }
        let registry = self.registry.upgrade()?;
        let versions = registry.read().unwrap();
        let owner = versions.get(&self.owner)?;
        self.current(owner)?;
        other.current(owner)?;
        Some(read())
    }

    /// Удерживает версию одного исходника на короткое чтение.
    pub(crate) fn with_current<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        self.with_linked(self, read)
    }
}

impl QuoteOrigin {
    /// Вычисляет неизменяемый идентификатор версии, включая исходный UUID ключа.
    pub(super) fn from_message(message: &HistoryMessage) -> Self {
        let mut stamp = Sha256::new();
        stamp.update(message.metadata.author_id.unwrap_or(Uuid::nil()).as_bytes());
        stamp.update(message.metadata.created_at.as_str().as_bytes());
        if let Some(time) = &message.metadata.updated_at {
            stamp.update(time.as_str().as_bytes());
        }
        stamp.update([0]);
        match &message.body {
            MessageBody::Markdown(text) => {
                stamp.update([1]);
                stamp.update(text.as_bytes());
            }
            MessageBody::Encrypted { key_id, ciphertext } => {
                stamp.update([2]);
                stamp.update(key_id.as_bytes());
                stamp.update(ciphertext.as_bytes());
            }
            MessageBody::KeyService { .. } => stamp.update([3]),
        }
        Self {
            id: message.id,
            stream: message.stream_id,
            topic: message.topic_id,
            updated_at: message.metadata.updated_at.clone(),
            stamp: stamp.finalize().into(),
        }
    }

    /// Проверяет точный стрим и тему оригинала.
    fn same_placement(&self, other: &Self) -> bool {
        self.stream == other.stream && self.topic == other.topic
    }

    /// Сравнивает серверные времена правки, если оба известны для того же размещения.
    pub(super) fn compare_time(&self, other: &Self) -> Option<Ordering> {
        let (current, previous) = (self.updated_at.as_ref()?, other.updated_at.as_ref()?);
        self.same_placement(other).then(|| current.cmp(previous))
    }

    /// Находит исходник, затронутый авторитетной правкой, удалением или отзывом области.
    pub(super) fn affected_by(&self, change: &HistoryChange) -> bool {
        match change {
            HistoryChange::Edit { message, .. } => {
                self.id == message.id.parse().unwrap_or(Uuid::nil())
            }
            HistoryChange::RemoveMessage(id) => self.id == id.parse().unwrap_or(Uuid::nil()),
            HistoryChange::RemoveStream(id) => self.stream == id.parse().unwrap_or(Uuid::nil()),
            HistoryChange::RemoveTopic(scope) => {
                self.stream == scope.stream_id.parse().unwrap_or(Uuid::nil())
                    && self.topic == scope.topic_id.parse().unwrap_or(Uuid::nil())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{context::AccountId, storage::HistoryMetadata};

    /// Создаёт владельца синтетических снимков без сети и хранилища.
    fn owner(account: &str) -> OwnerContext {
        OwnerContext {
            account_id: AccountId(account.into()),
            server: "https://workspace.test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        }
    }

    /// Создаёт обычный снимок с известным либо отсутствующим временем правки.
    fn snapshot(id: Uuid, body: &str, updated: Option<&str>) -> QuoteOrigin {
        QuoteOrigin::from_message(&HistoryMessage {
            read: None,
            reactions: Vec::new(),
            id,
            stream_id: Uuid::from_u128(1),
            topic_id: Uuid::from_u128(2),
            body: MessageBody::Markdown(body.into()),
            metadata: HistoryMetadata {
                native_source: None,
                author_id: Some(Uuid::from_u128(3)),
                created_at: HistoryTime::parse("2026-01-01T12:00:00Z").unwrap(),
                updated_at: updated.map(|time| HistoryTime::parse(time).unwrap()),
            },
        })
    }

    /// Равные версии конфликтуют, а более поздняя правка восстанавливает только нужный оригинал.
    #[test]
    fn equal_time_conflict_expires_only_that_owner_and_source() {
        let origins = QuoteOrigins::default();
        let owner_a = owner("a");
        let owner_b = owner("b");
        let generation = SessionGeneration(1);
        let source = Uuid::from_u128(4);
        let neighbor = Uuid::from_u128(5);
        let first = Some("2026-01-01T12:01:00Z");
        let later = Some("2026-01-01T12:02:00Z");
        let capture =
            |owner, id, text, time| origins.capture(owner, generation, snapshot(id, text, time));
        let old = capture(&owner_a, source, "first", first).0.unwrap();
        let other_owner = capture(&owner_b, source, "first", first).0.unwrap();
        let other_source = capture(&owner_a, neighbor, "first", first).0.unwrap();

        let (conflicting, revoked) = capture(&owner_a, source, "second", first);
        assert!(conflicting.is_none() && revoked);
        assert!(old.with_current(|| ()).is_none());
        assert!(capture(&owner_a, source, "first", first).0.is_none());
        assert!(capture(&owner_a, source, "second", first).0.is_none());

        let fresh = capture(&owner_a, source, "third", later).0.unwrap();
        assert!(fresh.with_current(|| ()).is_some());
        assert!(old.with_current(|| ()).is_none());
        assert!(other_owner.with_current(|| ()).is_some());
        assert!(other_source.with_current(|| ()).is_some());
    }

    /// Снимок без сравнимого времени ждёт успешной записи истории, а не повтора REST.
    #[test]
    fn unordered_conflict_waits_for_committed_history() {
        let origins = QuoteOrigins::default();
        let owner = owner("a");
        let generation = SessionGeneration(1);
        let id = Uuid::from_u128(4);
        let later = Some("2026-01-01T12:02:00Z");
        let capture = |text, time| origins.capture(&owner, generation, snapshot(id, text, time));
        let old = capture("first", None).0.unwrap();
        let (conflicting, revoked) = capture("second", later);
        assert!(conflicting.is_none() && revoked);
        assert!(capture("second", later).0.is_none());
        assert!(old.with_current(|| ()).is_none());

        origins
            .commit_matching(
                &owner,
                || Ok::<_, ()>(()),
                |origin, conflict, _| origin.id == id && conflict,
            )
            .unwrap();
        let fresh = capture("second", later).0.unwrap();
        assert!(fresh.with_current(|| ()).is_some());
        assert!(old.with_current(|| ()).is_none());
    }
}
