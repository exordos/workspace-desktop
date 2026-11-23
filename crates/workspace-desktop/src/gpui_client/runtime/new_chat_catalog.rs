//! Передаёт чтение и восстановление архива из окна в Core выбранного входа.

use uuid::Uuid;
use workspace_core::messenger::catalog::{CatalogError, NewChatCatalog};

use super::{AccountSession, RuntimeClient};

/// Причина, которую окно может показать рядом с действием.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::gpui_client) enum NewChatCatalogError {
    Runtime,            // Runtime остановлен или занят.
    Catalog,            // Core не смог подтвердить чтение или запись.
    RestoreUnconfirmed, // При повторе заново проверяем признак архива.
}

impl From<CatalogError> for NewChatCatalogError {
    /// Сохраняет неопределённый исход восстановления для повторной проверки.
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::RestoreUnconfirmed => Self::RestoreUnconfirmed,
            _ => Self::Catalog,
        }
    }
}

impl RuntimeClient {
    /// Загружает активные и архивные разговоры для выбранного входа.
    pub(in crate::gpui_client) async fn new_chat_catalog(
        &self,
        session: AccountSession,
    ) -> Result<NewChatCatalog, NewChatCatalogError> {
        let result = self
            .request(move |services| async move {
                services
                    .messenger
                    .new_chat_catalog(&services.sync, &session.owner, session.generation)
                    .await
            })
            .await
            .map_err(|_| NewChatCatalogError::Runtime)?;
        result.map_err(NewChatCatalogError::from)
    }

    /// Восстанавливает доступный выбранному входу разговор по UUID.
    pub(in crate::gpui_client) async fn restore_archived_chat(
        &self,
        session: AccountSession,
        stream: Uuid,
    ) -> Result<(), NewChatCatalogError> {
        let reply = self
            .enqueue(move |services, reply| async move {
                if reply.is_closed() {
                    return;
                }
                // Даже после закрытия панели подтверждённое изменение нужно сверить с каталогом.
                let result = services
                    .messenger
                    .restore_archived_chat(
                        &services.sync,
                        &session.owner,
                        session.generation,
                        stream,
                    )
                    .await;
                let _ = reply.send(result);
            })
            .map_err(|_| NewChatCatalogError::Runtime)?;
        reply
            .await
            .map_err(|_| NewChatCatalogError::Runtime)?
            .map_err(NewChatCatalogError::from)
    }
}
