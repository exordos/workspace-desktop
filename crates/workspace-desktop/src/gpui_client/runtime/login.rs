//! Связывает форму входа с авторизацией рабочего профиля.
//! Сначала ошибки и владение попыткой, затем проверка адреса, настройки и команды IAM.

use std::sync::{Arc, Weak};

use uuid::Uuid;
use workspace_core::{
    auth::{Auth, AuthError, IamClient, IamError, IamProject, LoginAttemptId},
    context::OwnerContext,
    iam_http::WorkspaceIamHttp,
};

use super::{RequestError, RuntimeClient};

/// Причина отказа, которую форма может показать без данных профиля и ответов сервера.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginError {
    InvalidServer,          // Адрес Workspace не подходит для HTTPS-запроса.
    OtpRequired,            // Сервер запросил одноразовый код.
    InvalidOtp,             // После отправки кода сервер снова запросил код.
    InvalidCredentials,     // IAM отклонил данные для входа.
    TemporarilyUnavailable, // Сбой IAM допускает повтор.
    AttemptUnavailable,     // Попытка истекла, отменена либо уже завершена.
    UnknownProject,         // Попытка не содержит выбранного проекта.
    NoProjects,             // IAM не вернул ни одного доступного проекта.
    InvalidResponse,        // IAM прислал неполные или несогласованные данные.
    Storage,                // Не удалось прочитать или сохранить данные профиля.
    Busy,                   // Очередь занята. Можно повторить запрос.
    Stopped,                // Рабочий поток недоступен или остановлен.
}

impl From<AuthError> for LoginError {
    /// Переводит сбой авторизации в состояние формы.
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Iam(IamError::InvalidServer) => Self::InvalidServer,
            AuthError::Iam(IamError::OtpRequired) => Self::OtpRequired,
            AuthError::Iam(IamError::InvalidCredentials) => Self::InvalidCredentials,
            AuthError::Iam(IamError::Temporary) => Self::TemporarilyUnavailable,
            AuthError::Iam(IamError::InvalidResponse) => Self::InvalidResponse,
            AuthError::Iam(IamError::ReauthenticationRequired)
            | AuthError::AttemptUnavailable
            | AuthError::ReauthenticationRequired
            | AuthError::SessionUnavailable => Self::AttemptUnavailable,
            AuthError::UnknownProject => Self::UnknownProject,
            AuthError::Storage(_) | AuthError::LogoutPending => Self::Storage,
            AuthError::WorkerUnavailable => Self::Stopped,
        }
    }
}

impl From<RequestError> for LoginError {
    /// Различает временно занятую очередь и недоступный исполнитель.
    fn from(error: RequestError) -> Self {
        match error {
            RequestError::Busy => Self::Busy,
            RequestError::Stopped => Self::Stopped,
        }
    }
}

/// Держит выбор проекта открытым, пока это нужно форме. При уничтожении отменяет попытку.
pub struct LoginAttempt {
    cancellation: CancelLogin, // Позволяет освободить промежуточные токены по номеру попытки.
    projects: Vec<IamProject>, // Доступные проекты для этого входа.
}

impl LoginAttempt {
    /// Возвращает номер, с которым форма отправит запрос выбора проекта.
    pub fn id(&self) -> LoginAttemptId {
        self.cancellation.id
    }

    /// Возвращает проекты для выбора. Сессия появится после подтверждения проекта.
    pub fn projects(&self) -> &[IamProject] {
        &self.projects
    }
}

/// Отменяет незавершённую попытку при уходе с формы или прекращении ожидания запроса.
struct CancelLogin {
    auth: Option<Weak<Auth>>, // Слабая ссылка нужна, чтобы окно не удерживало блокировку профиля.
    id: LoginAttemptId,       // Номер попытки для удаления временных токенов.
}

impl CancelLogin {
    /// Привязывает отмену к авторизации того же профиля, в котором начался вход.
    fn new(auth: &Arc<Auth>, id: LoginAttemptId) -> Self {
        Self {
            auth: Some(Arc::downgrade(auth)),
            id,
        }
    }
}

impl Drop for CancelLogin {
    /// Удаляет только данные попытки в памяти. Сохранённую сессию ведёт авторизация.
    fn drop(&mut self) {
        if let Some(auth) = self.auth.as_ref().and_then(Weak::upgrade) {
            auth.cancel_login(self.id);
        }
    }
}

impl RuntimeClient {
    /// Проверяет HTTPS-адрес тем же правилом, которое применяет рабочий клиент IAM.
    pub fn validate_login_server(&self, server: &str) -> Result<String, LoginError> {
        WorkspaceIamHttp::new()
            .normalize_server(server)
            .map_err(|error| LoginError::from(AuthError::from(error)))
    }

    /// Ищет ссылку регистрации для уже проверенного адреса. Без настроек вход остаётся доступен.
    pub async fn registration_url(&self, server: String) -> Result<Option<String>, RequestError> {
        let op_id = Uuid::new_v4();
        log::debug!("gpui_login op_id={op_id} event=registration_url_requested");
        self.request(move |services| async move {
            #[cfg(all(test, feature = "gpui-test"))]
            if let Some(fixture) = services.registration_url_for_test {
                return fixture(&server);
            }
            let result = services.messenger.registration_url(&server).await;
            log::debug!(
                "gpui_login op_id={op_id} event=registration_url_finished result={:?}",
                result.as_ref().map(Option::is_some),
            );
            result.ok().flatten()
        })
        .await
    }

    /// Проверяет учётные данные и возвращает выбор проектов, не занимая поток GPUI или диспетчера.
    pub async fn begin_login(
        &self,
        server: String,
        login: String,
        password: String,
        otp: Option<String>,
    ) -> Result<LoginAttempt, LoginError> {
        let op_id = Uuid::new_v4();
        log::debug!("gpui_login op_id={op_id} event=credentials_requested");
        let result = self
            .request(move |services| async move {
                tokio::task::spawn_blocking(move || {
                    let result =
                        services
                            .auth
                            .begin_login(&server, &login, &password, otp.as_deref());
                    let attempt = match result {
                        Ok(attempt) => attempt,
                        Err(AuthError::Iam(IamError::OtpRequired))
                            if otp.as_ref().is_some_and(|code| !code.trim().is_empty()) =>
                        {
                            return Err(LoginError::InvalidOtp);
                        }
                        Err(error) => return Err(error.into()),
                    };
                    // Если окно прекратит ожидание, поздний ответ сбросит попытку при уничтожении.
                    let view = LoginAttempt {
                        cancellation: CancelLogin::new(&services.auth, attempt.id),
                        projects: attempt.projects,
                    };
                    if view.projects.is_empty() {
                        return Err(LoginError::NoProjects);
                    }
                    Ok(view)
                })
                .await
                .unwrap_or(Err(LoginError::Stopped))
            })
            .await
            .unwrap_or_else(|error| Err(error.into()));
        log::debug!(
            "gpui_login op_id={op_id} event=credentials_finished result={:?}",
            result.as_ref().map(|attempt| attempt.id().0),
        );
        result
    }

    /// Подтверждает проект через Auth, который сохраняет сессию и публикует её окну.
    pub async fn complete_login(
        &self,
        attempt: LoginAttemptId,
        project_id: String,
    ) -> Result<OwnerContext, LoginError> {
        let op_id = Uuid::new_v4();
        log::debug!(
            "gpui_login op_id={op_id} event=project_requested attempt_id={}",
            attempt.0,
        );
        let result = self
            .request(move |services| async move {
                // При остановке диспетчера блокирующий запрос ещё может ждать ответа IAM.
                // Отмена сообщает Auth, что после этого ответа сессию сохранять уже не нужно.
                let mut cancellation = CancelLogin::new(&services.auth, attempt);
                let result = tokio::task::spawn_blocking(move || {
                    services
                        .auth
                        .complete_login(attempt, &project_id)
                        .map_err(LoginError::from)
                })
                .await
                .unwrap_or(Err(LoginError::Stopped));
                // После обычного ответа формой можно повторить запрос при временном сбое.
                cancellation.auth = None;
                result
            })
            .await
            .unwrap_or_else(|error| Err(error.into()));
        log::debug!(
            "gpui_login op_id={op_id} event=project_finished attempt_id={} result={:?}",
            attempt.0,
            result.as_ref().map(|_| ()),
        );
        result
    }
}
