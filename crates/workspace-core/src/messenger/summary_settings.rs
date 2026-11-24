//! Настройки выжимки одной темы. Проверяет роль участника, сессию и поля
//! команды перед отправкой, затем обновляет общий снимок правой панели.

use serde::{Deserialize, Serialize};

use crate::{
    auth::Service,
    context::{OwnerContext, SessionGeneration},
    storage::HistoryScope,
    sync::Sync,
};

use super::{
    MessengerSource,
    details::{ChannelSidebar, DetailsError, SidebarDetails, StreamRole},
};

/// Серверный предел длины пользовательской инструкции в символах.
pub const MAX_PROMPT_CHARS: usize = 16_384;

/// Уровень рассуждений модели, заданный владельцем темы.
#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Off,     // Рассуждения отключены.
    Minimal, // Минимальный бюджет.
    Low,     // Низкий бюджет.
    Medium,  // Средний бюджет.
    High,    // Высокий бюджет.
}

/// Настройки автоматического обновления выжимки.
#[derive(Clone, PartialEq, Eq)]
pub struct TopicSummaryConfiguration {
    pub enabled: bool,                      // Обновлять выжимку автоматически.
    pub prompt: Option<String>,             // None выбирает инструкцию сервера.
    pub reasoning: Option<ReasoningEffort>, // None выбирает уровень модели.
}

impl TopicSummaryConfiguration {
    /// Проверяет инструкцию после удаления внешних пробелов.
    pub fn valid_prompt(&self) -> bool {
        self.prompt.as_ref().is_none_or(|prompt| {
            let prompt = prompt.trim();
            !prompt.is_empty() && prompt.chars().count() <= MAX_PROMPT_CHARS
        })
    }

    /// Убирает пробелы, которые сервер отбрасывает при записи инструкции.
    pub fn normalized(mut self) -> Self {
        self.prompt = self.prompt.map(|prompt| prompt.trim().to_owned());
        self
    }
}

/// Исходный снимок и черновик, из которых строится только нужная часть команды.
pub struct TopicSummaryEdit {
    pub base: TopicSummaryConfiguration, // Последняя конфигурация из сервера.
    pub draft: TopicSummaryConfiguration, // Выбор пользователя в диалоге.
}

/// Отсутствие поля сохраняет прежнее значение, null сбрасывает параметр.
#[derive(Serialize)]
pub(super) struct SummaryPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_enabled: Option<bool>, // Изменился переключатель.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_system_prompt: Option<Option<String>>, // Изменилась инструкция.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_reasoning_effort: Option<Option<ReasoningEffort>>, // Изменился уровень.
}

impl TopicSummaryEdit {
    /// Формирует разницу с исходным снимком. Пустую команду не отправляет.
    fn patch(self) -> Result<Option<SummaryPatch>, DetailsError> {
        let draft = self.draft.normalized();
        if !draft.valid_prompt() {
            return Err(DetailsError::ActionDenied);
        }
        let base = self.base.normalized();
        if base == draft {
            return Ok(None);
        }
        Ok(Some(SummaryPatch {
            summary_enabled: (base.enabled != draft.enabled).then_some(draft.enabled),
            summary_system_prompt: (base.prompt != draft.prompt).then_some(draft.prompt),
            summary_reasoning_effort: (base.reasoning != draft.reasoning)
                .then_some(draft.reasoning),
        }))
    }
}

impl ChannelSidebar {
    /// Подтверждает наличие темы и роль текущего участника по снимку его сессии.
    pub fn can_edit_summary(&self, topic: uuid::Uuid) -> bool {
        !self.encrypted
            && self.topics.iter().any(|entry| entry.id == topic)
            && self.members.iter().any(|member| {
                member.is_self
                    && matches!(member.role, StreamRole::Owner | StreamRole::Administrator)
            })
    }
}

impl MessengerSource {
    /// Читает актуальные настройки доступной для управления темы.
    pub async fn topic_summary_configuration(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<TopicSummaryConfiguration, DetailsError> {
        self.check_summary_edit(sync, owner, generation, scope)
            .await?;
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self
            .rest
            .topic_summary_configuration(owner, &token, scope)
            .await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        result
    }

    /// Записывает изменившиеся поля и перечитывает общий снимок после подтверждения.
    pub async fn save_topic_summary_configuration(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        edit: TopicSummaryEdit,
    ) -> Result<TopicSummaryConfiguration, DetailsError> {
        let Some(patch) = edit.patch()? else {
            return self
                .topic_summary_configuration(sync, owner, generation, scope)
                .await;
        };
        // Локальный снимок отсекает неподходящие маршруты, а роль на момент
        // записи проверяет сервер в той же транзакции, что меняет конфигурацию.
        self.check_summary_edit(sync, owner, generation, scope)
            .await?;
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self
            .rest
            .save_topic_summary_configuration(owner, &token, scope, &patch)
            .await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        let configuration = result?;
        sync.refresh_sidebar(owner, generation, scope.stream_id)?;
        Ok(configuration)
    }

    /// Сверяет, что адрес относится к теме канала с подтверждённой ролью.
    async fn check_summary_edit(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<(), DetailsError> {
        let topic = scope.topic_id.ok_or(DetailsError::ActionDenied)?;
        let details = self
            .sidebar_details(sync, owner, generation, scope.stream_id)
            .await?;
        if matches!(details.as_ref(), SidebarDetails::Channel(channel) if channel.can_edit_summary(topic))
        {
            Ok(())
        } else {
            Err(DetailsError::ActionDenied)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Проверяет, что команда сбрасывает явно изменённый параметр и сохраняет остальные.
    #[test]
    fn summary_edit_sends_only_changed_fields_including_null() {
        let base = TopicSummaryConfiguration {
            enabled: true,
            prompt: Some("Выделяй решения".into()),
            reasoning: Some(ReasoningEffort::Low),
        };
        let draft = TopicSummaryConfiguration {
            prompt: None,
            ..base.clone()
        };
        let patch = TopicSummaryEdit {
            base: base.clone(),
            draft,
        }
        .patch()
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::to_value(patch).unwrap(),
            json!({ "summary_system_prompt": null })
        );
        assert!(
            TopicSummaryEdit {
                draft: TopicSummaryConfiguration {
                    prompt: Some("  Выделяй решения  ".into()),
                    ..base.clone()
                },
                base,
            }
            .patch()
            .unwrap()
            .is_none()
        );
    }

    /// Пустая инструкция не эквивалентна явному возврату к стандартной.
    #[test]
    fn summary_edit_rejects_empty_or_oversized_custom_prompt() {
        let base = TopicSummaryConfiguration {
            enabled: true,
            prompt: None,
            reasoning: None,
        };
        for value in ["  ".into(), "я".repeat(MAX_PROMPT_CHARS + 1)] {
            assert!(matches!(
                TopicSummaryEdit {
                    base: base.clone(),
                    draft: TopicSummaryConfiguration {
                        prompt: Some(value),
                        ..base.clone()
                    },
                }
                .patch(),
                Err(DetailsError::ActionDenied)
            ));
        }
    }
}
