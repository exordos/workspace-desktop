use std::sync::Arc;

use thiserror::Error;

use crate::{
    auth::{Auth, AuthError},
    context::{OwnerContext, SessionGeneration},
    storage::{Cursor, Message, Storage, StorageError},
};

pub use crate::storage::ApplyOutcome;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub cursor: Cursor,
    pub message: Message,
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

pub struct Sync {
    auth: Auth,
    storage: Arc<Storage>,
}

impl Sync {
    pub fn new(auth: Auth, storage: Arc<Storage>) -> Self {
        Self { auth, storage }
    }

    pub(crate) fn auth(&self) -> &Auth {
        &self.auth
    }

    pub fn cursor(&self, owner: &OwnerContext, source: &str) -> Result<Option<Cursor>, SyncError> {
        Ok(self.storage.cursor(owner, source)?)
    }

    pub fn apply(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        source: &str,
        event: Event,
    ) -> Result<ApplyOutcome, SyncError> {
        let outcome = self.auth.with_active_generation(owner, generation, || {
            self.storage
                .apply_message(owner, source, event.cursor, event.message)
        })??;
        Ok(outcome)
    }
}
