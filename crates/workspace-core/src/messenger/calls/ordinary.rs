//! Отправляет обычное приглашение Jitsi через очередь и проверяет его до открытия.

use std::{
    pin::pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use futures::StreamExt;
use uuid::Uuid;

use crate::{
    auth::Auth,
    context::{OwnerContext, SessionGeneration},
    storage::{HistoryScope, PreparedSend, Storage},
    sync::Sync,
};

use super::{CallError, INVITATION_TIMEOUT, JitsiJoinLink, MessengerSource, invitation_delivered};

/// Ссылка обычного звонка, принятого сервером для точного адреса разговора.
pub struct DeliveredPlainJitsiCall {
    owner: OwnerContext,           // Владелец приглашения.
    generation: SessionGeneration, // Поколение, которым отправили приглашение.
    scope: HistoryScope,           // Точный стрим и тема.
    access_revision: i64,          // Версия доступа до постановки приглашения в очередь.
    snapshot: PreparedSend,        // Снимок очереди, с которым сверяем историю.
    link: JitsiJoinLink,           // Адрес конференции из настроек Workspace.
    auth: Auth,                    // Проверяет вход перед открытием.
    storage: Arc<Storage>,         // Проверяет, что принятая строка не изменилась.
}

impl DeliveredPlainJitsiCall {
    /// Передаёт адрес адаптеру, пока поколение, доступ и тело сообщения действуют.
    pub fn with_url<T>(&self, open: impl FnOnce(&str) -> T) -> Option<T> {
        self.auth
            .with_active_generation(&self.owner, self.generation, || {
                self.verify_access().ok()?;
                self.storage
                    .matches_delivered_body(&self.owner, &self.snapshot)
                    .ok()?
                    .then(|| open(self.link.as_str()))
            })
            .ok()
            .flatten()
    }

    /// Останавливает ожидание после сброса или отзыва доступа к разговору.
    fn verify_access(&self) -> Result<(), CallError> {
        if self
            .storage
            .media_access_revision(&self.owner, self.scope)?
            != self.access_revision
        {
            return Err(CallError::Unavailable);
        }
        Ok(())
    }
}

impl MessengerSource {
    /// Сверяет тему, отправляет только ссылку и ждёт точную квитанцию из очереди.
    pub async fn send_jitsi_invitation(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        route: HistoryScope,
        expected_topic: Uuid,
    ) -> Result<DeliveredPlainJitsiCall, CallError> {
        if expected_topic.is_nil() || route.stream_id.is_nil() {
            return Err(CallError::Unavailable);
        }
        let scope = HistoryScope {
            stream_id: route.stream_id,
            topic_id: Some(expected_topic),
        };
        let auth = sync.auth();
        let storage = sync.storage();
        let access_revision = auth.with_active_generation(owner, generation, || {
            storage.media_access_revision(owner, scope)
        })??;
        if let Some(topic) = route.topic_id {
            if expected_topic != topic {
                return Err(CallError::Unavailable);
            }
            self.check_topic_send(sync, owner, generation, route.stream_id, topic)
                .await?;
        } else if self
            .resolve_direct_send(sync, owner, generation, route.stream_id)
            .await?
            != expected_topic
        {
            return Err(CallError::Unavailable);
        }
        let settings = self.jitsi_settings(sync, owner, generation).await?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CallError::Unavailable)?
            .as_millis();
        let link = settings.link_for_route(owner, route.stream_id, expected_topic, now)?;
        let mut updates = sync.subscribe();
        let mut sessions = pin!(auth.subscribe());
        if storage.media_access_revision(owner, scope)? != access_revision {
            return Err(CallError::Unavailable);
        }
        let snapshot = sync.prepare_send(
            owner,
            generation,
            route.stream_id,
            expected_topic,
            link.as_str().to_owned(),
        )?;
        log::debug!(
            "messenger_call event=invitation_queued message_id={}",
            snapshot.public_id
        );
        let call = DeliveredPlainJitsiCall {
            owner: owner.clone(),
            generation,
            scope,
            access_revision,
            snapshot,
            link,
            auth: auth.clone(),
            storage: Arc::clone(storage),
        };
        let waiting = async {
            loop {
                let ready = auth.with_active_generation(owner, generation, || {
                    call.verify_access()?;
                    invitation_delivered(storage, owner, scope, &call.snapshot)
                })??;
                if ready {
                    return Ok::<_, CallError>(());
                }
                tokio::select! {
                    _ = updates.recv() => {}
                    _ = sessions.next() => {}
                }
            }
        };
        tokio::time::timeout(INVITATION_TIMEOUT, waiting)
            .await
            .map_err(|_| CallError::Unconfirmed)??;
        log::debug!(
            "messenger_call event=invitation_confirmed message_id={}",
            call.snapshot.public_id
        );
        Ok(call)
    }
}
