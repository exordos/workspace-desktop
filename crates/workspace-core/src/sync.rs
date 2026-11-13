//! Связывает получение событий с локальной базой.
//! Проверяет действующую сессию перед записью сообщения или позиции чтения.

use std::sync::Arc;

use thiserror::Error;

use crate::{
    auth::{Auth, AuthError},
    context::{OwnerContext, SessionGeneration},
    storage::{Cursor, Message, Storage, StorageError},
};

pub use crate::storage::ApplyOutcome;

/// Изменение, которое источник передаёт для сохранения.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    MessageCreated {
        cursor: Cursor,   // Позиция, которую сохраняем вместе с сообщением.
        message: Message, // Новое сообщение для локальной базы.
    },
    Checkpoint(Cursor), // Новая позиция чтения без сообщения.
}

impl Event {
    /// Возвращает позицию, до которой нужно продвинуть чтение после записи.
    pub fn cursor(&self) -> &Cursor {
        match self {
            Self::MessageCreated { cursor, .. } | Self::Checkpoint(cursor) => cursor,
        }
    }

    /// Возвращает сообщение, если оно есть в событии.
    pub fn message(&self) -> Option<&Message> {
        match self {
            Self::MessageCreated { message, .. } => Some(message),
            Self::Checkpoint(_) => None,
        }
    }
}

/// Причина, по которой событие не удалось обработать.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Ошибка проверки действующей сессии.
    #[error(transparent)]
    Storage(#[from] StorageError), // Ошибка локального хранилища.
}

/// Проверяет право сессии на запись и передаёт события в хранилище.
pub struct Sync {
    auth: Auth,            // Авторизация для проверки владельца и поколения сессии.
    storage: Arc<Storage>, // Общая локальная база сообщений и позиций.
}

impl Sync {
    /// Создаёт синхронизацию с указанной авторизацией и базой.
    pub fn new(auth: Auth, storage: Arc<Storage>) -> Self {
        Self { auth, storage }
    }

    /// Даёт фоновой задаче доступ к авторизации и состояниям сессий.
    pub(crate) fn auth(&self) -> &Auth {
        &self.auth
    }

    /// Возвращает последнюю записанную позицию источника для владельца.
    pub fn cursor(&self, owner: &OwnerContext, source: &str) -> Result<Option<Cursor>, SyncError> {
        Ok(self.storage.cursor(owner, source)?)
    }

    /// Записывает событие, пока поколение сессии остаётся действующим.
    /// Проверка сессии и запись защищены одной блокировкой аккаунта.
    pub fn apply(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        source: &str,
        event: Event,
    ) -> Result<ApplyOutcome, SyncError> {
        let outcome = self.auth.with_active_generation(owner, generation, || {
            self.storage
                .apply(owner, source, event.cursor(), event.message())
        })??;
        Ok(outcome)
    }
}
