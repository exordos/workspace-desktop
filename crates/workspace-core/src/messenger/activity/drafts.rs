//! Читает черновики владельца и возвращает точный снимок для списка и редактора.

use uuid::Uuid;

use crate::{
    auth::{AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    runtime::SourceError,
    storage::{
        DraftAccess, DraftListEntry, DraftScope, DraftVersion, HistoryTime, PlaintextDraft,
        StorageError,
    },
    sync::{Sync, SyncError},
};

use super::super::{MessengerSource, catalog::MessagePreview};

/// Карточка черновика с переводимыми служебными частями превью.
pub type ActivityDraftEntry = DraftListEntry<MessagePreview>;

/// Причина, по которой не завершилась серверная сверка черновика.
#[derive(Debug, thiserror::Error)]
pub enum ServerDraftError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Вход завершился, изменился или не выдал токен.
    #[error(transparent)]
    Source(#[from] SourceError), // Сервер не вернул подходящий снимок.
    #[error(transparent)]
    Storage(#[from] StorageError), // Не удалось безопасно сверить локальную версию.
}

/// Проверенный снимок обычного черновика с сервера.
pub(in crate::messenger) struct RemoteDraft {
    pub id: Uuid, // UUID сервера, сохраняемый при открытии и локальном редактировании.
    pub scope: DraftScope, // Подтверждённое размещение в проекте.
    pub revision: u64, // Номер серверной версии, не сравнивается с локальной версией.
    pub updated_at: HistoryTime, // Время последней серверной правки.
    pub markdown: String, // Обычный текст.
}

impl MessengerSource {
    /// Читает черновики сервера, чьи UUID ещё не заняты локальным вводом или отправкой.
    pub async fn list_server_drafts(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<Vec<ActivityDraftEntry>, ServerDraftError> {
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self.rest.server_draft_collection(owner, &token).await;
        let mut claimed = auth.with_active_generation(owner, generation, || {
            sync.storage().claimed_draft_ids(owner)
        })??;
        let mut drafts = result?;
        drafts.sort_by(|first, second| {
            (&second.updated_at, second.revision, second.id).cmp(&(
                &first.updated_at,
                first.revision,
                first.id,
            ))
        });
        Ok(drafts
            .into_iter()
            .filter(|draft| claimed.insert(draft.id))
            .map(|draft| DraftListEntry {
                scope: draft.scope,
                version: DraftVersion {
                    id: draft.id,
                    version: draft.revision,
                },
                preview: MessagePreview::short(&draft.markdown, 160),
                updated_at: Some(draft.updated_at),
                from_server: true,
            })
            .collect())
    }

    /// Открывает точный серверный UUID, если локальная запись не появилась или не была очищена.
    pub async fn open_server_draft(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        access: &DraftAccess,
        id: Uuid,
    ) -> Result<Option<PlaintextDraft>, ServerDraftError> {
        if !access.belongs_to(owner) {
            return Err(StorageError::StaleDraftAccess.into());
        }
        let auth = sync.auth();
        let (local, claimed) = auth
            .with_active_generation(owner, generation, || local_draft(sync, owner, access, id))??;
        if claimed {
            return Ok(local);
        }
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let fetched = self
            .rest
            .server_draft(owner, &token, access.scope(), id)
            .await;
        auth.with_active_generation(owner, generation, || {
            // Повторное чтение проверяет ревизию доступа и локальный ввод, появившийся во время GET.
            let (latest, claimed) = local_draft(sync, owner, access, id)?;
            if claimed {
                return Ok(latest);
            }
            Ok(fetched?.map(|draft| PlaintextDraft {
                id: draft.id,
                version: 0,
                markdown: draft.markdown,
                replies: None,
            }))
        })?
    }

    /// Удаляет серверный снимок, только если его версия и область ещё совпадают с карточкой.
    pub async fn delete_server_draft(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: DraftScope,
        version: DraftVersion,
    ) -> Result<bool, ServerDraftError> {
        let auth = sync.auth();
        let access = auth.with_active_generation(owner, generation, || {
            sync.storage().prepare_draft_access(owner, scope)
        })??;
        let locally_claimed = || {
            auth.with_active_generation(owner, generation, || {
                local_draft(sync, owner, &access, version.id).map(|(_, claimed)| claimed)
            })
        };
        if locally_claimed()?? {
            return Ok(false);
        }
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self
            .rest
            .server_draft(owner, &token, scope, version.id)
            .await;
        if locally_claimed()?? {
            return Ok(false);
        }
        let Some(confirmed) = result? else {
            return Ok(true);
        };
        if confirmed.revision != version.version {
            return Ok(false);
        }
        let result = self
            .rest
            .delete_server_draft(owner, &token, &confirmed)
            .await;
        auth.with_active_generation(owner, generation, || ())?;
        Ok(result?)
    }
}

/// Читает точный снимок и отличает незанятый UUID от удалённой локальной записи.
fn local_draft(
    sync: &Sync,
    owner: &OwnerContext,
    access: &DraftAccess,
    id: Uuid,
) -> Result<(Option<PlaintextDraft>, bool), StorageError> {
    let local = sync.storage().read_plaintext_draft(access, Some(id))?;
    let claimed = local.is_some() || sync.storage().draft_id_claimed(owner, id)?;
    Ok((local, claimed))
}

impl Sync {
    /// Читает локальные черновики владельца, включая время временной потери сети.
    pub fn list_local_drafts(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<Vec<DraftListEntry>, SyncError> {
        Ok(self
            .auth()
            .with_confirmed_generation(owner, generation, || {
                self.storage().list_plaintext_drafts(owner)
            })??)
    }

    /// Удаляет ровно ту локальную версию, которую пользователь видел при выборе.
    pub fn delete_local_draft(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: DraftScope,
        version: DraftVersion,
    ) -> Result<bool, SyncError> {
        Ok(self
            .auth()
            .with_confirmed_generation(owner, generation, || {
                self.storage().delete_plaintext_draft(owner, scope, version)
            })??)
    }
}
