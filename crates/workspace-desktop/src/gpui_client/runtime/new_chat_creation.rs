//! Передаёт операции создания разговоров из GPUI в фоновый поток Core.
//! Все команды получают вход, выбранный в момент действия пользователя.

use uuid::Uuid;
use workspace_core::{
    directory::UserProfile,
    messenger::{
        creation::{ChannelCreation, ConversationCreationError, CreatedChannel},
        details::DetailsError,
    },
};

use super::{AccountSession, RequestError, RuntimeClient};

impl RuntimeClient {
    /// Читает страницы справочника тем же способом, что панель участников канала.
    pub(in crate::gpui_client) async fn new_chat_people(
        &self,
        session: AccountSession,
    ) -> Result<Result<Vec<(Uuid, UserProfile)>, DetailsError>, RequestError> {
        self.request(move |services| async move {
            let own = session.user_uuid();
            services
                .messenger
                .sidebar_directory(&services.sync, &session.owner, session.generation)
                .await
                .map(|mut people| {
                    people.retain(|(id, _)| !id.is_nil() && Some(*id) != own);
                    people.sort_by_cached_key(|(_, person)| person.display_name.to_lowercase());
                    people
                })
        })
        .await
    }

    /// Возвращает UUID личного стрима для перехода обычным маршрутом чата.
    pub(in crate::gpui_client) async fn new_chat_open_direct(
        &self,
        session: AccountSession,
        peer: Uuid,
    ) -> Result<Result<Uuid, ConversationCreationError>, RequestError> {
        self.request(move |services| async move {
            services
                .messenger
                .open_or_create_direct(&services.sync, &session.owner, session.generation, peer)
                .await
        })
        .await
    }

    /// Создаёт канал с постоянным UUID попытки, которым управляет открытая форма.
    pub(in crate::gpui_client) async fn new_chat_create_channel(
        &self,
        session: AccountSession,
        attempt: ChannelCreation,
    ) -> Result<Result<CreatedChannel, ConversationCreationError>, RequestError> {
        self.request(move |services| async move {
            services
                .messenger
                .create_channel(&services.sync, &session.owner, session.generation, &attempt)
                .await
        })
        .await
    }

    /// Добавляет людей в уже созданный канал через существующую команду участников.
    pub(in crate::gpui_client) async fn new_chat_add_members(
        &self,
        session: AccountSession,
        stream_id: Uuid,
        people: Vec<Uuid>,
    ) -> Result<Result<(), DetailsError>, RequestError> {
        self.request(move |services| async move {
            services
                .messenger
                .add_sidebar_members(
                    &services.sync,
                    &session.owner,
                    session.generation,
                    stream_id,
                    &people,
                )
                .await
        })
        .await
    }
}
