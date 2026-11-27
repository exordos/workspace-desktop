//! Передаёт запрос перехода из ленты оболочке и проверяет, что источник ещё действует.
//! Если цели нет в ленте, запрашивает у Core точный адрес по UUID в том же входе.

use gpui::{Context, EventEmitter};
use uuid::Uuid;
use workspace_core::storage::HistoryScope;

use super::{
    Conversation, MessageHistoryError,
    protected_quotes::{QuoteSlot, QuoteText},
    references::ReferenceSource,
};
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
    OrdinaryQuote {
        row: Uuid,       // Сообщение, в котором расположена цитата.
        caption: bool,   // Цитата находится в подписи вложения.
        position: usize, // Позиция цитаты в теле или подписи.
        target: Uuid,    // UUID оригинала этой цитаты.
    },
    ProtectedQuote {
        slot: QuoteSlot,   // Точная позиция цитаты в защищённом сообщении.
        access: QuoteText, // Отзываемый доступ к цитирующему и исходному сообщениям.
        target: Uuid,      // UUID оригинала, проверенный вместе с токеном доступа.
    },
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

    /// Запоминает точную позицию обычной цитаты для проверки после сетевого ожидания.
    fn ordinary_quote(row: Uuid, caption: bool, position: usize, target: Uuid) -> Self {
        Self(Source::OrdinaryQuote {
            row,
            caption,
            position,
            target,
        })
    }

    /// Сохраняет отзывный доступ к точной защищённой цитате.
    fn protected_quote(slot: QuoteSlot, access: QuoteText, target: Uuid) -> Self {
        Self(Source::ProtectedQuote {
            slot,
            access,
            target,
        })
    }

    /// Возвращает источник для отмены перехода при правке или удалении сообщения.
    pub(in crate::gpui_client) fn row(&self) -> Uuid {
        match &self.0 {
            Source::Forward { row, .. } => *row,
            Source::Reference(source) => source.row(),
            Source::OrdinaryQuote { row, .. } => *row,
            Source::ProtectedQuote { slot, .. } => slot.message,
        }
    }

    /// Отбрасывает переход, если исходная строка исчезла, изменилась или недоступна.
    pub(in crate::gpui_client) fn is_current(&self, view: &Conversation) -> bool {
        match &self.0 {
            Source::Forward { row, target } => {
                view.has_access() && view.viewport.has_forward_source(*row, *target)
            }
            Source::Reference(source) => source.is_current(view),
            Source::OrdinaryQuote {
                row,
                caption,
                position,
                target,
            } => {
                view.has_access()
                    && view
                        .viewport
                        .has_quote_source(*row, *caption, *position, *target)
            }
            Source::ProtectedQuote {
                slot,
                access,
                target,
            } => {
                view.has_access()
                    && view.encryption.as_ref().is_some_and(|encryption| {
                        encryption.quotes.has_current_source(*slot, access, *target)
                    })
            }
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

    /// Открывает оригинал обычной цитаты, только если её точный слот ещё не изменился.
    pub(super) fn open_history_quote(
        &self,
        row: Uuid,
        caption: bool,
        position: usize,
        target: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.emit_message_target(
            target,
            MessageNavigationSource::ordinary_quote(row, caption, position, target),
            cx,
        );
    }

    /// Открывает оригинал защищённой цитаты с токеном, который повторно проверяется после ожиданий.
    pub(super) fn open_protected_quote(
        &self,
        slot: QuoteSlot,
        access: QuoteText,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = access.current_source() else {
            return;
        };
        self.emit_message_target(
            target,
            MessageNavigationSource::protected_quote(slot, access, target),
            cx,
        );
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
