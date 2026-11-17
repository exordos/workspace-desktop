//! Хранит ввод открытого разговора и очищает его после сохранения той же версии текста.

use std::hash::{Hash, Hasher};

use futures::{Stream, stream};
use iced::advanced::widget::Id;
use iced::widget::text_editor::{Action, Content};
use iced::{Element, Subscription, Task};
use uuid::Uuid;
use workspace_core::context::{OwnerContext, SessionGeneration};
use workspace_core::messenger::send::DirectSendError;
use workspace_core::storage::{DeliveryStatus, OutboxEntry, SendBlock};

use crate::ui_kit::{
    MessageComposerOptions, MessageDelivery, MessageDirection, message_bubble, message_composer,
};

use super::{HistoryState, Message, Shell, Sources};

/// Максимальный размер текста в кодовых точках, как в Workspace UI.
const MAX_MESSAGE_CHARS: usize = 40_000;

/// Возможность отправки в текущую личку после проверки сервером.
#[derive(Default)]
pub(super) enum Address {
    #[default]
    Unchecked, // Запрос ещё не начинался.
    Ready(Uuid),               // Точная тема для отправки текста.
    Unavailable(&'static str), // Видимое пользователю объяснение отказа.
}

/// Поле ввода, подтверждённый адрес и локальная очередь видимого разговора.
#[derive(Default)]
pub(super) struct ConversationSend {
    pub(super) revision: u64, // Идентификатор редактора: переход к хвосту оставляет тот же черновик.
    pub(super) composer: Composer, // Черновик текущего экрана.
    pub(super) address: Address, // Результат проверки для текущего маршрута.
    pub(super) outbox: Vec<OutboxEntry>, // Записи отправки в области истории.
}

impl Shell {
    /// Проверяет адрес после открытия истории; при смене экрана подписка отменяет запрос.
    pub(super) fn composer_subscription(&self) -> Subscription<Message> {
        let Some(screen) = self.visible_history() else {
            return Subscription::none();
        };
        if screen.route.scope.topic_id.is_some()
            || !matches!(screen.state, HistoryState::Ready(_))
            || !matches!(screen.send.address, Address::Unchecked)
        {
            return Subscription::none();
        }
        Subscription::run_with(
            AddressRequest {
                sources: self.sources.clone(),
                owner: screen.route.owner.clone(),
                generation: screen.generation,
                stream: screen.route.scope.stream_id,
                revision: self.navigation_revision,
            },
            AddressRequest::resolve,
        )
    }

    /// Перечитывает записи очереди только для действующего и доступного экрана.
    pub(super) fn refresh_outbox(&mut self) {
        let Some(screen) = self.visible_history() else {
            return;
        };
        if !matches!(screen.state, HistoryState::Ready(_)) {
            return;
        }
        match self
            .sources
            .0
            .sync
            .outbox(&screen.route.owner, screen.generation, screen.route.scope)
        {
            Ok(entries) => self.history.as_mut().unwrap().send.outbox = entries,
            Err(error) => log::warn!("Не удалось перечитать очередь отправки: {error}"),
        }
    }

    /// Сохраняет текст с тем адресом и входом, которые были открыты в момент клика.
    pub(super) fn send_conversation_text(&mut self, revision: u64) -> Task<Message> {
        let Some(screen) = self.ready_send_screen(revision) else {
            return Task::none();
        };
        let Address::Ready(topic) = screen.send.address else {
            return Task::none();
        };
        let route = screen.route.clone();
        let generation = screen.generation;
        let revision = screen.send.revision;
        let Some(Submission { version, markdown }) = screen.send.composer.begin() else {
            return Task::none();
        };
        let source = self.sources.0.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    source.sync.prepare_send(
                        &route.owner,
                        generation,
                        route.scope.stream_id,
                        topic,
                        markdown,
                    )
                })
                .await
                .is_ok_and(|result| result.is_ok())
            },
            move |saved| Message::ConversationTextSaved {
                revision,
                version,
                saved,
            },
        )
    }

    /// Завершает запись для того же редактора, даже пока его вход временно отключён.
    pub(super) fn finish_conversation_text(
        &mut self,
        revision: u64,
        version: u64,
        saved: bool,
    ) -> Task<Message> {
        self.accounts.replace(self.sources.0.snapshot());
        let Some(screen) = self.history.as_ref() else {
            return Task::none();
        };
        if screen.send.revision != revision || !self.is_current_history_owner(screen) {
            return Task::none();
        }
        self.history
            .as_mut()
            .unwrap()
            .send
            .composer
            .finish(version, saved);
        if saved {
            self.queue_history_patch(false)
        } else {
            Task::none()
        }
    }

    /// Принимает результат только для открытого перехода с действующим поколением.
    pub(super) fn ready_send_screen(&mut self, revision: u64) -> Option<&mut super::HistoryScreen> {
        if revision != self.navigation_revision {
            return None;
        }
        self.accounts.replace(self.sources.0.snapshot());
        if !matches!(self.visible_history()?.state, HistoryState::Ready(_)) {
            return None;
        }
        self.history.as_mut()
    }
}

/// Тот же запрос существует, пока экран, владелец и поколение не изменились.
#[derive(Clone)]
struct AddressRequest {
    sources: Sources,              // Auth, Sync и штатный источник мессенджера.
    owner: OwnerContext,           // Владелец запроса.
    generation: SessionGeneration, // Его действующее поколение.
    stream: Uuid,                  // Личный стрим, в котором открыт редактор.
    revision: u64,                 // Переход, к которому относится ответ.
}

impl AddressRequest {
    /// Передаёт единственный результат запроса оболочке.
    fn resolve(&self) -> impl Stream<Item = Message> + use<> {
        let request = self.clone();
        stream::once(async move {
            let source = request.sources.0;
            let result = source
                .messenger
                .resolve_direct_send(
                    &source.sync,
                    &request.owner,
                    request.generation,
                    request.stream,
                )
                .await
                .map_err(std::sync::Arc::new);
            Message::SendAddressResolved {
                revision: request.revision,
                result,
            }
        })
    }
}

impl Hash for AddressRequest {
    /// Различает проверки разных входов и переходов.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.sources.hash(state);
        self.owner.hash(state);
        self.generation.hash(state);
        self.stream.hash(state);
        self.revision.hash(state);
    }
}

impl Address {
    /// Объясняет, почему редактор пока не может отправить текст.
    pub(super) fn unavailable(&self) -> Option<&'static str> {
        match *self {
            Self::Unchecked => Some("Проверяем возможность отправки…"),
            Self::Ready(_) => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }
}

/// Показывает причину отказа отдельно от состояния загрузки истории.
pub(super) fn unavailable_reason(error: &DirectSendError) -> &'static str {
    match error {
        DirectSendError::AccessLost => "Чат или тема для отправки недоступны",
        DirectSendError::EncryptionUnavailable => "Отправка открытого текста недоступна",
        DirectSendError::TopicUnavailable => "Сервер не указал тему для отправки",
        DirectSendError::Auth(_) | DirectSendError::Source(_) => "Не удалось проверить отправку",
    }
}

/// Показывает временное сообщение и объясняет, где остановилась отправка.
pub(super) fn pending_bubble<M: 'static>(entry: &OutboxEntry) -> Element<'_, M> {
    let (label, delivery) = match entry.status {
        DeliveryStatus::Waiting => ("В очереди", MessageDelivery::Sending),
        DeliveryStatus::Sending => ("Отправляется…", MessageDelivery::Sending),
        DeliveryStatus::Unknown => ("Исход неизвестен. Ищем сообщение", MessageDelivery::Sending),
        DeliveryStatus::Blocked(SendBlock::AccessLost) => {
            ("Чат или тема недоступны", MessageDelivery::Failed)
        }
        DeliveryStatus::Blocked(SendBlock::EncryptionUnavailable) => (
            "Отправка открытого текста недоступна",
            MessageDelivery::Failed,
        ),
        DeliveryStatus::Rejected(_) => ("Не отправлено", MessageDelivery::Failed),
    };
    message_bubble(
        &entry.snapshot.markdown,
        None,
        label,
        MessageDirection::Outgoing(Some(delivery)),
    )
}

/// Снимок ввода в момент нажатия кнопки отправки.
pub(super) struct Submission {
    pub(super) version: u64,     // Версия текста, который пользователь отправил.
    pub(super) markdown: String, // Текст до следующего редактирования.
}

/// Редактор одного открытого разговора.
#[derive(Default)]
pub(super) struct Composer {
    content: Content,     // Текст, выделение и курсор Iced.
    version: u64,         // Увеличивается при редактировании, включая отмену.
    writing: Option<u64>, // Версия, которую сейчас сохраняем. `None` разрешает отправку.
    save_failed: bool,    // Последняя запись очереди завершилась ошибкой.
}

impl Composer {
    /// Применяет действие Iced и помечает версию, если пользователь изменил текст.
    pub(super) fn edit(&mut self, action: Action) {
        if action.is_edit() {
            self.version = self.version.wrapping_add(1);
            self.save_failed = false;
        }
        self.content.perform(action);
    }

    /// Фиксирует непустой текст допустимой длины и блокирует второй клик до записи.
    pub(super) fn begin(&mut self) -> Option<Submission> {
        if self.writing.is_some() {
            return None;
        }
        let markdown = self.content.text();
        if markdown.trim().is_empty() || over_limit(&markdown) {
            return None;
        }
        self.writing = Some(self.version);
        self.save_failed = false;
        Some(Submission {
            version: self.version,
            markdown,
        })
    }

    /// Принимает результат записи; новые правки сохраняются при любом исходе.
    pub(super) fn finish(&mut self, version: u64, saved: bool) {
        if self.writing != Some(version) {
            return;
        }
        self.writing = None;
        if self.version == version {
            self.save_failed = !saved;
            if saved {
                self.content = Content::new();
            }
        }
    }

    /// Рисует штатный редактор и пояснение, если текст нельзя отправить.
    pub(super) fn view<'a, M: Clone + 'a>(
        &'a self,
        unavailable: Option<&'a str>,
        on_edit: impl Fn(Action) -> M + 'a,
        on_send: M,
    ) -> Element<'a, M> {
        use iced::widget::{column, text};

        let too_long = over_limit(&self.content.text());
        let editable = unavailable.is_none();
        let send = (editable && self.writing.is_none() && !too_long).then_some(on_send);
        let mut field = column![message_composer(
            &self.content,
            MessageComposerOptions {
                send_id: Some(Id::new("conversation-send")),
                ..MessageComposerOptions::default()
            },
            editable.then_some(on_edit),
            send,
        )];
        let notice = unavailable.or(if too_long {
            Some("Сообщение длиннее 40 000 символов")
        } else if self.save_failed {
            Some("Не удалось сохранить сообщение. Попробуйте отправить снова.")
        } else {
            None
        });
        if let Some(notice) = notice {
            field = field.push(text(notice).size(12));
        }
        field.spacing(4).into()
    }
}

/// Считает символы Unicode до первого превышения лимита.
fn over_limit(text: &str) -> bool {
    text.chars().nth(MAX_MESSAGE_CHARS).is_some()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use iced::widget::text_editor::Edit;

    use super::*;

    /// Проверяет границу Unicode, пробельный текст и повторное нажатие до записи.
    #[test]
    fn validates_text_and_serializes_a_submission() {
        let mut editor = Composer {
            content: Content::with_text(" \n\t"),
            ..Composer::default()
        };
        assert!(editor.begin().is_none());
        let maximum = "🦀".repeat(MAX_MESSAGE_CHARS);
        editor.content = Content::with_text(&format!("{maximum}я"));
        assert!(editor.begin().is_none());
        editor.content = Content::with_text(&maximum);
        let first = editor.begin().unwrap();
        assert_eq!(first.markdown, maximum);
        assert!(editor.begin().is_none());
        editor.finish(first.version, true);
        assert!(editor.content.text().is_empty());
    }

    /// Оставляет текст после ошибки и не очищает изменённый черновик после успеха.
    #[test]
    fn retains_failed_or_edited_text() {
        let mut editor = Composer {
            content: Content::with_text("первый"),
            ..Composer::default()
        };
        let first = editor.begin().unwrap();
        editor.finish(first.version, false);
        assert_eq!(editor.content.text(), "первый");
        let retry = editor.begin().unwrap();
        editor.edit(Action::Edit(Edit::Paste(Arc::new(String::from("новый")))));
        editor.finish(retry.version, true);
        assert!(editor.content.text().contains("новый"));
        assert!(editor.begin().is_some());
    }
}
