//! Данные пользователя Workspace, которые видны в карточке профиля.

use serde::Deserialize;
use uuid::Uuid;

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
    /// Возвращает UUID файла, если аватар хранится внутри Workspace.
    pub fn avatar_file_id(&self) -> Option<Uuid> {
        self.avatar
            .as_deref()?
            .strip_prefix("urn:image:")
            .and_then(|id| Uuid::parse_str(id).ok())
            .filter(|id| !id.is_nil())
    }
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
