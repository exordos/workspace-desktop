//! Читает настройки Jitsi, распознаёт ссылки и отправляет приглашения в чаты.
//! Ссылку нового звонка можно открыть только после подтверждения того же сообщения.

mod ordinary;

use std::{
    pin::pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::StreamExt;
use reqwest::Url;
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    auth::{Auth, AuthError},
    context::{OwnerContext, SessionGeneration},
    runtime::SourceError,
    storage::{
        DeliveryStatus, HistoryMessage, HistoryScope, MessageBody, PreparedSend, PreparedSendBody,
        Storage, StorageError,
    },
    sync::{PrepareSendError, Sync},
};

use super::{
    MessengerSource,
    encrypted_send::EncryptedMessageError,
    encryption::{DecryptionError, EncryptionActivation, MessageReadPermission},
    history::OpenHistoryError,
    quote::QuoteSourceFence,
    references::external_links,
    send::DirectSendError,
};

const INVITATION_TIMEOUT: Duration = Duration::from_secs(30);
const PUBLIC_JITSI_HOST: &str = "meet.jit.si";

pub use ordinary::DeliveredPlainJitsiCall;

/// Разрешённый адрес конференций этой организации.
pub struct JitsiSettings(Url); // HTTPS-источник из настроек Workspace.

/// Проверенная ссылка на комнату, которая остаётся в памяти представления.
pub struct JitsiJoinLink(Zeroizing<String>);

impl JitsiJoinLink {
    /// Даёт адаптеру точный HTTPS-адрес принятой комнаты.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl JitsiSettings {
    /// Ищет ссылку на комнату организации или публичного Jitsi в отображаемом сообщении.
    pub fn recognize_invitation(&self, markdown: &str) -> Option<JitsiJoinLink> {
        external_links(markdown)
            .into_iter()
            .find_map(|link| self.meeting_link(&link.url))
    }

    /// Проверяет точный источник и одно имя комнаты без произвольных параметров внешнего клиента.
    fn meeting_link(&self, raw: &str) -> Option<JitsiJoinLink> {
        let url = Url::parse(raw).ok()?;
        if !safe_https(&url)
            || (url.origin() != self.0.origin()
                && !(url.host_str() == Some(PUBLIC_JITSI_HOST) && url.port().is_none()))
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return None;
        }
        let room = url.path().strip_prefix('/')?.trim_end_matches('/');
        if room.is_empty()
            || !room
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return None;
        }
        Some(JitsiJoinLink(Zeroizing::new(url.into())))
    }

    /// Собирает приглашение по устойчивому формату встреч Workspace.
    fn link_for_route(
        &self,
        owner: &OwnerContext,
        stream_id: Uuid,
        topic_id: Uuid,
        unix_ms: u128,
    ) -> Result<JitsiJoinLink, CallError> {
        let stream = stream_id.to_string();
        let topic = topic_id.to_string();
        let timestamp = unix_ms.to_string();
        let room = [
            "workspace",
            &owner.organization_id,
            &owner.project_id,
            &stream,
            &topic,
            &timestamp,
        ]
        .map(room_segment)
        .join("-");
        self.0
            .join(&room)
            .ok()
            .and_then(|url| self.meeting_link(url.as_str()))
            .ok_or(CallError::Unavailable)
    }
}

/// Приглашение в очереди и условия, при которых текущий экран сможет открыть комнату.
pub struct PendingJitsiCall {
    activation: EncryptionActivation, // Точный выбранный вход и устройство.
    scope: HistoryScope,              // Подтверждаемая тема приглашения.
    access_revision: i64,             // Версия отзыва доступа к теме или стриму.
    snapshot: PreparedSend,           // Исходный UUID и шифротекст для проверки доставки.
    link: JitsiJoinLink,              // Открытая ссылка только в памяти живой операции.
    permission: MessageReadPermission, // Закрывает ссылку после отзыва поколения.
}

impl PendingJitsiCall {
    /// Возвращает UUID исходящей строки, доступный до получения квитанции.
    pub fn message_id(&self) -> Uuid {
        self.snapshot.public_id
    }

    /// Проверяет, что исходная тема и разрешение точного ключа всё ещё доступны.
    fn verify(&self, storage: &Storage) -> Result<(), CallError> {
        if self.permission.with_current(|| ()).is_some()
            && storage.media_access_revision(&self.activation.session().owner, self.scope)?
                == self.access_revision
        {
            return Ok(());
        }
        Err(CallError::Unavailable)
    }
}

/// Подтверждённое приглашение, которое можно передать системному средству открытия.
pub struct DeliveredJitsiCall {
    invitation: PendingJitsiCall, // Адрес и условия исходного сообщения.
    auth: Auth,                   // Авторизация для проверки перед открытием.
    storage: Arc<Storage>,        // Каноническая строка и версия доступа.
}

impl DeliveredJitsiCall {
    /// Выполняет только короткое действие системного адаптера под живыми разрешениями.
    pub fn with_url<T>(&self, open: impl FnOnce(&str) -> T) -> Option<T> {
        let call = &self.invitation;
        let session = call.activation.session();
        self.auth
            .with_active_generation(&session.owner, session.generation, || {
                call.verify(&self.storage).ok()?;
                if !self
                    .storage
                    .matches_delivered_body(&session.owner, &call.snapshot)
                    .ok()?
                {
                    return None;
                }
                call.permission.with_current(|| open(call.link.as_str()))
            })
            .ok()
            .flatten()
    }
}

/// Разрешение открыть комнату по проверенной защищённой строке истории.
pub struct ConfirmedJitsiJoin {
    activation: EncryptionActivation, // Точный вход и выбор пользователя.
    expected: HistoryMessage,         // Канонический шифротекст и его серверная версия.
    fence: QuoteSourceFence,          // Версии оригинала и доступа к теме.
    permission: MessageReadPermission, // Выбранный вход всё ещё владеет исходным ключом.
    link: JitsiJoinLink,              // Проверенный адрес в памяти короткой операции.
    auth: Auth,                       // Проверка поколения до системного действия.
    storage: Arc<Storage>,            // Локальные версии сообщения и доступа.
}

impl ConfirmedJitsiJoin {
    /// Передаёт адрес адаптеру только пока строка, доступ и ключ соответствуют проверке.
    pub fn with_url<T>(&self, open: impl FnOnce(&str) -> T) -> Option<T> {
        let session = self.activation.session();
        let scope = HistoryScope {
            stream_id: self.expected.stream_id,
            topic_id: Some(self.expected.topic_id),
        };
        self.auth
            .with_active_generation(&session.owner, session.generation, || {
                let fence = QuoteSourceFence::capture(
                    &self.storage,
                    &session.owner,
                    scope,
                    self.expected.id,
                )
                .ok()?;
                if fence != self.fence
                    || !self
                        .storage
                        .matches_encrypted_edit(&session.owner, &self.expected)
                        .ok()?
                {
                    return None;
                }
                self.permission.with_current(|| open(self.link.as_str()))
            })
            .ok()
            .flatten()
    }
}

/// Причина, по которой нельзя подготовить или открыть приглашение.
#[derive(Debug, Error)]
pub enum CallError {
    #[error("Jitsi is unavailable for this conversation")]
    Unavailable, // Нет настроек, доступа, активного выбора или исходного ключа.
    #[error("the invitation was not confirmed")]
    Unconfirmed, // Первая попытка завершилась без точной квитанции.
    #[error(transparent)]
    Auth(#[from] AuthError), // Вход завершился или сменилось его поколение.
    #[error(transparent)]
    Source(#[from] SourceError), // Не удалось прочитать настройки сервера.
    #[error(transparent)]
    Storage(#[from] StorageError), // Не удалось сверить исходное размещение или очередь.
    #[error(transparent)]
    Send(#[from] EncryptedMessageError), // Не удалось подготовить защищённую отправку.
    #[error(transparent)]
    Destination(#[from] DirectSendError), // Сервер больше не разрешает запись в выбранную тему.
    #[error(transparent)]
    Queue(#[from] PrepareSendError), // Не удалось поставить приглашение в очередь.
    #[error(transparent)]
    Key(#[from] DecryptionError), // Исходный конверт нельзя подтвердить локальным ключом.
    #[error(transparent)]
    History(#[from] OpenHistoryError), // Исходное сообщение или его тема больше недоступны.
}

impl MessengerSource {
    /// Читает публичный HTTPS-источник конференций и сверяет исходное поколение входа.
    pub async fn jitsi_settings(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<JitsiSettings, CallError> {
        let auth = sync.auth();
        auth.with_active_generation(owner, generation, || ())?;
        let settings = self.rest.public_settings(&owner.server).await;
        auth.with_active_generation(owner, generation, || ())?;
        let settings = settings?;
        let origin = settings
            .get("meet_url")
            .and_then(Value::as_str)
            .and_then(|text| Url::parse(text.trim()).ok())
            .filter(|url| {
                safe_https(url)
                    && url.path() == "/"
                    && url.query().is_none()
                    && url.fragment().is_none()
            })
            .ok_or(CallError::Unavailable)?;
        Ok(JitsiSettings(origin))
    }

    /// Готовит вход только по заново подтверждённой версии защищённого сообщения.
    pub async fn confirm_jitsi_join(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        scope: HistoryScope,
        expected: &HistoryMessage,
    ) -> Result<ConfirmedJitsiJoin, CallError> {
        let MessageBody::Encrypted { key_id, ciphertext } = &expected.body else {
            return Err(CallError::Unavailable);
        };
        let profile = self
            .encryption_profile(activation)
            .map_err(EncryptedMessageError::from)?;
        let session = activation.session();
        let settings = self
            .jitsi_settings(sync, &session.owner, session.generation)
            .await?;
        let (original, fence) = self
            .quote_original_with_fence(
                sync,
                &session.owner,
                session.generation,
                Some(scope),
                expected.id,
            )
            .await?;
        if !original.same_content(expected) {
            return Err(CallError::Unavailable);
        }
        let accepted = profile.keys.accept(
            profile
                .keys
                .prepare(activation, expected.stream_id, *key_id)?
                .decrypt(ciphertext),
        )?;
        let link = accepted
            .with_body(|markdown| settings.recognize_invitation(markdown))
            .flatten()
            .ok_or(CallError::Unavailable)?;
        Ok(ConfirmedJitsiJoin {
            activation: activation.clone(),
            expected: original,
            fence,
            permission: accepted.into_permission(),
            link,
            auth: sync.auth().clone(),
            storage: Arc::clone(sync.storage()),
        })
    }

    /// Готовит приглашение точно так же, как новое защищённое сообщение.
    pub async fn prepare_encrypted_call(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        scope: HistoryScope,
        expected_topic: Uuid,
    ) -> Result<PendingJitsiCall, CallError> {
        if expected_topic.is_nil() || scope.stream_id.is_nil() {
            return Err(CallError::Unavailable);
        }
        let session = activation.session();
        let selected_scope = HistoryScope {
            stream_id: scope.stream_id,
            topic_id: Some(expected_topic),
        };
        let access_revision =
            sync.auth()
                .with_active_generation(&session.owner, session.generation, || {
                    sync.storage()
                        .media_access_revision(&session.owner, selected_scope)
                })??;
        let settings = self
            .jitsi_settings(sync, &session.owner, session.generation)
            .await?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CallError::Unavailable)?
            .as_millis();
        let link = settings.link_for_route(&session.owner, scope.stream_id, expected_topic, now)?;
        let snapshot = self
            .prepare_encrypted_send_to_expected(
                sync,
                activation,
                scope,
                expected_topic,
                link.as_str().to_owned(),
            )
            .await?;
        let profile = self
            .encryption_profile(activation)
            .map_err(EncryptedMessageError::from)?;
        let PreparedSendBody::Encrypted {
            key_id, ciphertext, ..
        } = &snapshot.body
        else {
            return Err(CallError::Unavailable);
        };
        let accepted = profile.keys.accept(
            profile
                .keys
                .prepare(activation, snapshot.stream_id, *key_id)?
                .decrypt(ciphertext),
        )?;
        if accepted.with_body(|text| text == link.as_str()) != Some(true) {
            return Err(CallError::Unavailable);
        }
        Ok(PendingJitsiCall {
            activation: activation.clone(),
            scope: selected_scope,
            access_revision,
            snapshot,
            link,
            permission: accepted.into_permission(),
        })
    }

    /// Ждёт точной доставки, пока сохранились выбранный вход, доступ и исходный ключ.
    pub async fn wait_for_call_invitation(
        &self,
        sync: &Sync,
        invitation: PendingJitsiCall,
    ) -> Result<DeliveredJitsiCall, CallError> {
        let profile = self
            .encryption_profile(&invitation.activation)
            .map_err(EncryptedMessageError::from)?;
        let auth = sync.auth();
        let session = invitation.activation.session();
        let mut updates = sync.subscribe();
        let mut keys = profile.keys.subscribe();
        let mut selection = profile.keys.observe_selection();
        let mut sessions = pin!(auth.subscribe());
        let waiting = async {
            loop {
                let ready =
                    auth.with_active_generation(&session.owner, session.generation, || {
                        call_delivered(sync.storage(), &invitation)
                    })??;
                if ready {
                    return Ok::<_, CallError>(());
                }
                tokio::select! {
                    _ = updates.recv() => {}
                    _ = keys.recv() => {}
                    _ = selection.changed() => {}
                    _ = sessions.next() => {}
                }
            }
        };
        tokio::time::timeout(INVITATION_TIMEOUT, waiting)
            .await
            .map_err(|_| CallError::Unconfirmed)??;
        Ok(DeliveredJitsiCall {
            invitation,
            auth: auth.clone(),
            storage: Arc::clone(sync.storage()),
        })
    }
}

/// Сверяет доставленное размещение с неизменным снимком этой очереди.
fn call_delivered(storage: &Storage, call: &PendingJitsiCall) -> Result<bool, CallError> {
    call.verify(storage)?;
    invitation_delivered(
        storage,
        &call.activation.session().owner,
        call.scope,
        &call.snapshot,
    )
}

/// Ждёт исходящую попытку и сверяет точный результат по канонической истории.
fn invitation_delivered(
    storage: &Storage,
    owner: &OwnerContext,
    scope: HistoryScope,
    snapshot: &PreparedSend,
) -> Result<bool, CallError> {
    let pending = storage
        .outbox(owner, Some(scope))?
        .into_iter()
        .find(|entry| entry.snapshot.canonical_id == snapshot.canonical_id);
    match pending {
        Some(entry)
            if matches!(
                entry.status,
                DeliveryStatus::Waiting | DeliveryStatus::Sending
            ) =>
        {
            Ok(false)
        }
        Some(_) => Err(CallError::Unconfirmed),
        None if storage.matches_delivered_body(owner, snapshot)? => Ok(true),
        None => Err(CallError::Unconfirmed),
    }
}

/// Принимает HTTPS-адрес с известным источником и без данных пользователя в URL.
fn safe_https(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

/// Сохраняет ASCII-части имени и нормализует разделители, как Electron.
fn room_segment(value: &str) -> String {
    let cleaned = value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_ascii_lowercase();
    if cleaned.is_empty() {
        "room".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Совпадает с нормализацией имени комнаты в Electron.
    #[test]
    fn normalizes_workspace_room_segments() {
        assert_eq!(room_segment("  A--B_C/(D)  "), "a-b-c-d");
        assert_eq!(room_segment("???⚡"), "room");
        assert_eq!(room_segment("44b5-1153"), "44b5-1153");
    }
}
