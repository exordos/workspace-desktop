//! Даёт оболочке общий способ проверить разрешение и отправить баннер. В macOS запрос обрабатывает центр уведомлений.

use std::fmt;
use std::future::Future;

#[cfg(target_os = "macos")]
mod macos;

/// Ошибка, которую оболочка может показать независимо от состояния синхронизации.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NotificationError {
    /// У процесса нет доступа к центру уведомлений на этой платформе.
    Unavailable,
    /// Системные настройки запрещают уведомления приложения.
    PermissionDenied,
    /// Система вернула ошибку при проверке разрешения или отправке запроса.
    System(String), // Текст ошибки от платформы.
}

impl fmt::Display for NotificationError {
    /// Даёт оболочке короткое описание причины.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("Системные уведомления недоступны"),
            Self::PermissionDenied => {
                formatter.write_str("Уведомления отключены в настройках macOS")
            }
            Self::System(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for NotificationError {}

/// Системный адаптер, который оболочка хранит на протяжении работы окна.
#[derive(Default)]
pub struct Notifications {
    #[cfg(target_os = "macos")]
    macos: Option<macos::Center>, // Есть при запуске из приложения с идентификатором macOS.
}

impl Notifications {
    /// Подключает центр уведомлений. Вызывать в главном потоке приложения.
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            macos: macos::Center::new(),
        }
    }

    /// Запрашивает разрешение заранее, чтобы первое сообщение сразу могло показать баннер.
    pub fn prepare(&self) -> impl Future<Output = Result<(), NotificationError>> + Send + 'static {
        #[cfg(target_os = "macos")]
        let authorization = self.macos.as_ref().map(macos::Center::prepare);

        async move {
            #[cfg(target_os = "macos")]
            if let Some(authorization) = authorization {
                return authorization.await;
            }
            Err(NotificationError::Unavailable)
        }
    }

    /// Отправляет уведомление с уникальным идентификатором. Успех означает, что ОС приняла запрос.
    pub fn show(
        &self,
        id: &str,
        title: &str,
        body: &str,
    ) -> impl Future<Output = Result<(), NotificationError>> + Send + 'static {
        #[cfg(target_os = "macos")]
        let delivery = self
            .macos
            .as_ref()
            .map(|center| center.show(id, title, body));
        #[cfg(not(target_os = "macos"))]
        let _ = (id, title, body);

        async move {
            #[cfg(target_os = "macos")]
            if let Some(delivery) = delivery {
                return delivery.await;
            }
            Err(NotificationError::Unavailable)
        }
    }
}
