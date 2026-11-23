//! Читает личную информацию и сохраняет статус и профиль текущего входа.
//! Все вызовы проверяют поколение сессии до отправки и после ответа.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    auth::Service,
    context::{OwnerContext, SessionGeneration},
    directory::UserProfile,
    runtime::SourceError,
    sync::Sync,
};

use super::{MessengerSource, details::DetailsError};

/// Данные личной карточки из IAM. UUID должен совпадать с владельцем сессии.
#[derive(Clone, Debug, Deserialize)]
pub struct PersonalIdentity {
    pub uuid: Uuid,                  // Идентификатор записи IAM.
    pub username: String,            // Логин пользователя.
    pub first_name: Option<String>,  // Имя.
    pub last_name: Option<String>,   // Фамилия.
    pub email: Option<String>,       // Адрес из IAM.
    pub phone: Option<String>,       // Контактный номер, если сохранён.
    pub description: Option<String>, // Текст «О себе».
}

/// Максимальный размер файла из настроек Workspace.
pub const MAX_OWN_AVATAR_BYTES: usize = 25 * 1024 * 1024;

/// Изменившиеся поля карточки. Вложенное None очищает имя через JSON null.
#[derive(Serialize)]
pub struct PersonalEdit {
    #[serde(skip_serializing_if = "Option::is_none")]
    first_name: Option<Option<String>>, // None пропускает неизменённое имя.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_name: Option<Option<String>>, // None пропускает неизменённую фамилию.
    #[serde(skip_serializing_if = "Option::is_none")]
    phone: Option<String>, // Номер или пустая строка для удаления.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>, // Новый текст «О себе».
}

impl PersonalEdit {
    /// Вычисляет изменения относительно последнего подтверждённого ответа IAM.
    pub fn new(
        baseline: &PersonalIdentity,
        first: &str,
        last: &str,
        phone: &str,
        description: &str,
    ) -> Self {
        let first = first.trim();
        let last = last.trim();
        let phone = phone.trim();
        let name_patch = |before: Option<&str>, after: &str| {
            (before.unwrap_or_default() != after)
                .then(|| (!after.is_empty()).then(|| after.to_owned()))
        };
        Self {
            first_name: name_patch(baseline.first_name.as_deref(), first),
            last_name: name_patch(baseline.last_name.as_deref(), last),
            phone: (baseline.phone.as_deref().unwrap_or_default() != phone)
                .then(|| phone.to_owned()),
            description: (baseline.description.as_deref().unwrap_or_default() != description)
                .then(|| description.to_owned()),
        }
    }

    /// Допускает пустое имя, учитывает длину Unicode и ограничение номера.
    pub fn is_valid(&self) -> bool {
        [&self.first_name, &self.last_name]
            .into_iter()
            .filter_map(|field| field.as_ref()?.as_deref())
            .all(|part| part.chars().count() <= 128 && !part.contains('\n'))
            && self
                .phone
                .as_deref()
                .is_none_or(|phone| phone.chars().count() <= 15)
    }

    /// Сообщает странице, что IAM-запрос не нужен.
    pub fn is_empty(&self) -> bool {
        self.first_name.is_none()
            && self.last_name.is_none()
            && self.phone.is_none()
            && self.description.is_none()
    }
}

/// Выбор статуса и ручного отсутствия в окне.
pub struct StatusEdit {
    pub text: String,  // Текст пользовательского статуса.
    pub emoji: String, // Один знак или пустая строка для удаления.
    pub away: bool,    // Пользователь сам включил отсутствие.
}

impl MessengerSource {
    /// Читает публичную часть личного профиля без зависимости от открытого разговора.
    pub async fn own_profile(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<UserProfile, DetailsError> {
        let user = owner_uuid(owner)?;
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self.rest.sidebar_profile(owner, &token, user).await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        result?.ok_or(DetailsError::ProfileUnavailable)
    }

    /// Загружает редактируемые поля из IAM и проверяет владельца ответа.
    pub async fn own_identity(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<PersonalIdentity, DetailsError> {
        let user = owner_uuid(owner)?;
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self.rest.own_identity(owner, &token, user, None).await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        Ok(result?)
    }

    /// Сохраняет личные данные в IAM. Некорректный ввод не отправляется.
    pub async fn edit_own_identity(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        edit: PersonalEdit,
    ) -> Result<PersonalIdentity, DetailsError> {
        let user = owner_uuid(owner)?;
        if !edit.is_valid() || edit.is_empty() {
            return Err(SourceError::InvalidResponse.into());
        }
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self
            .rest
            .own_identity(owner, &token, user, Some(&edit))
            .await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        Ok(result?)
    }

    /// Обновляет текущий пользовательский статус через действие присутствия.
    pub async fn edit_own_status(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        edit: StatusEdit,
    ) -> Result<UserProfile, DetailsError> {
        let user = owner_uuid(owner)?;
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self.rest.edit_own_status(owner, &token, user, edit).await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        Ok(result?)
    }

    /// Меняет аватар файлом или возвращает его системную заглушку, сохраняя владельца.
    pub async fn edit_own_avatar(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        file: Option<Vec<u8>>,
    ) -> Result<UserProfile, DetailsError> {
        let user = owner_uuid(owner)?;
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let fence = sync.auth().with_active_generation(owner, generation, || {
            sync.storage().avatar_fence(owner, user).ok()
        })?;
        let result = self.rest.edit_own_avatar(owner, &token, user, file).await;
        sync.auth().with_active_generation(owner, generation, || {
            if let (Ok(profile), Some(fence)) = (&result, fence) {
                let reference = profile
                    .avatar
                    .as_deref()
                    .and_then(crate::directory::normalize_avatar_reference);
                match sync
                    .storage()
                    .save_avatar_edit(owner, user, fence, reference.as_deref())
                {
                    Ok(true) => sync.publish_avatar_change(owner, generation, user),
                    Ok(false) => {}
                    Err(error) => log::warn!(
                        "event=workspace_avatar_cache_update result=failed error={error}"
                    ),
                }
            }
        })?;
        Ok(result?)
    }
}

/// Не отправляет запрос личного профиля с идентификатором из неполной сессии.
fn owner_uuid(owner: &OwnerContext) -> Result<Uuid, SourceError> {
    Uuid::parse_str(&owner.user_id)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or(SourceError::InvalidResponse)
}

/// Строит ссылку на избранное в веб-клиенте текущей организации без токенов.
pub fn favorites_link(owner: &OwnerContext) -> Result<String, SourceError> {
    let mut url = super::rest::workspace_origin(owner)?;
    url.path_segments_mut()
        .map_err(|_| SourceError::InvalidResponse)?
        .clear()
        .extend([
            "org",
            owner.organization_id.as_str(),
            "project",
            owner.project_id.as_str(),
            "activity",
            "favorites",
        ]);
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Имя можно очистить, неизменённые поля при этом не отправляются.
    #[test]
    fn patch_only_sends_changed_fields_and_supports_null_names() {
        let identity = PersonalIdentity {
            uuid: Uuid::from_u128(1),
            username: "local".into(),
            first_name: Some("Имя".into()),
            last_name: None,
            email: None,
            phone: Some("123".into()),
            description: None,
        };
        let clean = PersonalEdit::new(&identity, " ", " ", " 123 ", "Строка 1\nСтрока 2");
        assert!(clean.is_valid());
        assert_eq!(
            serde_json::to_value(clean).unwrap(),
            serde_json::json!({"first_name":null,"description":"Строка 1\nСтрока 2"})
        );
        assert!(!PersonalEdit::new(&identity, "Имя\nФамилия", "", "123", "").is_valid());
        assert!(PersonalEdit::new(&identity, "Имя", "", "123", "").is_empty());
    }
}
