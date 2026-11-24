//! Проверяет REST-снимки обычных черновиков текущего пользователя.

use reqwest::{
    StatusCode,
    header::{ACCEPT, IF_MATCH},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    messenger::activity::RemoteDraft,
    runtime::SourceError,
    storage::{DraftScope, HistoryTime},
};

use super::{RestClient, check_explicit_owner, check_status, request_error, workspace_origin};

const DRAFTS_PATH: &str = "/api/workspace/v1/messenger/drafts/";

/// Серверная запись из списка или ответа одного черновика.
#[derive(Deserialize)]
struct WireDraft {
    uuid: Uuid,         // Постоянный UUID черновика.
    project_id: String, // Проект из привязки IAM.
    user_uuid: String,  // Пользователь из привязки IAM.
    stream_uuid: Uuid,  // Разговор, которому принадлежит тема.
    topic_uuid: Uuid,   // Точная тема.
    revision: u64,      // Положительный номер серверной версии.
    updated_at: String, // Время изменения в UTC.
    payload: DraftBody, // Маркер Markdown и текст.
}

/// Поддерживаемое содержимое серверного черновика.
#[derive(Deserialize)]
struct DraftBody {
    kind: String,    // Ожидается markdown.
    content: String, // Обычный текст без ключей и шифртекста.
}

impl WireDraft {
    /// Проверяет владельца, размещение, версию и формат до передачи текста редактору.
    fn into_draft(self, owner: &OwnerContext) -> Result<RemoteDraft, SourceError> {
        check_explicit_owner(owner, Some(&self.project_id), Some(&self.user_uuid))?;
        if [self.uuid, self.stream_uuid, self.topic_uuid]
            .iter()
            .any(Uuid::is_nil)
            || self.revision == 0
            || self.payload.kind != "markdown"
            || self.payload.content.trim().is_empty()
        {
            return Err(SourceError::InvalidResponse);
        }
        let updated_at =
            HistoryTime::parse(&self.updated_at).ok_or(SourceError::InvalidResponse)?;
        Ok(RemoteDraft {
            id: self.uuid,
            scope: DraftScope {
                stream_id: self.stream_uuid,
                topic_id: self.topic_uuid,
            },
            revision: self.revision,
            updated_at,
            markdown: self.payload.content,
        })
    }

    /// Принимает выбранную версию только из ответа на её точный адрес.
    fn into_exact_draft(
        self,
        owner: &OwnerContext,
        scope: DraftScope,
        id: Uuid,
        etag: &str,
    ) -> Result<RemoteDraft, SourceError> {
        let draft = self.into_draft(owner)?;
        if draft.id != id || draft.scope != scope || etag != format!("\"{}\"", draft.revision) {
            return Err(SourceError::InvalidResponse);
        }
        Ok(draft)
    }
}

impl RestClient {
    /// Читает законченный снимок всех страниц, от свежих к старым.
    pub(in crate::messenger) async fn server_draft_collection(
        &self,
        owner: &OwnerContext,
        token: &str,
    ) -> Result<Vec<RemoteDraft>, SourceError> {
        let rows: Vec<WireDraft> = self
            .filtered_collection(
                owner,
                token,
                DRAFTS_PATH,
                &[("sort_key", "updated_at"), ("sort_dir", "desc")],
            )
            .await?;
        rows.into_iter().map(|row| row.into_draft(owner)).collect()
    }

    /// Читает точный UUID и отклоняет несовпадающий номер ETag или адрес черновика.
    pub(in crate::messenger) async fn server_draft(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: DraftScope,
        id: Uuid,
    ) -> Result<Option<RemoteDraft>, SourceError> {
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
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .ok_or(SourceError::InvalidResponse)?
            .to_owned();
        let row: WireDraft = response.json().await.map_err(request_error)?;
        row.into_exact_draft(owner, scope, id, &etag).map(Some)
    }

    /// Удаляет ровно подтверждённую версию. Расхождение требует нового выбора пользователя.
    pub(in crate::messenger) async fn delete_server_draft(
        &self,
        owner: &OwnerContext,
        token: &str,
        draft: &RemoteDraft,
    ) -> Result<bool, SourceError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(&format!("{DRAFTS_PATH}{}", draft.id));
        let response = self
            .send
            .delete(url)
            .header(ACCEPT, "application/json")
            .header(IF_MATCH, format!("\"{}\"", draft.revision))
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

#[cfg(test)]
mod tests;
