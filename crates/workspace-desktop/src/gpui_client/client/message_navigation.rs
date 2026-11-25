//! Готовит переход к UUID, пока исходная лента открыта, и показывает результат текущего клика.

use std::{cell::Cell, rc::Rc};

use gpui::{Context, Div, Entity, Task, prelude::*};
use workspace_core::{
    storage::HistoryScope,
    sync::{HistoryRevocation, SyncChange, SyncUpdate},
};

use super::{Client, ConversationOpening};
use crate::gpui_client::{
    contracts::{ChatEntry, ChatRoute, ConversationAddress, NavigationRevision},
    conversation::{
        Conversation, MessageHistoryError, OpenMessageTarget, PreparedMessageHistory,
        prepare_message_history, resolve_message_target,
    },
    preferences::Preferences,
    ui::{Button, ConversationNotice, Palette, TopBarSection},
};

/// Намерение перейти, привязанное к экрану, входу и версии навигации.
#[derive(Clone)]
struct Request {
    target: OpenMessageTarget, // UUID и исходный элемент, по которому нажали.
    revision: NavigationRevision, // Версия последнего действия в оболочке.
}

/// Учитывает изменения истории, пришедшие в окно во время фоновой подготовки.
#[derive(Default)]
struct HistoryChanges {
    scope: Cell<Option<HistoryScope>>, // Точная область после проверки размещения оригинала.
    changed: Cell<bool>, // Снимок цели может быть устаревшим, если было подходящее событие.
}

/// Отвечает за одну подготовку или за сообщение об ошибке того же клика.
pub(super) struct MessageNavigation {
    request: Request,       // Источник перехода остаётся тем же и для повторной попытки.
    state: NavigationState, // Подготовка в фоне либо причина отказа.
}

/// Стадия операции перехода, которую хранит рабочее окно.
enum NavigationState {
    Preparing {
        changes: Rc<HistoryChanges>, // Изменения цели между запросом и показом.
        _task: Task<()>, // Живёт, пока последнее действие пользователя требует этого перехода.
    },
    Failed(MessageHistoryError), // Безопасная категория ошибки для текста и действия повтора.
}

impl Client {
    /// Принимает клик только от текущей ленты и запускает общее действие для UUID.
    pub(super) fn open_message_target(
        &mut self,
        sender: &Entity<Conversation>,
        target: &OpenMessageTarget,
        cx: &mut Context<Self>,
    ) {
        if self.conversation.as_ref() == Some(sender) {
            self.start_message_navigation(target.clone(), cx);
        }
    }

    /// Прокручивает готовую ленту сразу или начинает подготовку другой страницы.
    fn start_message_navigation(&mut self, target: OpenMessageTarget, cx: &mut Context<Self>) {
        let Some(origin) = self.message_navigation_origin(&target, cx) else {
            return;
        };
        let Some(revision) = self.shell.begin_message_navigation(&target.address) else {
            return;
        };
        self.message_navigation = None;
        let already_loaded = origin.read(cx).can_reveal_message(target.id);
        log::debug!(
            "gpui_shell op_id={} event=message_navigation_started already_loaded={already_loaded}",
            revision.0
        );
        if already_loaded {
            self.save_place_reading(cx);
            origin.update(cx, |view, cx| view.reveal_message(target.id, cx));
            self.record_message_place();
            log::debug!(
                "gpui_shell op_id={} event=message_navigation_finished outcome=shown",
                revision.0
            );
            cx.notify();
            return;
        }

        let request = Request { target, revision };
        let changes = Rc::new(HistoryChanges::default());
        let runtime = self.runtime.clone();
        let background = cx.background_executor().clone();
        let origin = origin.downgrade();
        let task = cx.spawn({
            let request = request.clone();
            let changes = changes.clone();
            async move |client, cx| {
                let result = async {
                    // При одной правке в пути повторно проверяем размещение, в том числе перенос темы.
                    for _ in 0..2 {
                        changes.changed.set(false);
                        changes.scope.set(None);
                        let source = &request.target.address;
                        let scope = resolve_message_target(
                            runtime.clone(),
                            source.clone(),
                            request.target.id,
                        )
                        .await;
                        let result = match scope {
                            Ok(scope) => {
                                changes.scope.set(Some(scope));
                                let address = ConversationAddress {
                                    route: ChatRoute {
                                        owner: source.route.owner.clone(),
                                        scope,
                                        entry: ChatEntry::Around(request.target.id),
                                    },
                                    generation: source.generation,
                                    revision: request.revision,
                                };
                                let existing = origin
                                    .update(cx, |view, _| view.message_history_encryption())
                                    .ok()
                                    .flatten();
                                prepare_message_history(
                                    runtime.clone(),
                                    address.clone(),
                                    background.clone(),
                                    existing,
                                )
                                .await
                                .map(|history| (address, history))
                            }
                            Err(error) => Err(error),
                        };
                        if !changes.changed.get() {
                            return result;
                        }
                    }
                    Err(MessageHistoryError::Changed)
                }
                .await;
                let _ = client.update(cx, |client, cx| {
                    client.finish_message_target(request.revision, result, cx);
                });
            }
        });
        self.message_navigation = Some(MessageNavigation {
            request,
            state: NavigationState::Preparing {
                changes,
                _task: task,
            },
        });
        cx.notify();
    }

    /// Возвращает исходный экран, когда ссылка всё ещё существует в доступной ленте.
    fn message_navigation_origin(
        &self,
        target: &OpenMessageTarget,
        cx: &Context<Self>,
    ) -> Option<Entity<Conversation>> {
        if self.section != TopBarSection::Chats || self.sign_in.is_some() || self.licenses.is_some()
        {
            return None;
        }
        let origin = self.conversation.as_ref()?;
        let view = origin.read(cx);
        (view.address() == &target.address && target.source.is_current(view))
            .then(|| origin.clone())
    }

    /// Проверяет, что новое действие или обновление исходной ссылки не отменили переход.
    fn is_message_request_current(&self, request: &Request, cx: &Context<Self>) -> bool {
        self.shell
            .is_message_navigation_current(&request.target.address, request.revision)
            && self
                .message_navigation_origin(&request.target, cx)
                .is_some()
    }

    /// Убирает подготовку или ошибку, принадлежавшую закрытому экрану либо прошлому действию.
    pub(super) fn prune_message_navigation(&mut self, cx: &Context<Self>) {
        if self
            .message_navigation
            .as_ref()
            .is_some_and(|navigation| !self.is_message_request_current(&navigation.request, cx))
        {
            self.message_navigation = None;
        }
    }

    /// Помечает результаты, которые нужно перечитать после изменения цели или сброса синхронизации.
    pub(super) fn note_message_navigation_change(&mut self, update: Option<&SyncUpdate>) {
        let Some(navigation) = &self.message_navigation else {
            return;
        };
        let request = &navigation.request.target;
        if update.is_some_and(|update| {
            update.context != request.address.route.owner
                || update.generation != request.address.generation
        }) {
            return;
        }
        let change = update.map(|update| &update.change);
        if matches!(change, Some(SyncChange::HistoryMessageChanged { message_id })
            if *message_id == request.source.row())
        {
            // Старая разметка ещё может ждать перечитывания строки после правки.
            self.message_navigation = None;
            return;
        }
        let NavigationState::Preparing { changes, .. } = &navigation.state else {
            return;
        };
        let changed = match change {
            None | Some(SyncChange::MessengerReset) => true,
            Some(
                SyncChange::HistoryMessageChanged { .. }
                | SyncChange::HistoryMessageReactionsChanged { .. },
            ) => true,
            Some(SyncChange::QuoteOriginChanged { message_id }) => *message_id == request.id,
            Some(
                SyncChange::HistoryMessageCreated { scope, .. }
                | SyncChange::OutboxChanged { scope },
            ) => changes.scope.get().is_none_or(|target| {
                target.stream_id == scope.stream_id
                    && (target.topic_id.is_none() || target.topic_id == scope.topic_id)
            }),
            Some(SyncChange::HistoryAccessRevoked(revoked)) => {
                changes.scope.get().is_none_or(|scope| match *revoked {
                    HistoryRevocation::Stream(id) => scope.stream_id == id,
                    HistoryRevocation::Topic {
                        stream_id,
                        topic_id,
                    } => {
                        scope.stream_id == stream_id
                            && scope.topic_id.is_none_or(|id| id == topic_id)
                    }
                })
            }
            _ => false,
        };
        if changed {
            changes.changed.set(true);
        }
    }

    /// Показывает подготовленное окно одним действием, сохраняя разрешённую ленту при отказе.
    fn finish_message_target(
        &mut self,
        revision: NavigationRevision,
        result: Result<(ConversationAddress, PreparedMessageHistory), MessageHistoryError>,
        cx: &mut Context<Self>,
    ) {
        if self
            .message_navigation
            .as_ref()
            .is_none_or(|navigation| navigation.request.revision != revision)
        {
            return;
        }
        let Some(MessageNavigation { request, .. }) = self.message_navigation.take() else {
            return;
        };
        if !self.is_message_request_current(&request, cx) {
            return;
        }
        let result = result.and_then(|(address, mut prepared)| {
            if !prepared.ready_for_show() {
                return Err(MessageHistoryError::Changed);
            }
            self.save_place_reading(cx);
            let source = &request.target.address;
            if source.route.scope == address.route.scope {
                let Some(conversation) = &self.conversation else {
                    return Err(MessageHistoryError::Changed);
                };
                if !conversation.update(cx, |view, cx| view.apply_prepared_message(prepared, cx)) {
                    return Err(MessageHistoryError::Changed);
                }
                self.record_message_place();
            } else {
                let effect = self
                    .shell
                    .finish_message_navigation(source, address)
                    .ok_or(MessageHistoryError::Changed)?;
                self.apply_opening(
                    Some(effect),
                    ConversationOpening::Prepared(Box::new(prepared)),
                    cx,
                );
            }
            Ok(())
        });
        if let Err(error) = result {
            log::debug!(
                "gpui_shell op_id={} event=message_navigation_finished outcome=failed reason={error:?}",
                revision.0
            );
            self.message_navigation = Some(MessageNavigation {
                request,
                state: NavigationState::Failed(error),
            });
        } else {
            log::debug!(
                "gpui_shell op_id={} event=message_navigation_finished outcome=shown",
                revision.0
            );
        }
        cx.notify();
    }

    /// Выполняет повтор только если исходная ссылка всё ещё относится к открытому разговору.
    fn retry_message_navigation(&mut self, revision: NavigationRevision, cx: &mut Context<Self>) {
        self.prune_message_navigation(cx);
        let Some(navigation) = &self.message_navigation else {
            cx.notify();
            return;
        };
        if navigation.request.revision != revision
            || !matches!(navigation.state, NavigationState::Failed(_))
        {
            return;
        }
        let target = navigation.request.target.clone();
        self.start_message_navigation(target, cx);
    }

    /// Рисует ошибку перехода рядом с сохранённой лентой и действие для временного сбоя.
    pub(super) fn render_message_navigation_problem(
        &self,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Option<Div> {
        let navigation = self.message_navigation.as_ref()?;
        let NavigationState::Failed(error) = &navigation.state else {
            return None;
        };
        let request = &navigation.request;
        let language = Preferences::read_for(cx, &request.target.address.route.owner).language;
        let revision = request.revision;
        let (key, retry) = match error {
            MessageHistoryError::AccessLost => {
                ("desktop.messenger.history.deliveryNoAccess", false)
            }
            MessageHistoryError::MissingMessage => (
                "desktop.conversation.history.thisMessageIsNoLongerAvailable",
                false,
            ),
            _ => ("desktop.conversation.history.couldNotOpenMessage", true),
        };
        let client = cx.entity().downgrade();
        let notice = ConversationNotice {
            text: language.tr(key).into(),
            action: retry
                .then(|| Button::new("message-navigation-retry", language.tr("common.retry"))),
        }
        .render(palette, move |_, app| {
            let _ = client.update(app, |view, cx| view.retry_message_navigation(revision, cx));
        })
        .debug_selector(|| "message-navigation-error".into());
        Some(notice)
    }
}
