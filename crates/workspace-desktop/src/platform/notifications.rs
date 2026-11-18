//! Даёт оболочке общий способ проверить разрешение, отправить баннер и получить клик по нему.

use std::fmt;
use std::future::Future;

use futures::channel::mpsc;
use workspace_core::context::SessionGeneration;
use workspace_core::notification_target::NotificationTarget;

#[cfg(target_os = "macos")]
mod macos;

/// Настройки macOS, от которых зависит показ системного баннера.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationPermission {
    NeedsPermission, // Пользователь ещё не отвечал на запрос macOS.
    NeedsSettings,   // macOS не показывает баннеры при текущих настройках.
    Enabled,         // macOS разрешает показывать баннеры.
}

/// Ошибка системной операции, независимой от синхронизации чатов.
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

/// Цель нажатого баннера и номер входа, для которого он был показан.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationClick {
    pub target: NotificationTarget, // Аккаунт, проект, разговор и сообщение из исходного события.
    pub generation: SessionGeneration, // Номер входа, чтобы отсеять баннеры завершённой сессии.
}

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

    /// Один раз передаёт оболочке поток кликов. Центр может принять клик до подписки.
    pub fn take_clicks(&self) -> Option<mpsc::Receiver<NotificationClick>> {
        #[cfg(target_os = "macos")]
        return self.macos.as_ref()?.take_clicks();
        #[cfg(not(target_os = "macos"))]
        None
    }

    /// Получает настройки ОС без системного диалога.
    pub fn permission(
        &self,
    ) -> impl Future<Output = Result<NotificationPermission, NotificationError>> + Send + 'static
    {
        #[cfg(target_os = "macos")]
        let settings = self.macos.as_ref().map(macos::Center::permission);

        async move {
            #[cfg(target_os = "macos")]
            if let Some(settings) = settings {
                return settings.await;
            }
            Err(NotificationError::Unavailable)
        }
    }

    /// Вызывает системный запрос после явного действия пользователя.
    pub fn request_permission(
        &self,
    ) -> impl Future<Output = Result<(), NotificationError>> + Send + 'static {
        #[cfg(target_os = "macos")]
        let authorization = self.macos.as_ref().map(macos::Center::request_permission);

        async move {
            #[cfg(target_os = "macos")]
            if let Some(authorization) = authorization {
                return authorization.await;
            }
            Err(NotificationError::Unavailable)
        }
    }

    /// Открывает системный раздел уведомлений и сообщает, приняла ли macOS ссылку.
    pub fn open_settings(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self.macos.is_some() && macos::open_settings();
        #[cfg(not(target_os = "macos"))]
        false
    }

    /// Отправляет уведомление с уникальным идентификатором. Успех означает, что ОС приняла запрос.
    pub fn show(
        &self,
        id: &str,
        title: &str,
        body: &str,
        target: &NotificationTarget,
        generation: SessionGeneration,
    ) -> impl Future<Output = Result<(), NotificationError>> + Send + 'static {
        #[cfg(target_os = "macos")]
        let delivery = self.macos.as_ref().map(|center| {
            center.show(
                id,
                title,
                body,
                NotificationClick {
                    target: target.clone(),
                    generation,
                },
            )
        });
        #[cfg(not(target_os = "macos"))]
        let _ = (id, title, body, target, generation);

        async move {
            #[cfg(target_os = "macos")]
            if let Some(delivery) = delivery {
                return delivery.await;
            }
            Err(NotificationError::Unavailable)
        }
    }
}
