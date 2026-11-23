//! Сверяет и удаляет прежний обычный черновик по адресу и ETag.

use reqwest::{
    StatusCode,
    header::{ACCEPT, ETAG, IF_MATCH},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{context::OwnerContext, runtime::SourceError, storage::DraftScope};

use super::{RestClient, check_explicit_owner, check_status, request_error, workspace_origin};

const DRAFTS_PATH: &str = "/api/workspace/v1/messenger/drafts/";

/// Только координаты и номер версии из ответа, остальные поля не читаем в модель.
#[derive(Deserialize)]
struct WireDraftAddress {
    uuid: Uuid,         // Ранее подтверждённый UUID черновика.
    project_id: String, // Проект, подтверждающий владельца.
    user_uuid: String,  // Пользователь, подтверждающий владельца.
    stream_uuid: Uuid,  // Стрим бывшего обычного черновика.
    topic_uuid: Uuid,   // Тема внутри исходного стрима.
    revision: u64,      // Номер версии, совпадающий с ETag.
}

impl RestClient {
    /// Читает подтверждённую версию прежнего черновика без извлечения его текста.
    pub(in crate::messenger) async fn protected_draft_revision(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: DraftScope,
        id: Uuid,
    ) -> Result<Option<u64>, SourceError> {
        let response = self
            .request(owner, token, &format!("{DRAFTS_PATH}{id}"))?
            .send()
            .await
            .map_err(request_error)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        check_status(response.status())?;
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .ok_or(SourceError::InvalidResponse)?
            .to_owned();
        let address: WireDraftAddress = response.json().await.map_err(request_error)?;
        check_explicit_owner(owner, Some(&address.project_id), Some(&address.user_uuid))?;
        if address.uuid != id
            || address.stream_uuid != scope.stream_id
            || address.topic_uuid != scope.topic_id
            || address.revision == 0
            || etag != format!("\"{}\"", address.revision)
        {
            return Err(SourceError::InvalidResponse);
        }
        Ok(Some(address.revision))
    }

    /// Удаляет только версию, которую только что подтвердили для исходного адреса.
    pub(in crate::messenger) async fn delete_protected_draft_revision(
        &self,
        owner: &OwnerContext,
        token: &str,
        id: Uuid,
        revision: u64,
    ) -> Result<bool, SourceError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(&format!("{DRAFTS_PATH}{id}"));
        let response = self
            .send
            .delete(url)
            .header(ACCEPT, "application/json")
            .header(IF_MATCH, format!("\"{revision}\""))
            .bearer_auth(token)
            .send()
            .await
            .map_err(request_error)?;
        match response.status() {
            StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => Ok(true),
            StatusCode::PRECONDITION_FAILED => Ok(false),
            status => {
                check_status(status)?;
                Err(SourceError::InvalidResponse)
            }
        }
    }
}
