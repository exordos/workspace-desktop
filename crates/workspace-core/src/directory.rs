//! Данные пользователя Workspace, которые видны в карточке профиля.

use serde::Deserialize;
use uuid::Uuid;

/// Публичные поля, подтверждённые ответом справочника.
#[derive(Debug, PartialEq, Eq)]
pub struct UserProfile {
    pub display_name: String,              // Имя и фамилия или имя пользователя.
    pub email: Option<String>,             // Адрес, если его вернул сервер.
    pub presence: Option<ProfilePresence>, // Последнее состояние из ответа сервера.
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
    uuid: Uuid,                 // Публичный идентификатор пользователя.
    first_name: Option<String>, // Имя, если оно заполнено.
    last_name: Option<String>,  // Фамилия, если она заполнена.
    username: String,           // Резервное имя карточки.
    email: Option<String>,      // Доступный в проекте адрес.
    status: Option<String>,     // Сохраняет совместимость с новыми значениями API.
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
                nonempty(self.username)?
            } else {
                name
            },
            email: self.email.and_then(nonempty),
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
