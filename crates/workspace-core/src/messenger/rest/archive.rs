//! Читает состояние архива и восстанавливает чат одним запросом Workspace.

use reqwest::{StatusCode, header::ACCEPT};
use serde::Deserialize;
use uuid::Uuid;

use crate::{context::OwnerContext, messenger::catalog::CatalogError, runtime::SourceError};

use super::{
    RestClient, STREAMS_PATH, check_explicit_owner, check_status, request_error, workspace_origin,
};

/// Часть ответа Workspace для проверки цели и результата восстановления.
#[derive(Deserialize)]
struct ArchiveState {
    uuid: Uuid,         // UUID разговора из запроса.
    project_id: String, // Проект, где сервер нашёл разговор.
    user_uuid: String,  // Пользователь, которому сервер вернул разговор.
    is_archived: bool,  // Текущий, явно подтверждённый признак архива.
}

impl ArchiveState {
    /// Проверяет, что ответ относится к запрошенному разговору текущего входа.
    fn archived(self, owner: &OwnerContext, stream: Uuid) -> Result<bool, SourceError> {
        check_explicit_owner(owner, Some(&self.project_id), Some(&self.user_uuid))?;
        if self.uuid != stream {
            return Err(SourceError::InvalidResponse);
        }
        Ok(self.is_archived)
    }
}

impl RestClient {
    /// Проверяет доступ и берёт признак архива непосредственно со стрима.
    pub(in crate::messenger) async fn chat_is_archived(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream: Uuid,
    ) -> Result<bool, CatalogError> {
        let response = self
            .request(owner, token, &format!("{STREAMS_PATH}/{stream}"))?
            .send()
            .await
            .map_err(request_error)?;
        if matches!(
            response.status(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ) {
            return Err(CatalogError::RestoreRejected(response.status().as_u16()));
        }
        check_status(response.status())?;
        let state: ArchiveState = response.json().await.map_err(request_error)?;
        Ok(state.archived(owner, stream)?)
    }

    /// Отправляет восстановление без автоматического повтора и сверяет серверный ответ.
    pub(in crate::messenger) async fn restore_chat(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream: Uuid,
    ) -> Result<(), CatalogError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(&format!("{STREAMS_PATH}/{stream}/actions/unarchive/invoke"));
        let response = self
            .send
            .post(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| CatalogError::RestoreUnconfirmed)?;
        if response.status().is_client_error() {
            return Err(CatalogError::RestoreRejected(response.status().as_u16()));
        }
        if response.status() != StatusCode::OK {
            return Err(CatalogError::RestoreUnconfirmed);
        }
        let state: ArchiveState = response
            .json()
            .await
            .map_err(|_| CatalogError::RestoreUnconfirmed)?;
        match state.archived(owner, stream) {
            Ok(false) => Ok(()),
            _ => Err(CatalogError::RestoreUnconfirmed),
        }
    }
}
