//! Данные пользователя Workspace и доступ к его аватару по UUID.

use serde::Deserialize;
use uuid::Uuid;

mod avatars;
mod external_avatar;
pub(crate) use avatars::AvatarLoads;
pub use avatars::{AvatarFormat, AvatarImage, AvatarState, AvatarStore};

/// Публичные поля, подтверждённые ответом справочника.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserProfile {
    pub display_name: String,              // Имя и фамилия или имя пользователя.
    pub email: Option<String>,             // Адрес, если его вернул сервер.
    pub presence: Option<ProfilePresence>, // Последнее состояние из ответа сервера.
    pub status_text: Option<String>,       // Текст, который пользователь добавил к статусу.
    pub status_emoji: Option<String>,      // Эмодзи пользовательского статуса.
    pub avatar: Option<String>,            // Серверный URN изображения или Gravatar.
    pub created_at: Option<String>,        // Время появления в каталоге, в формате API.
}

impl UserProfile {
    /// Собирает ручной статус из эмодзи и текста для шапки и карточки.
    pub fn status_label(&self) -> Option<String> {
        match (&self.status_emoji, &self.status_text) {
            (Some(emoji), Some(text)) => Some(format!("{emoji} {text}")),
            (Some(value), None) | (None, Some(value)) => Some(value.clone()),
            (None, None) => None,
        }
    }

    /// Возвращает UUID файла, если аватар хранится внутри Workspace.
    pub fn avatar_file_id(&self) -> Option<Uuid> {
        self.avatar.as_deref().and_then(avatar_file_id)
    }
}

/// Читает UUID файла из ссылки на аватар Workspace.
fn avatar_file_id(reference: &str) -> Option<Uuid> {
    Uuid::parse_str(reference.strip_prefix("urn:image:")?)
        .ok()
        .filter(|id| !id.is_nil())
}

/// Приводит ссылку Workspace к одному виду и проверяет известные внешние ссылки.
pub(crate) fn normalize_avatar_reference(reference: &str) -> Option<String> {
    let reference = reference.trim();
    if reference.len() > 2048 {
        return None;
    }
    if reference.starts_with("urn:image:") {
        return avatar_file_id(reference).map(|id| format!("urn:image:{id}"));
    }

    external_avatar::profile_url(reference)?;
    Some(reference.to_owned())
}

/// Известные значения присутствия из Workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfilePresence {
    Active,       // Пользователь активен.
    Idle,         // Пользователь отошёл.
    Offline,      // Пользователь не в сети.
    DoNotDisturb, // Пользователь включил режим «Не беспокоить».
}

/// Ответ справочника до проверки UUID и нормализации отображаемых полей.
#[derive(Deserialize)]
pub(crate) struct ProfileRow {
    pub(crate) uuid: Uuid,        // Публичный идентификатор пользователя.
    first_name: Option<String>,   // Имя, если оно заполнено.
    last_name: Option<String>,    // Фамилия, если она заполнена.
    username: String,             // Резервное имя карточки.
    email: Option<String>,        // Доступный в проекте адрес.
    status: Option<String>,       // Сохраняет совместимость с новыми значениями API.
    display_name: Option<String>, // Имя пользователя из внешнего провайдера.
    status_text: Option<String>,  // Публичная подпись около присутствия.
    status_emoji: Option<String>, // Дополнительный символ пользовательского статуса.
    avatar: Option<String>,       // Идентификатор изображения в формате сервера.
    created_at: Option<String>,   // Время создания записи каталога.
}

impl ProfileRow {
    /// Отдаёт только карточку запрошенного пользователя и опускает пустые поля.
    pub(crate) fn into_profile(self, expected: Uuid) -> Option<UserProfile> {
        if self.uuid != expected {
            return None;
        }
        let name = [self.first_name, self.last_name]
            .iter()
            .flatten()
            .map(|part| part.trim())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        Some(UserProfile {
            display_name: if name.is_empty() {
                self.display_name
                    .and_then(nonempty)
                    .or_else(|| nonempty(self.username))?
            } else {
                name
            },
            email: self.email.and_then(nonempty),
            status_text: self.status_text.and_then(nonempty),
            status_emoji: self.status_emoji.and_then(nonempty),
            avatar: self.avatar.and_then(nonempty),
            created_at: self.created_at.and_then(nonempty),
            presence: self.status.as_deref().and_then(|status| match status {
                "active" => Some(ProfilePresence::Active),
                "idle" => Some(ProfilePresence::Idle),
                "offline" => Some(ProfilePresence::Offline),
                "do_not_disturb" => Some(ProfilePresence::DoNotDisturb),
                _ => None,
            }),
        })
    }
}

/// Возвращает строку для отображения, если в ней есть текст.
fn nonempty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}
