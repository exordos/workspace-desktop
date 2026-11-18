//! Общие ошибки и команды мессенджера для панели сведений и аватаров.

use thiserror::Error;

use crate::{auth::AuthError, runtime::SourceError};

use super::MessengerSource;

/// Различает потерю чата, отсутствие профиля и временный сбой.
#[derive(Clone, Debug, Error)]
pub enum DetailsError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Вход изменился или не выдал токен.
    #[error("conversation access lost")]
    AccessLost, // Входу больше недоступен стрим или тема.
    #[error("peer profile unavailable")]
    ProfileUnavailable, // Сервер не показывает профиль проверенного собеседника.
    #[error("conversation details unsupported")]
    Unsupported, // Адрес ведёт в тип разговора вне этого среза.
    #[error("conversation action denied")]
    ActionDenied, // У сессии нет права выполнить действие.
    #[error(transparent)]
    Source(#[from] SourceError), // Ошибка запроса или несовпадение данных.
}

mod avatars;
mod sidebar;
pub use avatars::*;
pub use sidebar::*;
