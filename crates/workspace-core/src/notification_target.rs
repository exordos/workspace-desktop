//! Описывает сообщение, к которому ведёт системное уведомление, и причины отказа при проверке цели.

use thiserror::Error;

use crate::{auth::AuthError, context::OwnerContext, storage::StorageError};

/// Адрес конкретного сообщения в контексте аккаунта, получившего уведомление.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationTarget {
    pub context: OwnerContext, // Аккаунт и проект, которым принадлежит уведомление.
    pub conversation_id: String, // Идентификатор разговора, подтверждённый при синхронизации.
    pub message_id: String,    // Идентификатор сообщения, о котором пришло уведомление.
}

/// Причина, по которой ядро не смогло подтвердить цель уведомления.
#[derive(Debug, Error)]
pub enum NotificationTargetError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Аккаунт недоступен или поколение сессии уже сменилось.
    #[error(transparent)]
    Storage(#[from] StorageError), // Сбой чтения локальной базы.
    #[error("the notification target was not found")]
    UnknownTarget, // В локальной базе нет сообщения с указанным контекстом и разговором.
}
