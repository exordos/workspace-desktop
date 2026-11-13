use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::context::{AccountId, OwnerContext, SessionGeneration};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AccountRecord {
    pub context: OwnerContext,
    pub secret_key: String,
    pub signed_out: bool,
    #[serde(default)]
    pub retired_secret_keys: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("authentication storage is unavailable")]
pub struct StoreError;

pub trait AccountStore: Send + Sync + 'static {
    fn load(&self) -> Result<Vec<AccountRecord>, StoreError>;
    fn save(&self, record: &AccountRecord) -> Result<(), StoreError>;
    fn remove(&self, account_id: &AccountId) -> Result<(), StoreError>;
}

pub trait SecretStore: Send + Sync + 'static {
    fn load(&self, key: &str) -> Result<Option<String>, StoreError>;
    fn save(&self, key: &str, value: &str) -> Result<(), StoreError>;
    fn remove(&self, key: &str) -> Result<(), StoreError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IamProject {
    pub id: String,
    pub name: String,
    pub organization_id: Option<String>,
}

pub struct IamToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Duration,
    pub user_id: String,
    pub project_id: Option<String>,
    pub organization_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IamError {
    #[error("invalid Workspace server address")]
    InvalidServer,
    #[error("the server requires an OTP")]
    OtpRequired,
    #[error("IAM rejected the sign-in credentials")]
    InvalidCredentials,
    #[error("IAM requires signing in again")]
    ReauthenticationRequired,
    #[error("IAM is temporarily unavailable")]
    Temporary,
    #[error("IAM returned an invalid or mismatched response")]
    InvalidResponse,
}

pub trait IamClient: Send + Sync + 'static {
    fn normalize_server(&self, server: &str) -> Result<String, IamError>;

    fn login(
        &self,
        server: &str,
        login: &str,
        password: &str,
        otp: Option<&str>,
    ) -> Result<IamToken, IamError>;

    fn projects(&self, server: &str, access_token: &str) -> Result<Vec<IamProject>, IamError>;

    fn select_project(
        &self,
        server: &str,
        refresh_token: &str,
        project_id: &str,
    ) -> Result<IamToken, IamError>;

    fn refresh(&self, server: &str, refresh_token: &str) -> Result<IamToken, IamError>;
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AuthError {
    #[error(transparent)]
    Iam(#[from] IamError),
    #[error(transparent)]
    Storage(#[from] StoreError),
    #[error("the sign-in attempt expired or was cancelled")]
    AttemptUnavailable,
    #[error("the project is not available for this sign-in attempt")]
    UnknownProject,
    #[error("no active session matches this context")]
    SessionUnavailable,
    #[error("the account must sign in again")]
    ReauthenticationRequired,
    #[error("sign-out cleanup is pending; retry signing out")]
    LogoutPending,
    #[error("could not start an authentication worker")]
    WorkerUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LoginAttemptId(pub u64);

pub struct LoginAttempt {
    pub id: LoginAttemptId,
    pub projects: Vec<IamProject>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Service {
    Messenger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStatus {
    Restoring,
    Active,
    TemporarilyUnavailable,
    ReauthenticationRequired,
    SigningOut,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSnapshot {
    pub context: OwnerContext,
    pub generation: SessionGeneration,
    pub status: SessionStatus,
    pub token_version: u64,
}
