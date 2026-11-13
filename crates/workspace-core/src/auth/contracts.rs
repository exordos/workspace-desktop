//! Общие типы и правила обмена между авторизацией, сервером IAM и локальными хранилищами.
//! Сначала описано хранение аккаунтов, затем запросы к IAM и состояния входа.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::context::{AccountId, OwnerContext, SessionGeneration};

/// Данные аккаунта, которые можно сохранить отдельно от токенов.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AccountRecord {
    pub context: OwnerContext, // Пользователь, сервер и выбранный проект.
    pub secret_key: String,    // Ключ, по которому в защищённом хранилище лежит токен обновления.
    pub signed_out: bool,      // Признак начатого выхода. При запуске нужно продолжить очистку.
    #[serde(default)]
    pub retired_secret_keys: Vec<String>, // Ключи старых токенов, удаление которых ещё нужно подтвердить.
}

/// Ошибка чтения, записи или удаления в хранилище авторизации.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("authentication storage is unavailable")]
pub struct StoreError;

/// Операции с данными аккаунтов, которые сохраняются между запусками.
pub trait AccountStore: Send + Sync + 'static {
    /// Загружает все сохранённые аккаунты.
    fn load(&self) -> Result<Vec<AccountRecord>, StoreError>;
    /// Добавляет или заменяет запись аккаунта.
    fn save(&self, record: &AccountRecord) -> Result<(), StoreError>;
    /// Удаляет запись по идентификатору аккаунта.
    fn remove(&self, account_id: &AccountId) -> Result<(), StoreError>;
}

/// Операции с защищённым хранилищем токенов.
pub trait SecretStore: Send + Sync + 'static {
    /// Читает секрет по ключу. Возвращает `None`, если записи нет.
    fn load(&self, key: &str) -> Result<Option<String>, StoreError>;
    /// Сохраняет или заменяет секрет по ключу.
    fn save(&self, key: &str, value: &str) -> Result<(), StoreError>;
    /// Удаляет секрет по ключу.
    fn remove(&self, key: &str) -> Result<(), StoreError>;
}

/// Проект, который IAM предлагает пользователю при входе.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IamProject {
    pub id: String,   // Идентификатор, который передаётся при выборе проекта.
    pub name: String, // Название, которое видит пользователь.
    pub organization_id: Option<String>, // Идентификатор организации. `None`, если IAM его не прислал.
}

/// Токены и сведения о пользователе, полученные от IAM.
pub struct IamToken {
    pub access_token: String, // Токен для запросов к сервисам Workspace.
    pub refresh_token: Option<String>, // Новый токен обновления, если IAM его выдал.
    pub expires_in: Duration, // Оставшийся срок действия токена доступа.
    pub user_id: String,      // Пользователь, которому выдан токен.
    pub project_id: Option<String>, // Проект, если токен привязан к нему.
    pub organization_id: Option<String>, // Организация, если IAM указал её.
}

/// Причины, по которым клиент IAM не смог выполнить запрос.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IamError {
    #[error("invalid Workspace server address")]
    InvalidServer, // Некорректный адрес Workspace.
    #[error("the server requires an OTP")]
    OtpRequired, // Сервер требует одноразовый код.
    #[error("IAM rejected the sign-in credentials")]
    InvalidCredentials, // Сервер отклонил данные для входа.
    #[error("IAM requires signing in again")]
    ReauthenticationRequired, // Для продолжения нужен новый вход.
    #[error("IAM is temporarily unavailable")]
    Temporary, // Временный сбой. Запрос можно повторить позже.
    #[error("IAM returned an invalid or mismatched response")]
    InvalidResponse, // Ответ не читается или не совпадает с ожидаемыми данными.
}

/// Запросы к IAM, которые нужны для входа и продления сессии.
pub trait IamClient: Send + Sync + 'static {
    /// Проверяет адрес и приводит его к единому виду для запросов и записей аккаунтов.
    fn normalize_server(&self, server: &str) -> Result<String, IamError>;

    /// Получает первый токен по логину, паролю и одноразовому коду, если он нужен.
    fn login(
        &self,
        server: &str,
        login: &str,
        password: &str,
        otp: Option<&str>,
    ) -> Result<IamToken, IamError>;

    /// Получает проекты, доступные владельцу токена.
    fn projects(&self, server: &str, access_token: &str) -> Result<Vec<IamProject>, IamError>;

    /// Обменивает токен обновления на токен для выбранного проекта.
    fn select_project(
        &self,
        server: &str,
        refresh_token: &str,
        project_id: &str,
    ) -> Result<IamToken, IamError>;

    /// Продлевает сессию по токену обновления.
    fn refresh(&self, server: &str, refresh_token: &str) -> Result<IamToken, IamError>;
}

/// Ошибки, которые авторизация передаёт вызывающему коду.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AuthError {
    #[error(transparent)]
    Iam(#[from] IamError), // Ошибка запроса к IAM.
    #[error(transparent)]
    Storage(#[from] StoreError), // Сбой хранилища аккаунтов или токенов.
    #[error("the sign-in attempt expired or was cancelled")]
    AttemptUnavailable, // Попытка входа истекла, отменена или уже занята выбором проекта.
    #[error("the project is not available for this sign-in attempt")]
    UnknownProject, // Этого проекта нет среди доступных для попытки входа.
    #[error("no active session matches this context")]
    SessionUnavailable, // Нет действующей сессии для указанного пользователя и проекта.
    #[error("the account must sign in again")]
    ReauthenticationRequired, // Чтобы продолжить, пользователь должен войти заново.
    #[error("sign-out cleanup is pending; retry signing out")]
    LogoutPending, // Выход начат. Очистку нужно повторить.
    #[error("could not start an authentication worker")]
    WorkerUnavailable, // Не удалось запустить фоновую работу авторизации.
}

/// Идентификатор незавершённой попытки входа.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LoginAttemptId(pub u64); // Порядковый номер, выданный авторизацией.

/// Данные, которые получает экран выбора проекта после проверки логина и пароля.
pub struct LoginAttempt {
    pub id: LoginAttemptId, // Номер попытки, который нужен для выбора проекта или отмены.
    pub projects: Vec<IamProject>, // Проекты, из которых пользователь может выбрать.
}

/// Сервис Workspace, для которого нужен токен доступа.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Service {
    Messenger, // Мессенджер Workspace.
}

/// Состояния, через которые проходит сессия аккаунта.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStatus {
    Restoring,                // Приложение восстанавливает сохранённую сессию.
    Active,                   // Сессия готова к работе.
    TemporarilyUnavailable,   // Сессия временно недоступна. Восстановление можно повторить.
    ReauthenticationRequired, // Пользователю нужно заново войти.
    SigningOut,               // Приложение очищает данные сессии после выхода.
}

/// Сведения о сессии, которые можно передавать другим частям приложения.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSnapshot {
    pub context: OwnerContext,         // Владелец и проект сессии.
    pub generation: SessionGeneration, // Номер сессии, по которому отсеивается работа от старого входа.
    pub status: SessionStatus,         // Текущее состояние сессии.
    pub token_version: u64, // Версия токена доступа. Меняется при установке нового токена.
}
