//! Выполняет команды над разговором из каталога и сообщает, если создана новая тема.

use uuid::Uuid;

use crate::{
    auth::Service,
    context::{OwnerContext, SessionGeneration},
    directory::UserProfile,
    sync::Sync,
};

use super::{
    MessengerSource,
    details::{DetailsError, DirectSidebar, TopicNotificationMode},
};

impl TopicNotificationMode {
    /// Перечисляет режимы, которые пользователь может выбрать для темы.
    pub fn choices(direct: bool, stream_muted: bool, mode: Self) -> &'static [Self] {
        use TopicNotificationMode::{Default, Follow, Mute, Unmute};
        if !direct && (stream_muted || mode == Unmute) {
            &[Mute, Default, Unmute, Follow]
        } else {
            &[Mute, Default, Follow]
        }
    }

    /// Показывает прежний режим явного включения личной темы как «Следить».
    pub fn displayed(self, direct: bool) -> Self {
        match (self, direct) {
            (Self::Unmute, true) => Self::Follow,
            (mode, _) => mode,
        }
    }
}

/// Команда участника разговора из общего контекстного меню.
pub enum SidebarStreamAction {
    MarkRead, // Прочитать все темы от имени текущего пользователя.
    CreateTopic {
        id: Uuid,     // UUID будущей темы, одинаковый при повторе формы.
        name: String, // Название темы в разговоре.
    },
    Rename(String), // Переименовать канал.
    Archive,        // Архивировать разговор после подтверждения.
}

/// Подтверждённый результат операции над разговором.
pub enum SidebarStreamResult {
    Updated,            // Сервер применил действие над разговором.
    TopicCreated(Uuid), // UUID созданной темы для открытия её истории.
}

impl MessengerSource {
    /// Читает публичный профиль по UUID из ссылки. Сервер проверяет право текущего входа.
    pub async fn reference_user_profile(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        user_id: Uuid,
    ) -> Result<Option<UserProfile>, DetailsError> {
        if user_id.is_nil() {
            return Ok(None);
        }
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let profile = self.rest.sidebar_profile(owner, &token, user_id).await;
        auth.with_active_generation(owner, generation, || ())?;
        profile
    }

    /// Читает публичный профиль подтверждённого собеседника указанной лички.
    pub async fn sidebar_contact(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
    ) -> Result<DirectSidebar, DetailsError> {
        self.check_sidebar_mutation_access(sync, owner, generation, stream_id)?;
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self.rest.sidebar_contact(owner, &token, stream_id).await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        result
    }

    /// Выполняет команду для подтверждённого разговора в действующем входе.
    pub async fn update_sidebar_stream(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        action: SidebarStreamAction,
    ) -> Result<SidebarStreamResult, DetailsError> {
        self.check_sidebar_mutation_access(sync, owner, generation, stream_id)?;
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self
            .rest
            .update_sidebar_stream(owner, &token, stream_id, action)
            .await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        let result = result?;
        sync.refresh_sidebar_catalog(owner, generation, stream_id)?;
        Ok(result)
    }
}
