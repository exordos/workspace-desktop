//! Читает и сохраняет настройки выжимки по HTTPS. Проверяет владельца,
//! канал и тему в каждом ответе.

use reqwest::{StatusCode, header::ACCEPT};
use serde::Deserialize;
use uuid::Uuid;

use crate::{context::OwnerContext, runtime::SourceError, storage::HistoryScope};

use super::{
    RestClient, TOPICS_PATH, check_explicit_owner, check_status, request_error, workspace_origin,
};
use crate::messenger::{
    details::DetailsError,
    summary_settings::{ReasoningEffort, SummaryPatch, TopicSummaryConfiguration},
};

/// Сведения о выжимке из ответа чтения темы или её действия.
#[derive(Deserialize)]
struct WireSummaryConfiguration {
    uuid: Uuid,                                        // Идентификатор темы в ответе.
    stream_uuid: Uuid,                                 // Родительский канал.
    project_id: String,                                // Проект текущего входа.
    user_uuid: String,                                 // Пользователь текущего входа.
    summary_enabled: Option<bool>,                     // Для старых ответов действует true.
    summary_system_prompt: Option<String>,             // Пользовательская инструкция или None.
    summary_reasoning_effort: Option<ReasoningEffort>, // Серверное имя уровня или None.
}

impl RestClient {
    /// Читает точную тему действующего маршрута, не загружая инструкции остальных тем.
    pub(in crate::messenger) async fn topic_summary_configuration(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
    ) -> Result<TopicSummaryConfiguration, DetailsError> {
        let topic = scope.topic_id.ok_or(DetailsError::ActionDenied)?;
        let request = self.request(owner, token, &format!("{TOPICS_PATH}/{topic}"))?;
        let response = request.send().await.map_err(request_error)?;
        settings_response(response, owner, scope, topic).await
    }

    /// Отправляет подтверждённое действие один раз и принимает только ответ для той же темы.
    pub(in crate::messenger) async fn save_topic_summary_configuration(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
        patch: &SummaryPatch,
    ) -> Result<TopicSummaryConfiguration, DetailsError> {
        let topic = scope.topic_id.ok_or(DetailsError::ActionDenied)?;
        let mut url = workspace_origin(owner)?;
        url.set_path(&format!(
            "{TOPICS_PATH}/{topic}/actions/set_summary_prompt/invoke"
        ));
        let response = self
            .send
            .post(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .json(patch)
            .send()
            .await
            .map_err(request_error)?;
        settings_response(response, owner, scope, topic).await
    }
}

/// Разбирает результат и не использует тему, чужую для маршрута или сессии.
async fn settings_response(
    response: reqwest::Response,
    owner: &OwnerContext,
    scope: HistoryScope,
    topic: Uuid,
) -> Result<TopicSummaryConfiguration, DetailsError> {
    match response.status() {
        StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => return Err(DetailsError::ActionDenied),
        status => check_status(status)?,
    }
    let row: WireSummaryConfiguration = response.json().await.map_err(request_error)?;
    check_explicit_owner(owner, Some(&row.project_id), Some(&row.user_uuid))?;
    if row.uuid != topic || row.stream_uuid != scope.stream_id {
        return Err(SourceError::InvalidResponse.into());
    }
    let configuration = TopicSummaryConfiguration {
        enabled: row.summary_enabled.unwrap_or(true),
        prompt: row.summary_system_prompt,
        reasoning: row.summary_reasoning_effort,
    };
    Ok(configuration.normalized())
}
