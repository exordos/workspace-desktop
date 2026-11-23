//! Вызывает IAM для данных профиля и Workspace для статуса и аватара.

use reqwest::{Method, header::CONTENT_TYPE};

use super::*;
use crate::{
    directory::{AvatarFormat, ProfileRow, UserProfile},
    messenger::own_profile::{MAX_OWN_AVATAR_BYTES, PersonalEdit, PersonalIdentity, StatusEdit},
};

impl RestClient {
    /// Сверяет UUID IAM и применяет только поля, которые передала страница.
    pub(in crate::messenger) async fn own_identity(
        &self,
        owner: &OwnerContext,
        token: &str,
        user: Uuid,
        edit: Option<&PersonalEdit>,
    ) -> Result<PersonalIdentity, SourceError> {
        let path = format!("/api/core/v1/iam/users/{user}");
        let request = match edit {
            Some(edit) => self
                .profile_mutation(owner, token, Method::PUT, &path)?
                .json(edit),
            None => self.request(owner, token, &path)?,
        };
        let identity: PersonalIdentity = self.profile_json(request).await?;
        if identity.uuid == user && !identity.username.trim().is_empty() {
            Ok(identity)
        } else {
            Err(SourceError::InvalidResponse)
        }
    }

    /// Ограничивает текст и эмодзи перед отправкой, пустые поля обнуляет.
    pub(in crate::messenger) async fn edit_own_status(
        &self,
        owner: &OwnerContext,
        token: &str,
        user: Uuid,
        edit: StatusEdit,
    ) -> Result<UserProfile, SourceError> {
        let path = format!("/api/workspace/v1/users/{user}/actions/presence/invoke");
        let status = if edit.away { "idle" } else { "active" };
        let body = serde_json::json!({"status": status, "emoji": nonempty(&edit.emoji, 64), "text": nonempty(&edit.text, 256)});
        let request = self
            .profile_mutation(owner, token, Method::POST, &path)?
            .json(&body);
        self.profile_json::<ProfileRow>(request)
            .await?
            .into_profile(user)
            .ok_or(SourceError::InvalidResponse)
    }

    /// Передаёт серверу JPEG или PNG в поле file, либо вызывает сброс аватара.
    pub(in crate::messenger) async fn edit_own_avatar(
        &self,
        owner: &OwnerContext,
        token: &str,
        user: Uuid,
        bytes: Option<Vec<u8>>,
    ) -> Result<UserProfile, SourceError> {
        let request = if let Some(bytes) = bytes {
            let (mime, extension) = match AvatarFormat::sniff(&bytes) {
                Some(AvatarFormat::Png) => ("image/png", "png"),
                Some(AvatarFormat::Jpeg) => ("image/jpeg", "jpg"),
                Some(AvatarFormat::Gif) => ("image/gif", "gif"),
                Some(AvatarFormat::Webp) => ("image/webp", "webp"),
                None => return Err(SourceError::InvalidResponse),
            };
            if bytes.len() > MAX_OWN_AVATAR_BYTES {
                return Err(SourceError::InvalidResponse);
            }
            // Случайная граница не совпадёт с содержимым, имя файла не берём из внешнего ввода.
            let boundary = format!("workspace-avatar-{}", Uuid::new_v4());
            let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"avatar.{extension}\"\r\nContent-Type: {mime}\r\n\r\n").into_bytes();
            body.extend(bytes);
            body.extend(format!("\r\n--{boundary}--\r\n").bytes());
            self.profile_mutation(
                owner,
                token,
                Method::POST,
                &format!("/api/workspace/v1/users/{user}/actions/avatar_upload/invoke"),
            )?
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
        } else {
            self.profile_mutation(
                owner,
                token,
                Method::POST,
                &format!("/api/workspace/v1/users/{user}/actions/avatar_reset/invoke"),
            )?
            .json(&serde_json::json!({}))
        };
        self.profile_json::<ProfileRow>(request)
            .await?
            .into_profile(user)
            .ok_or(SourceError::InvalidResponse)
    }

    /// Отправляет изменение на исходный сервер без автоматического повтора.
    fn profile_mutation(
        &self,
        owner: &OwnerContext,
        token: &str,
        method: Method,
        path: &str,
    ) -> Result<RequestBuilder, SourceError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(path);
        Ok(self
            .send
            .request(method, url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token))
    }

    /// Не пытается разбирать тело ошибки как данные пользователя.
    async fn profile_json<T: DeserializeOwned>(
        &self,
        request: RequestBuilder,
    ) -> Result<T, SourceError> {
        let response = request.send().await.map_err(request_error)?;
        check_status(response.status())?;
        response.json().await.map_err(request_error)
    }
}

/// Сохраняет границу UTF-16, которую использует оригинальный интерфейс.
fn nonempty(value: &str, limit: usize) -> Option<String> {
    let value = value.trim();
    let mut remaining = limit;
    let result: String = value
        .chars()
        .take_while(|character| {
            if let Some(left) = remaining.checked_sub(character.len_utf16()) {
                remaining = left;
                true
            } else {
                false
            }
        })
        .collect();
    (!result.is_empty()).then_some(result)
}
