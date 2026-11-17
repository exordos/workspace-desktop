//! Обращается к core с рабочего потока и возвращает только безопасный итог для редактора.

use std::sync::Arc;

use futures::future::BoxFuture;
use uuid::Uuid;
use workspace_core::messenger::send::DirectSendError;

use super::Submission;
use crate::gpui_client::{
    contracts::{ConversationAddress, ConversationReply},
    runtime::RuntimeClient,
};

/// Уточняет причину недоступности отправки без деталей сетевой ошибки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gpui_client) enum DestinationError {
    AccessLost,            // Стрим или тема недоступны исходному входу.
    EncryptionUnavailable, // Сервер не разрешил отправку открытого текста.
    TopicUnavailable,      // Сервер не предоставил пригодный UUID темы.
    TopicClosed,           // Отправку ограничивает завершённая тема канала.
    Unavailable,           // Проверка цели временно не завершилась.
}

impl DestinationError {
    /// Даёт короткое пояснение рядом с недоступной кнопкой отправки.
    pub(super) fn text(self) -> &'static str {
        match self {
            Self::AccessLost => "Чат или тема для отправки недоступны",
            Self::EncryptionUnavailable => "Отправка открытого текста недоступна",
            Self::TopicUnavailable => "Сервер не указал тему для отправки",
            Self::TopicClosed => "Тема закрыта для отправки",
            Self::Unavailable => "Не удалось проверить возможность отправки",
        }
    }
}

impl From<DirectSendError> for DestinationError {
    /// Отбрасывает детали токенов, адресов и HTTP-ответов.
    fn from(error: DirectSendError) -> Self {
        match error {
            DirectSendError::AccessLost => Self::AccessLost,
            DirectSendError::EncryptionUnavailable => Self::EncryptionUnavailable,
            DirectSendError::TopicUnavailable => Self::TopicUnavailable,
            DirectSendError::TopicClosed => Self::TopicClosed,
            DirectSendError::Auth(_) | DirectSendError::Source(_) => Self::Unavailable,
        }
    }
}

/// Возвращает проверенную тему с исходным адресом разговора.
type Resolve = Arc<
    dyn Fn(
        ConversationAddress,
        Uuid,
    ) -> BoxFuture<'static, ConversationReply<Result<Uuid, DestinationError>>>,
>;

/// Возвращает записанный публичный UUID, если core успел сохранить сообщение в SQLite.
type Save = Arc<dyn Fn(Submission) -> BoxFuture<'static, ConversationReply<Option<Uuid>>>>;

/// Запускает операции отправки; в рабочем окне обе выполняет RuntimeClient.
pub(in crate::gpui_client) struct SendSource {
    pub resolve: Resolve, // Проверяет, куда можно отправить обычный текст.
    pub save: Save,       // Сохраняет конкретный снимок отправки в очередь.
}

impl SendSource {
    /// Связывает отправку с тем же исполнителем, на котором работает мессенджер.
    pub fn runtime(runtime: RuntimeClient) -> Self {
        let worker = runtime.clone();
        Self {
            resolve: Arc::new(move |address, op_id| {
                let worker = worker.clone();
                Box::pin(async move {
                    let reply = worker
                        .conversation(address, move |services, address| async move {
                            log::debug!("gpui_send op_id={op_id} event=destination_worker_started");
                            let result = if let Some(topic) = address.route.scope.topic_id {
                                services
                                    .messenger
                                    .check_topic_send(
                                        &services.sync,
                                        &address.route.owner,
                                        address.generation,
                                        address.route.scope.stream_id,
                                        topic,
                                    )
                                    .await
                                    .map(|()| topic)
                            } else {
                                services
                                    .messenger
                                    .resolve_direct_send(
                                        &services.sync,
                                        &address.route.owner,
                                        address.generation,
                                        address.route.scope.stream_id,
                                    )
                                    .await
                            };
                            result.map_err(DestinationError::from)
                        })
                        .await;
                    ConversationReply {
                        address: reply.address,
                        value: reply.value.unwrap_or(Err(DestinationError::Unavailable)),
                    }
                })
            }),
            save: Arc::new(move |submission| {
                let worker = runtime.clone();
                Box::pin(async move {
                    let op_id = submission.attempt.op_id;
                    let reply = worker
                        .conversation(submission.address, move |services, address| async move {
                            log::debug!("gpui_send op_id={op_id} event=queue_write_started");
                            // Тема могла закрыться, пока пользователь редактировал черновик.
                            if let Some(topic) = address.route.scope.topic_id
                                && (topic != submission.topic
                                    || services
                                        .messenger
                                        .check_topic_send(
                                            &services.sync,
                                            &address.route.owner,
                                            address.generation,
                                            address.route.scope.stream_id,
                                            topic,
                                        )
                                        .await
                                        .is_err())
                            {
                                return None;
                            }
                            services
                                .sync
                                .prepare_send(
                                    &address.route.owner,
                                    address.generation,
                                    address.route.scope.stream_id,
                                    submission.topic,
                                    submission.markdown,
                                )
                                .ok()
                                .map(|record| record.public_id)
                        })
                        .await;
                    let public_id = reply.value.ok().flatten();
                    log::debug!(
                        "gpui_send op_id={op_id} event=queue_write_finished saved={} message_id={public_id:?}",
                        public_id.is_some(),
                    );
                    ConversationReply {
                        address: reply.address,
                        value: public_id,
                    }
                })
            }),
        }
    }
}
