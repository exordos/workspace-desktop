//! Читает реакцию владельца и отправляет команды ресурса реакций.
//! Преобразует серверные счётчики и события в группы для истории.

use std::collections::{BTreeMap, HashSet};

use futures::{StreamExt, stream};
use reqwest::{StatusCode, header::ACCEPT};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    messenger::{
        message_actions::MessageTarget,
        quick_reactions::canonical_reaction_name,
        reactions::{ReactionError, valid_reaction_emoji},
    },
    runtime::SourceError,
    storage::{Cursor, HistoryMessage, MessageReaction},
    sync::Event,
};

use super::{RestClient, check_explicit_owner, check_status, request_error, workspace_origin};

const REACTIONS_PATH: &str = "/api/workspace/v1/messenger/message_reactions/";

/// Факт реакции из фильтрованной проекции текущего пользователя.
#[derive(Deserialize)]
struct ReactionRow {
    uuid: Uuid,         // UUID реакции для удаления.
    project_id: String, // Проект, где подтверждена реакция.
    user_uuid: String,  // Владелец реакции.
    message_uuid: Uuid, // Публичный UUID размещения.
    emoji_name: String, // Каноническое имя эмодзи.
}

impl ReactionRow {
    /// Принимает только факты указанного пользователя и точного сообщения.
    fn verify(&self, owner: &OwnerContext, message_id: Uuid) -> Result<(), SourceError> {
        check_explicit_owner(owner, Some(&self.project_id), Some(&self.user_uuid))?;
        if self.uuid.is_nil()
            || self.message_uuid != message_id
            || !valid_reaction_emoji(&self.emoji_name)
        {
            return Err(SourceError::InvalidResponse);
        }
        Ok(())
    }
}

impl RestClient {
    /// Сверяет фактические стрим, тему и тип сообщения перед записью.
    pub(in crate::messenger) async fn check_reaction_target(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: MessageTarget,
    ) -> Result<(), ReactionError> {
        let row = self
            .history_anchor_row(owner, token, Some(target.scope), target.message_id)
            .await?;
        if row.topic_uuid != target.topic_id
            || !matches!(row.payload.kind.as_str(), "markdown" | "e2ee")
            || row.into_history(owner, target.scope.stream_id)?.is_none()
        {
            return Err(ReactionError::InvalidInput);
        }
        Ok(())
    }

    /// Читает все реакции одного сообщения только для пользователя текущей сессии.
    async fn own_reaction_rows(
        &self,
        owner: &OwnerContext,
        token: &str,
        message_id: Uuid,
    ) -> Result<Vec<ReactionRow>, SourceError> {
        let rows: Vec<ReactionRow> = self
            .filtered_collection(
                owner,
                token,
                REACTIONS_PATH,
                &[
                    ("project_id", &owner.project_id),
                    ("message_uuid", &message_id.to_string()),
                    ("user_uuid", &owner.user_id),
                ],
            )
            .await?;
        for row in &rows {
            row.verify(owner, message_id)?;
        }
        Ok(rows)
    }

    /// Находит UUID реакции после проверки ответа привязанного к владельцу ресурса.
    pub(in crate::messenger) async fn own_reaction(
        &self,
        owner: &OwnerContext,
        token: &str,
        message_id: Uuid,
        emoji: &str,
    ) -> Result<Option<(Uuid, String)>, SourceError> {
        Ok(self
            .own_reaction_rows(owner, token, message_id)
            .await?
            .into_iter()
            .find(|row| canonical_reaction_name(&row.emoji_name) == emoji)
            .map(|row| (row.uuid, row.emoji_name)))
    }

    /// Сверяет собственную отметку для групп, чей список участников сервер скрыл.
    pub(in crate::messenger) async fn hydrate_own_reactions(
        &self,
        owner: &OwnerContext,
        token: &str,
        messages: &mut [HistoryMessage],
    ) {
        // Запросы владеют только индексом и UUID. Меняем строку после получения ответа.
        let missing: Vec<_> = messages
            .iter()
            .enumerate()
            .filter(|(_, row)| row.reactions.iter().any(|group| group.users.is_none()))
            .map(|(index, row)| (index, row.id))
            .collect();
        let mut requests = stream::iter(missing.into_iter().map(|(index, id)| async move {
            (index, self.own_reaction_rows(owner, token, id).await)
        }))
        .buffer_unordered(4);
        while let Some((index, result)) = requests.next().await {
            match result {
                Ok(own) => {
                    let own: HashSet<_> = own
                        .iter()
                        .map(|reaction| canonical_reaction_name(&reaction.emoji_name))
                        .collect();
                    for group in &mut messages[index].reactions {
                        if group.users.is_none() {
                            group.is_own = own.contains(&canonical_reaction_name(&group.emoji));
                        }
                    }
                }
                Err(error) => log::debug!("Messenger own reaction history: {error}"),
            }
        }
    }

    /// Отправляет POST один раз и подтверждает UUID, пользователя и нормализованное имя.
    pub(in crate::messenger) async fn add_reaction(
        &self,
        owner: &OwnerContext,
        token: &str,
        message_id: Uuid,
        emoji: &str,
        canonical: &str,
    ) -> Result<String, ReactionError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(REACTIONS_PATH);
        let response = self
            .send
            .post(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .json(&serde_json::json!({ "message_uuid": message_id, "emoji_name": emoji }))
            .send()
            .await
            .map_err(request_error)?;
        let status = response.status();
        if status.is_client_error() {
            return Err(ReactionError::Rejected(status.as_u16()));
        }
        if status != StatusCode::CREATED {
            check_status(status)?;
        }
        let row: ReactionRow = response.json().await.map_err(request_error)?;
        row.verify(owner, message_id)?;
        if canonical_reaction_name(&row.emoji_name) != canonical {
            return Err(SourceError::InvalidResponse.into());
        }
        Ok(row.emoji_name)
    }

    /// Снимает подтверждённую собственную реакцию по её UUID.
    pub(in crate::messenger) async fn remove_reaction(
        &self,
        owner: &OwnerContext,
        token: &str,
        reaction_id: Uuid,
    ) -> Result<(), ReactionError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(&format!("{REACTIONS_PATH}{reaction_id}"));
        let status = self
            .send
            .delete(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .send()
            .await
            .map_err(request_error)?
            .status();
        if status.is_client_error() {
            return Err(ReactionError::Rejected(status.as_u16()));
        }
        if status != StatusCode::NO_CONTENT {
            check_status(status)?;
        }
        Ok(())
    }
}

/// Собирает полный снимок реакций, если сервер передал его в записи сообщения.
pub(super) fn aggregate(
    counts: &Value,
    users: Option<&Value>,
    owner: &OwnerContext,
) -> Option<Vec<MessageReaction>> {
    let counts = counts.as_object()?;
    let users = users.and_then(Value::as_object);
    let user_id = Uuid::parse_str(&owner.user_id).ok();
    let mut groups = BTreeMap::new();
    for (emoji, count) in counts {
        let Some(count) = count
            .as_u64()
            .and_then(|count| u32::try_from(count).ok())
            .filter(|count| *count > 0)
        else {
            continue;
        };
        if !valid_reaction_emoji(emoji) {
            continue;
        }
        let users = users
            .and_then(|users| users.get(emoji))
            .and_then(|group| Vec::<Uuid>::deserialize(group).ok())
            .filter(|users| {
                users.len() == count as usize
                    && users.iter().all(|user| !user.is_nil())
                    && users.iter().collect::<HashSet<_>>().len() == users.len()
            });
        let is_own =
            user_id.is_some_and(|user| users.as_ref().is_some_and(|users| users.contains(&user)));
        groups.insert(
            emoji.clone(),
            MessageReaction {
                emoji: emoji.clone(),
                count,
                users,
                is_own,
            },
        );
    }
    Some(groups.into_values().collect())
}

/// Проверяет владельца события о собственной реакции и превращает его в изменение истории.
pub(super) fn own_reaction_event(
    payload: &Value,
    owner: &OwnerContext,
    cursor: Cursor,
    action: &str,
) -> Result<Event, SourceError> {
    if !matches!(action, "created" | "updated" | "deleted")
        || payload
            .get("kind")
            .and_then(Value::as_str)
            .and_then(|kind| kind.strip_prefix("message_reaction."))
            != Some(action)
    {
        return Ok(Event::Checkpoint(cursor));
    }
    let row = ReactionRow::deserialize(payload).map_err(|_| SourceError::InvalidResponse)?;
    if row.message_uuid.is_nil() {
        return Err(SourceError::InvalidResponse);
    }
    row.verify(owner, row.message_uuid)?;
    Ok(Event::OwnMessageReactionChanged {
        cursor,
        message_id: row.message_uuid,
        emoji: row.emoji_name,
        selected: action != "deleted",
    })
}
