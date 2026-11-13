//! Продлевает сессию и выдаёт токен доступа для запросов.
//! Здесь рассчитываются сроки токенов, объединяются параллельные обновления и сохраняются новые секреты.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::future::{BoxFuture, FutureExt, Shared};

use super::{
    AccountState, Auth, AuthError, IamError, IamToken, Service, SessionStatus, StoreError,
};
use crate::context::{OwnerContext, SessionGeneration};

const REFRESH_SKEW: Duration = Duration::from_secs(60);
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Результат одного обновления, которого могут ждать несколько запросов.
pub(super) type RefreshFlight = Shared<BoxFuture<'static, Result<(), AuthError>>>;

/// Ответ IAM с рассчитанными сроками действия токена доступа.
pub(super) struct TimedToken {
    token: IamToken,     // Токены и данные владельца, которые прислал IAM.
    expires_at: Instant, // Момент, после которого токен доступа использовать нельзя.
    refresh_at: Instant, // Момент, с которого пора запросить новый токен.
}

impl TimedToken {
    /// Проверяет владельца и срок токена, затем рассчитывает время обновления и истечения.
    pub(super) fn new(
        token: IamToken,
        context: &OwnerContext,
        requested_at: Instant,
    ) -> Result<Self, AuthError> {
        if token.user_id != context.user_id
            || token.project_id.as_deref() != Some(&context.project_id)
            || token.organization_id.as_ref().is_some_and(|id| {
                context.organization_id != context.server && id != &context.organization_id
            })
        {
            return Err(AuthError::ReauthenticationRequired);
        }
        if token.access_token.is_empty()
            || token.expires_in.is_zero()
            || token.refresh_token.as_ref().is_some_and(String::is_empty)
        {
            return Err(IamError::InvalidResponse.into());
        }
        let expires_at = requested_at
            .checked_add(token.expires_in)
            .ok_or(IamError::InvalidResponse)?;
        let skew = REFRESH_SKEW.min(token.expires_in / 10);
        Ok(Self {
            token,
            expires_at,
            refresh_at: expires_at - skew,
        })
    }

    /// Забирает новый токен обновления из ответа, оставляя вместо него `None`.
    pub(super) fn take_replacement_refresh(&mut self) -> Option<String> {
        self.token.refresh_token.take()
    }

    /// Возвращает ссылку на новый токен обновления, если IAM его выдал.
    pub(super) fn replacement_refresh(&self) -> Option<&String> {
        self.token.refresh_token.as_ref()
    }
}

impl AccountState {
    /// Устанавливает токен, сбрасывает задержку повторов и помечает сессию активной.
    pub(super) fn install(&mut self, mut token: TimedToken) {
        if let Some(refresh_token) = token.take_replacement_refresh() {
            self.refresh_token = Some(refresh_token);
        }
        self.access = Some(token);
        self.retry_at = None;
        self.status = SessionStatus::Active;
        self.token_version += 1;
    }

    /// Возвращает токен доступа, пока не закончился его срок.
    fn valid_access(&self, now: Instant) -> Option<String> {
        self.access
            .as_ref()
            .filter(|token| now < token.expires_at)
            .map(|token| token.token.access_token.clone())
    }

    /// Возвращает токен доступа, пока не настало время обновления.
    fn token_before_refresh(&self, now: Instant) -> Option<String> {
        self.access
            .as_ref()
            .filter(|token| now < token.refresh_at)
            .map(|token| token.token.access_token.clone())
    }
}

impl Auth {
    /// Сбрасывает отклонённый токен, если сессия и версия токена ещё те же.
    pub(crate) fn reject_access_token(
        &self,
        context: &OwnerContext,
        generation: SessionGeneration,
        version: u64,
    ) {
        let Ok(slot) = self.account_slot(&context.account_id) else {
            return;
        };
        let mut state = slot.lock().unwrap();
        if state.generation == generation
            && state.token_version == version
            && state.status == SessionStatus::Active
            && state.record.as_ref().map(|record| &record.context) == Some(context)
            && state.access.take().is_some()
        {
            // Новый токен запустит задачу с начальной паузой переподключения.
            // Поэтому ограничиваем частоту повторов здесь, пока сервер отклоняет токены.
            let retry_at = self.inner.clock.now() + RETRY_DELAY;
            state.retry_at = Some(
                state
                    .retry_at
                    .map_or(retry_at, |current| current.max(retry_at)),
            );
        }
    }

    /// Выдаёт действующий токен доступа для текущей сессии, при необходимости обновляя его.
    pub async fn access_token(
        &self,
        context: &OwnerContext,
        service: Service,
    ) -> Result<String, AuthError> {
        self.load_accounts()?;
        let slot = self.account_slot(&context.account_id)?;
        let generation = slot.lock().unwrap().generation;
        self.access_token_for_generation(context, service, generation)
            .await
    }

    /// Выдаёт токен для конкретного поколения сессии и проверяет его после ожидания обновления.
    pub(crate) async fn access_token_for_generation(
        &self,
        context: &OwnerContext,
        _service: Service,
        generation: SessionGeneration,
    ) -> Result<String, AuthError> {
        let slot = self.account_slot(&context.account_id)?;
        loop {
            let (flight, valid_token) = {
                let mut state = slot.lock().unwrap();
                if state.generation != generation
                    || state.status == SessionStatus::SigningOut
                    || state.record.as_ref().map(|record| &record.context) != Some(context)
                {
                    return Err(AuthError::SessionUnavailable);
                }
                if state.status == SessionStatus::ReauthenticationRequired {
                    return Err(AuthError::ReauthenticationRequired);
                }
                let now = self.inner.clock.now();
                if let Some(token) = state.token_before_refresh(now) {
                    return Ok(token);
                }
                let valid_token = if state.retry_at.is_some_and(|retry_at| now < retry_at) {
                    state.valid_access(now)
                } else {
                    None
                };
                (self.refresh_flight(&slot, &mut state)?, valid_token)
            };
            if let Some(token) = valid_token {
                return Ok(token);
            }
            match flight.await {
                Ok(()) => {
                    let state = slot.lock().unwrap();
                    Self::check_generation(&state, generation)?;
                    if let Some(token) = state.valid_access(self.inner.clock.now()) {
                        return Ok(token);
                    }
                }
                Err(AuthError::Iam(IamError::Temporary) | AuthError::Storage(StoreError)) => {
                    let state = slot.lock().unwrap();
                    Self::check_generation(&state, generation)?;
                    if let Some(token) = state.valid_access(self.inner.clock.now()) {
                        return Ok(token);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Проверяет, что за время ожидания сессию не заменили и из неё не вышли.
    fn check_generation(
        state: &AccountState,
        generation: SessionGeneration,
    ) -> Result<(), AuthError> {
        if state.generation != generation || state.status == SessionStatus::SigningOut {
            Err(AuthError::SessionUnavailable)
        } else {
            Ok(())
        }
    }

    /// Возвращает уже запущенное обновление или запускает одно новое для аккаунта.
    pub(super) fn refresh_flight(
        &self,
        slot: &Arc<Mutex<AccountState>>,
        state: &mut AccountState,
    ) -> Result<RefreshFlight, AuthError> {
        if let Some(flight) = &state.refresh {
            return Ok(flight.clone());
        }
        let auth = self.clone();
        let slot = slot.clone();
        let generation = state.generation;
        let delay = state
            .retry_at
            .map(|retry_at| retry_at.saturating_duration_since(self.inner.clock.now()));
        let (send, receive) = oneshot::channel();
        thread::Builder::new()
            .name("auth-refresh".into())
            .spawn(move || {
                if let Some(delay) = delay {
                    thread::sleep(delay);
                }
                let result = auth.run_refresh(&slot, generation);
                let mut state = slot.lock().unwrap();
                if Self::check_generation(&state, generation).is_ok() {
                    state.refresh = None;
                }
                let _ = send.send(result);
            })
            .map_err(|_| AuthError::WorkerUnavailable)?;
        let flight = receive
            .map(|result| result.unwrap_or(Err(AuthError::WorkerUnavailable)))
            .boxed()
            .shared();
        state.refresh = Some(flight.clone());
        Ok(flight)
    }

    /// Получает или берёт из памяти ответ IAM и применяет его только к той же сессии.
    fn run_refresh(
        &self,
        slot: &Mutex<AccountState>,
        generation: SessionGeneration,
    ) -> Result<(), AuthError> {
        let (record, refresh_token) = {
            let mut state = slot.lock().unwrap();
            Self::check_generation(&state, generation)?;
            let record = state.record.clone().ok_or(AuthError::SessionUnavailable)?;
            // Если запись секрета не удалась, повторяем её с тем же ответом IAM.
            if let Some(token) = state.pending_token.take() {
                return self.commit_refresh(&mut state, &record, token);
            }
            (record, state.refresh_token.clone())
        };

        let result = self.request_refresh(&record, refresh_token);
        let mut state = slot.lock().unwrap();
        Self::check_generation(&state, generation)?;
        let account_id = &record.context.account_id;
        match result {
            Ok((token, refresh_token)) => {
                state.refresh_token = Some(refresh_token);
                self.commit_refresh(&mut state, &record, token)
            }
            Err(
                AuthError::Iam(IamError::ReauthenticationRequired)
                | AuthError::ReauthenticationRequired,
            ) => {
                state.end(SessionStatus::ReauthenticationRequired);
                self.publish(account_id, &state);
                Err(AuthError::ReauthenticationRequired)
            }
            Err(error) => {
                self.record_temporary_failure(&mut state, &record.context);
                Err(error)
            }
        }
    }

    /// Читает токен обновления из памяти или хранилища и запрашивает новый токен у IAM.
    fn request_refresh(
        &self,
        record: &super::AccountRecord,
        refresh_token: Option<String>,
    ) -> Result<(TimedToken, String), AuthError> {
        let refresh_token = match refresh_token {
            Some(token) => token,
            None => self
                .inner
                .secrets
                .load(&record.secret_key)?
                .filter(|token| !token.is_empty())
                .ok_or(AuthError::ReauthenticationRequired)?,
        };
        let requested_at = self.inner.clock.now();
        let token = self
            .inner
            .iam
            .refresh(&record.context.server, &refresh_token)?;
        let token = TimedToken::new(token, &record.context, requested_at)?;
        Ok((token, refresh_token))
    }

    /// Сохраняет замену токена обновления перед тем, как сделать новый токен доступа доступным.
    fn commit_refresh(
        &self,
        state: &mut AccountState,
        record: &super::AccountRecord,
        token: TimedToken,
    ) -> Result<(), AuthError> {
        if let Some(refresh_token) = token.replacement_refresh()
            && let Err(error) = self.inner.secrets.save(&record.secret_key, refresh_token)
        {
            state.pending_token = Some(token);
            self.record_temporary_failure(state, &record.context);
            return Err(error.into());
        }
        state.install(token);
        self.publish(&record.context.account_id, state);
        Ok(())
    }

    /// Назначает время повтора и публикует статус с учётом срока старого токена.
    fn record_temporary_failure(&self, state: &mut AccountState, context: &OwnerContext) {
        let now = self.inner.clock.now();
        state.retry_at = Some(now + RETRY_DELAY);
        if state
            .access
            .as_ref()
            .is_none_or(|token| now >= token.expires_at)
        {
            state.status = SessionStatus::TemporarilyUnavailable;
        }
        self.publish(&context.account_id, state);
    }
}
