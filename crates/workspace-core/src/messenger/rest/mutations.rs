//! Разбирает заголовок квитанции и читает статус команды в исходном контексте.

use reqwest::{StatusCode, header::HeaderMap};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    messenger::message_actions::{MutationStatus, MutationStatusError},
    runtime::SourceError,
};

use super::{RestClient, check_explicit_owner, check_status, request_error};

const MUTATIONS_PATH: &str = "/api/workspace/v1/messenger/message_mutations";
const MUTATION_ID_HEADER: &str = "x-workspace-mutation-id";

/// Возвращает UUID команды из единственного корректного заголовка квитанции.
pub(in crate::messenger) fn mutation_id(headers: &HeaderMap) -> Result<Option<Uuid>, SourceError> {
    let mut values = headers.get_all(MUTATION_ID_HEADER).into_iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let id = value
        .to_str()
        .ok()
        .and_then(|text| Uuid::parse_str(text).ok())
        .filter(|id| !id.is_nil() && values.next().is_none());
    id.map(Some).ok_or(SourceError::InvalidResponse)
}

/// Поля ответа на чтение состояния очереди мутаций.
#[derive(Deserialize)]
struct WireMutationStatus {
    uuid: Uuid,             // UUID запроса из заголовка исходной операции.
    project_id: String,     // Проект, в котором сервер принял команду.
    user_uuid: String,      // Пользователь, который её отправил.
    status: MutationStatus, // Состояние серверной обработки.
}

impl RestClient {
    /// Читает статус по UUID квитанции и отвергает ответ для другой команды или аккаунта.
    pub(in crate::messenger) async fn mutation_status(
        &self,
        owner: &OwnerContext,
        token: &str,
        id: Uuid,
    ) -> Result<MutationStatus, MutationStatusError> {
        let response = self
            .request(owner, token, &format!("{MUTATIONS_PATH}/{id}"))?
            .send()
            .await
            .map_err(request_error)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(MutationStatusError::Unavailable);
        }
        check_status(response.status())?;
        let row: WireMutationStatus = response.json().await.map_err(request_error)?;
        check_explicit_owner(owner, Some(&row.project_id), Some(&row.user_uuid))?;
        if row.uuid != id {
            return Err(SourceError::InvalidResponse.into());
        }
        Ok(row.status)
    }
}
