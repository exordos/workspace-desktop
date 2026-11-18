//! Запрашивает данные панели и выполняет команды с адресом исходного разговора.

use std::sync::Arc;

use uuid::Uuid;
use workspace_core::{
    directory::UserProfile,
    messenger::details::{DetailsError, SidebarDetails, StreamNotificationMode},
};

use crate::gpui_client::{
    contracts::{ConversationAddress, ConversationReply},
    runtime::{RequestError, RuntimeClient},
};

/// Причина, которую панель может показать без сетевых или личных данных.
#[derive(Clone, Copy)]
pub(super) enum Problem {
    AccessLost,         // Отзыв доступа закрывает панель и историю.
    Unsupported,        // У этого типа разговора пока нет карточки.
    ProfileUnavailable, // Сервер не показывает профиль проверенного собеседника.
    Unavailable,        // Временная ошибка, для которой можно предложить повтор.
}

/// Команда изменения подтверждённых сведений о канале.
pub(super) enum Mutation {
    Notifications(StreamNotificationMode), // Настройка уведомлений для всего стрима.
    AddMembers(Vec<Uuid>),                 // Выбранные пользователи проекта.
    RemoveMember(Uuid),                    // Идентификатор привязки участника.
}

/// Запрашивает снимок стрима у core под владельцем и поколением разговора.
pub(super) async fn load(
    runtime: RuntimeClient,
    address: ConversationAddress,
) -> ConversationReply<Result<Arc<SidebarDetails>, Problem>> {
    map_reply(
        runtime
            .conversation(address, |services, address| async move {
                services
                    .messenger
                    .sidebar_details(
                        &services.sync,
                        &address.route.owner,
                        address.generation,
                        address.route.scope.stream_id,
                    )
                    .await
            })
            .await,
    )
}

/// Получает список людей для поиска и выбора приглашённых участников.
pub(super) async fn directory(
    runtime: RuntimeClient,
    address: ConversationAddress,
) -> ConversationReply<Result<Vec<(Uuid, UserProfile)>, Problem>> {
    map_reply(
        runtime
            .conversation(address, |services, address| async move {
                services
                    .messenger
                    .sidebar_directory(&services.sync, &address.route.owner, address.generation)
                    .await
            })
            .await,
    )
}

/// Передаёт изменение модулю Messenger, который проверяет права и обновляет кэш.
pub(super) async fn mutate(
    runtime: RuntimeClient,
    address: ConversationAddress,
    mutation: Mutation,
) -> ConversationReply<Result<(), Problem>> {
    map_reply(
        runtime
            .conversation(address, move |services, address| async move {
                let owner = &address.route.owner;
                let generation = address.generation;
                let stream_id = address.route.scope.stream_id;
                let sync = &services.sync;
                let messenger = &services.messenger;
                match mutation {
                    Mutation::Notifications(mode) => {
                        messenger
                            .update_sidebar_notifications(sync, owner, generation, stream_id, mode)
                            .await
                    }
                    Mutation::AddMembers(ids) => {
                        messenger
                            .add_sidebar_members(sync, owner, generation, stream_id, &ids)
                            .await
                    }
                    Mutation::RemoveMember(binding_id) => {
                        messenger
                            .remove_sidebar_member(sync, owner, generation, stream_id, binding_id)
                            .await
                    }
                }
            })
            .await,
    )
}

/// Оставляет адрес запроса и преобразует ошибку в безопасное состояние панели.
fn map_reply<T>(
    reply: ConversationReply<Result<Result<T, DetailsError>, RequestError>>,
) -> ConversationReply<Result<T, Problem>> {
    ConversationReply {
        address: reply.address,
        value: match reply.value {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(DetailsError::AccessLost)) => Err(Problem::AccessLost),
            Ok(Err(DetailsError::Unsupported)) => Err(Problem::Unsupported),
            Ok(Err(DetailsError::ProfileUnavailable)) => Err(Problem::ProfileUnavailable),
            Ok(Err(_)) | Err(_) => Err(Problem::Unavailable),
        },
    }
}
