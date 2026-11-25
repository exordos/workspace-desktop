//! Передаёт запрос перехода из ленты оболочке и проверяет, что источник ещё действует.
//! Если цели нет в ленте, запрашивает у Core точный адрес по UUID в том же входе.

use gpui::{Context, EventEmitter};
use uuid::Uuid;
use workspace_core::storage::HistoryScope;

use super::{Conversation, MessageHistoryError, references::ReferenceSource};
use crate::gpui_client::{contracts::ConversationAddress, runtime::RuntimeClient};

/// Действие пользователя в текущей ленте, которое оболочка может принять или отменить.
#[derive(Clone)]
pub(in crate::gpui_client) struct OpenMessageTarget {
    pub address: ConversationAddress, // Точный экран, в котором нажали на источник.
    pub id: Uuid,                     // Публичный UUID сообщения, которое нужно показать.
    pub source: MessageNavigationSource, // Проверка источника перед применением перехода.
}

impl EventEmitter<OpenMessageTarget> for Conversation {}

/// Сохраняет только сведения, нужные для повторной проверки источника.
#[derive(Clone)]
pub(in crate::gpui_client) struct MessageNavigationSource(Source); // Исходный элемент истории.

/// Отличает заголовок пересылки от обычной или защищённой ссылки.
#[derive(Clone)]
enum Source {
    Forward {
        row: Uuid,    // Бабл, в котором показана пересылка.
        target: Uuid, // Оригинал, на который ведёт заголовок пересылки.
    },
    Reference(ReferenceSource), // Исходная ссылка или разрешение на чтение защищённого тела.
}

impl MessageNavigationSource {
    /// Запоминает конкретный оригинал пересылки в подтверждённом бабле.
    pub(super) fn forward(row: Uuid, target: Uuid) -> Self {
        Self(Source::Forward { row, target })
    }

    /// Сохраняет проверку ссылки на сообщение и действующих разрешений на её чтение.
    pub(super) fn reference(source: ReferenceSource) -> Self {
        Self(Source::Reference(source))
    }

    /// Возвращает источник для отмены перехода при правке или удалении сообщения.
    pub(in crate::gpui_client) fn row(&self) -> Uuid {
        match &self.0 {
            Source::Forward { row, .. } => *row,
            Source::Reference(source) => source.row(),
        }
    }

    /// Отбрасывает переход, если исходная строка исчезла, изменилась или недоступна.
    pub(in crate::gpui_client) fn is_current(&self, view: &Conversation) -> bool {
        match &self.0 {
            Source::Forward { row, target } => {
                view.has_access() && view.viewport.has_forward_source(*row, *target)
            }
            Source::Reference(source) => source.is_current(view),
        }
    }
}

impl Conversation {
    /// Сразу передаёт действительный запрос, чтобы оболочка могла отменить предыдущий переход.
    pub(super) fn emit_message_target(
        &self,
        id: Uuid,
        source: MessageNavigationSource,
        cx: &mut Context<Self>,
    ) {
        if source.is_current(self) {
            cx.emit(OpenMessageTarget {
                address: self.address.clone(),
                id,
                source,
            });
        }
    }
}

/// Получает подтверждённый адрес цели. Ошибку можно безопасно показать без текста REST.
pub(in crate::gpui_client) async fn resolve_message_target(
    runtime: RuntimeClient,
    address: ConversationAddress,
    id: Uuid,
) -> Result<HistoryScope, MessageHistoryError> {
    runtime
        .conversation(address, move |services, address| async move {
            services
                .messenger
                .resolve_message_history_scope(
                    &services.sync,
                    &address.route.owner,
                    address.generation,
                    id,
                )
                .await
        })
        .await
        .value
        .map_err(|_| MessageHistoryError::Unavailable)?
        .map_err(MessageHistoryError::from)
}
