//! Связывает запрос сведений с рабочим потоком и исходным адресом разговора.

use workspace_core::messenger::details::{ConversationDetails, DetailsError};

use crate::gpui_client::{
    contracts::{ConversationAddress, ConversationReply},
    runtime::RuntimeClient,
};

/// Причина, которую панель может показать без сетевых или личных данных.
#[derive(Clone, Copy)]
pub(super) enum Problem {
    AccessLost,         // Отзыв доступа закрывает панель и историю.
    Unsupported,        // У этого типа разговора пока нет карточки.
    ProfileUnavailable, // Собеседник подтверждён, но сервер не показывает профиль.
    Unavailable,        // Временная ошибка, для которой можно предложить повтор.
}

/// Запрашивает сведения под владельцем и поколением исходного разговора.
pub(super) async fn load(
    runtime: RuntimeClient,
    address: ConversationAddress,
) -> ConversationReply<Result<ConversationDetails, Problem>> {
    let reply = runtime
        .conversation(address, |services, address| async move {
            let route = address.route;
            services
                .messenger
                .conversation_details(
                    &services.auth,
                    &route.owner,
                    address.generation,
                    route.scope,
                )
                .await
        })
        .await;
    ConversationReply {
        address: reply.address,
        value: match reply.value {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(DetailsError::AccessLost)) => Err(Problem::AccessLost),
            Ok(Err(DetailsError::Unsupported)) => Err(Problem::Unsupported),
            Ok(Err(DetailsError::ProfileUnavailable)) => Err(Problem::ProfileUnavailable),
            Ok(Err(_)) | Err(_) => Err(Problem::Unavailable),
        },
    }
}
