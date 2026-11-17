//! Экран открытого разговора. Здесь находятся запросы, переходы истории и отображение состояний.

#[path = "history/source.rs"]
mod source;
#[path = "history/viewport.rs"]
mod viewport;

use futures::channel::oneshot;
use gpui::{App, Context, Entity, IntoElement, Render, Subscription, Window, div, prelude::*};
use uuid::Uuid;
use workspace_core::{
    storage::{HistoryDirection, HistoryEdge, HistoryScope, HistoryWindow, OutboxEntry},
    sync::{HistoryRevocation, SyncChange, SyncUpdate},
};

use self::{
    source::{Failure, Load, LoadResult, Snapshot, Source, runtime_source},
    viewport::{Position, Viewport},
};
use super::{
    composer::{Composer, ComposerEvent, SendSource, Submission},
    contracts::{ChatEntry, ConversationAddress},
    runtime::RuntimeClient,
    ui::{Button, Palette, TextRole, ThemeMode, label},
};

/// Один запрос экрана и его идентификатор в журнале.
#[derive(Clone, Copy)]
struct Flight {
    id: Uuid,   // Общий op_id запроса, полученной страницы и замера позиции.
    load: Load, // Действие и направление запроса.
}

/// Видимое состояние разговора.
enum State {
    Opening,              // Ожидаем проверку доступа и первое окно.
    Ready(HistoryWindow), // Подтверждённое окно с сообщениями и краями.
    Failed(Failure),      // Причина, которую окно показывает пользователю.
}

/// Связывает редактор с экраном и с рабочими операциями отправки.
struct Sending {
    view: Entity<Composer>, // Ввод текущего разговора.
    source: SendSource,     // Проверка адреса и запись на исполнителе.
    _intents: Subscription, // Передаёт действия редактора только этому экрану.
}

/// История, адрес открытия и очередь сообщений одного экрана.
pub struct Conversation {
    address: ConversationAddress, // Владелец, область, поколение и ревизия от оболочки.
    entry: ChatEntry,             // Текущая точка открытия, включая переход к последним.
    source: Source,               // Переносит запросы на рабочий поток.
    state: State,                 // Текущее подтверждённое окно или сообщение о проблеме.
    active: Option<Flight>,       // Один запрос истории, пока не применён его последний ответ.
    read_pending: bool, // Во время запроса пришло событие сохранённой истории или очереди.
    action_error: Option<Flight>, // Запрос, который можно повторить, пока открыта история.
    outbox: Vec<OutboxEntry>, // Последний снимок очереди в области разговора.
    viewport: Viewport, // Виртуальный список и его положение.
    sending: Option<Sending>, // Рабочий редактор; изолированные тесты истории могут его не подключать.
}

impl Conversation {
    /// Создаёт экран и сразу запрашивает историю в адресе, переданном оболочкой.
    pub fn open(
        address: ConversationAddress,
        runtime: RuntimeClient,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::with_source(address, runtime_source(runtime.clone()), cx);
        if view.address.route.scope.topic_id.is_none() {
            view.attach_composer(SendSource::runtime(runtime), cx);
        }
        view
    }

    /// Создаёт экран с заданным источником, в том числе с управляемыми ответами в GPUI-тестах.
    fn with_source(address: ConversationAddress, source: Source, cx: &mut Context<Self>) -> Self {
        let entry = address.route.entry;
        let mut conversation = Self {
            address,
            entry,
            source,
            state: State::Opening,
            active: None,
            read_pending: false,
            action_error: None,
            outbox: Vec::new(),
            viewport: Viewport::new(),
            sending: None,
        };
        let target = cx.entity().downgrade();
        // GPUI держит состояние незавершённо созданной Entity до выхода из cx.new.
        conversation
            .viewport
            .list()
            .set_scroll_handler(move |event, _, app| {
                let visible = event.visible_range.clone();
                let count = event.count;
                let target = target.clone();
                app.defer(move |app| {
                    let _ = target.update(app, |view, cx| view.scroll(visible, count, cx));
                });
            });
        conversation.start(Load::Open(entry), cx);
        conversation
    }

    /// Подключает редактор после создания экрана, пока первое окно истории ещё загружается.
    fn attach_composer(&mut self, source: SendSource, cx: &mut Context<Self>) {
        let view = cx.new(|cx| Composer::new(self.address.clone(), cx));
        let intents =
            cx.subscribe(
                &view,
                |conversation, sender, event: &ComposerEvent, cx| match event {
                    ComposerEvent::Send(submission) => {
                        conversation.save_submission(sender, submission, cx)
                    }
                    ComposerEvent::RetryDestination(address)
                        if conversation.has_access() && *address == conversation.address =>
                    {
                        if let Some(sending) = &conversation.sending {
                            sending.view.update(cx, |editor, cx| {
                                editor.retry_destination(&sending.source, cx)
                            });
                        }
                    }
                    ComposerEvent::RetryDestination(_) => {}
                },
            );
        self.sending = Some(Sending {
            view,
            source,
            _intents: intents,
        });
    }

    /// Перепроверяет адрес экрана и запускает одну запись очереди для принятого намерения.
    fn save_submission(
        &mut self,
        sender: Entity<Composer>,
        submission: &Submission,
        cx: &mut Context<Self>,
    ) {
        let Some(sending) = self
            .sending
            .as_ref()
            .filter(|sending| sending.view == sender)
        else {
            return;
        };
        if submission.address != self.address {
            return;
        }
        if !self.has_access() || !sender.read(cx).accepts(submission) {
            sender.update(cx, |editor, cx| {
                editor.finish(submission.attempt, false, cx)
            });
            return;
        }

        let save = sending.source.save.clone();
        let editor = sender.downgrade();
        let attempt = submission.attempt;
        let submission = submission.clone();
        cx.spawn(async move |conversation, cx| {
            // Рабочая запись остаётся привязана к старому адресу и при закрытии разговора.
            let reply = save(submission).await;
            let _ = conversation.update(cx, |view, cx| {
                let Some(public_id) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                let saved = public_id.is_some();
                if view
                    .sending
                    .as_ref()
                    .is_some_and(|sending| sending.view.downgrade() == editor)
                {
                    let _ = editor.update(cx, |editor, cx| editor.finish(attempt, saved, cx));
                }
                if saved {
                    view.refresh_outbox(cx);
                }
            });
        })
        .detach();
    }

    /// Снимает подтверждение отправки, когда текущее окно истории больше нельзя показывать.
    fn invalidate_send_destination(&self, cx: &mut Context<Self>) {
        if let Some(sending) = &self.sending {
            sending.view.update(cx, Composer::invalidate_destination);
        }
    }

    /// Возвращает адрес, по которому оболочка доставляет события истории.
    pub fn address(&self) -> &ConversationAddress {
        &self.address
    }

    /// Проверяет, что доступ к открытому окну подтверждён и история может быть показана.
    pub fn has_access(&self) -> bool {
        matches!(self.state, State::Ready(_))
    }

    /// Перечитывает историю и временные строки после сохранения отправки.
    pub fn refresh_outbox(&mut self, cx: &mut Context<Self>) {
        self.queue_read(cx);
    }

    /// Повторно открывает текущую область после пропуска событий Sync.
    pub fn reload(&mut self, address: &ConversationAddress, cx: &mut Context<Self>) {
        if self.address == *address {
            self.restart(cx);
        }
    }

    /// Применяет изменение только в том окне, которому оно адресовано.
    pub fn apply_sync(
        &mut self,
        address: &ConversationAddress,
        update: SyncUpdate,
        cx: &mut Context<Self>,
    ) {
        if self.address != *address
            || self.address.route.owner != update.context
            || self.address.generation != update.generation
        {
            return;
        }
        let scope = self.address.route.scope;
        match update.change {
            SyncChange::MessengerReset => self.restart(cx),
            SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(stream))
                if scope.stream_id == stream =>
            {
                self.fail(Failure::AccessLost, cx);
            }
            SyncChange::HistoryAccessRevoked(HistoryRevocation::Topic {
                stream_id,
                topic_id,
            }) if scope.stream_id == stream_id => match scope.topic_id {
                Some(current) if current == topic_id => self.fail(Failure::AccessLost, cx),
                None => self.restart(cx),
                _ => {}
            },
            SyncChange::HistoryMessageCreated { scope: changed, .. }
            | SyncChange::OutboxChanged { scope: changed }
                if includes(scope, changed) =>
            {
                self.queue_read(cx)
            }
            SyncChange::HistoryMessageChanged { message_id }
                if self.active.is_some()
                    || matches!(&self.state, State::Ready(window) if window.messages.iter().any(|message| message.id == message_id)) =>
            {
                self.queue_read(cx)
            }
            _ => {}
        }
    }

    /// Очищает недействительные данные и начинает проверку прежней цели заново.
    fn restart(&mut self, cx: &mut Context<Self>) {
        self.invalidate_send_destination(cx);
        self.state = State::Opening;
        self.active = None;
        self.read_pending = false;
        self.action_error = None;
        self.outbox.clear();
        self.viewport.clear();
        self.start(Load::Open(self.entry), cx);
    }

    /// Скрывает историю и делает любые запоздавшие ответы неприменимыми.
    fn fail(&mut self, error: Failure, cx: &mut Context<Self>) {
        self.invalidate_send_destination(cx);
        self.state = State::Failed(error);
        self.active = None;
        self.read_pending = false;
        self.action_error = None;
        self.outbox.clear();
        self.viewport.clear();
        cx.notify();
    }

    /// Объединяет события во время запроса и перечитывает уже подтверждённое окно.
    fn queue_read(&mut self, cx: &mut Context<Self>) {
        if self.active.is_some() {
            self.read_pending = true;
        } else if let State::Ready(window) = &self.state {
            self.start(Load::Read(window.id), cx);
        }
    }

    /// Запускает единственный запрос окна и применяет сохранённый хвост до окончания сети.
    fn start(&mut self, load: Load, cx: &mut Context<Self>) {
        if self.active.is_some() {
            return;
        }
        let flight = Flight {
            id: Uuid::new_v4(),
            load,
        };
        self.active = Some(flight);
        if self.action_error.is_some_and(|failed| failed.load == load) {
            self.action_error = None;
        }
        let address = self.address.clone();
        log::debug!(
            "gpui_history op_id={} event=request_started stream_id={} topic_id={:?} generation={} revision={} action={load:?}",
            flight.id,
            address.route.scope.stream_id,
            address.route.scope.topic_id,
            address.generation.0,
            address.revision.0
        );
        let source = self.source.clone();
        cx.spawn(async move |target, cx| {
            let (send_cache, receive_cache) = oneshot::channel();
            let response = source(address.clone(), load, flight.id, send_cache);
            let display_cache = async {
                if let Ok(snapshot) = receive_cache.await {
                    let _ = target.update(cx, |view, cx| {
                        if view.accepts(&address, flight) {
                            view.show(snapshot, flight, true, cx);
                        }
                    });
                }
            };
            let ((), response) = futures::join!(display_cache, response);
            let _ = target.update(cx, |view, cx| {
                if view.accepts(&address, flight)
                    && let Some(value) = response.for_current(Some(view.address()))
                {
                    view.finish(flight, value, cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Сверяет адрес экрана и проверяет, что сброс или отзыв доступа не отменил запрос.
    fn accepts(&self, address: &ConversationAddress, flight: Flight) -> bool {
        self.address == *address && self.active.is_some_and(|active| active.id == flight.id)
    }

    /// Применяет последний результат, сохраняет видимое окно при временной ошибке и запускает отложенное чтение.
    fn finish(&mut self, flight: Flight, result: LoadResult, cx: &mut Context<Self>) {
        self.active = None;
        if let Err(reason) = &result {
            log::debug!(
                "gpui_history op_id={} event=request_failed reason={reason:?}",
                flight.id
            );
        }
        match result {
            Ok(Some(snapshot)) if self.needs_fresh_tail(flight, &snapshot.window) => {
                // Подтверждённая запись может выйти за границу текущего окна. До свежей
                // страницы сохраняем видимую очередь и не сдвигаем читаемую историю.
                log::debug!(
                    "gpui_history op_id={} event=tail_refresh_required",
                    flight.id
                );
                self.start(Load::Open(ChatEntry::Latest), cx);
                return;
            }
            Ok(Some(snapshot)) => self.show(snapshot, flight, false, cx),
            Ok(None) if matches!(flight.load, Load::Read(_) | Load::Extend(_, _)) => {
                self.restart(cx);
                return;
            }
            Err(Failure::AccessLost) => self.fail(Failure::AccessLost, cx),
            Err(Failure::MissingMessage) => self.fail(Failure::MissingMessage, cx),
            Err(error) if self.has_access() => {
                if matches!(flight.load, Load::Extend(_, _)) && error == Failure::Changed {
                    // У старого края больше нет действующего маркера. Повтор проверит новое окно.
                    self.action_error = Some(Flight {
                        load: Load::Open(self.entry),
                        ..flight
                    });
                } else {
                    self.action_error = Some(flight);
                }
            }
            Err(error) => self.fail(error, cx),
            Ok(None) => self.fail(Failure::Changed, cx),
        }
        if self.active.is_none() && std::mem::take(&mut self.read_pending) {
            self.queue_read(cx);
        }
        cx.notify();
    }

    /// Обновляет потерявший подтверждение хвост, только когда пользователь остался у него.
    fn needs_fresh_tail(&self, flight: Flight, window: &HistoryWindow) -> bool {
        matches!(flight.load, Load::Read(_))
            && self.entry == ChatEntry::Latest
            && window.newer == HistoryEdge::Unchecked
            && matches!(&self.state, State::Ready(previous) if previous.newer == HistoryEdge::End)
            && self.viewport.at_tail()
    }

    /// Устанавливает снимок и восстанавливает позицию чтения, когда пользователь ушёл от хвоста.
    fn show(&mut self, snapshot: Snapshot, flight: Flight, cached: bool, cx: &mut Context<Self>) {
        let Snapshot { window, outbox } = snapshot;
        if window.scope != self.address.route.scope {
            self.fail(Failure::Changed, cx);
            return;
        }
        if let ChatEntry::Around(id) = self.entry
            && !window.messages.iter().any(|row| row.id == id)
            && (matches!(flight.load, Load::Open(ChatEntry::Around(_)))
                || matches!(&self.state, State::Ready(previous) if previous.messages.iter().any(|row| row.id == id)))
        {
            self.fail(Failure::MissingMessage, cx);
            return;
        }
        let position = match &self.state {
            State::Ready(previous)
                if cached && previous.id != window.id && self.entry == ChatEntry::Latest =>
            {
                Position::Tail
            }
            State::Ready(previous)
                if previous.newer == HistoryEdge::End
                    && window.newer == HistoryEdge::End
                    && self.viewport.at_tail()
                    && !matches!(flight.load, Load::Extend(_, HistoryDirection::Older)) =>
            {
                Position::Tail
            }
            State::Ready(_) => Position::Reading,
            _ => match self.entry {
                ChatEntry::Latest => Position::Tail,
                ChatEntry::Around(id) => Position::Around(id),
            },
        };
        if let Some(outbox) = outbox {
            self.outbox = outbox;
        }
        log::debug!(
            "gpui_history op_id={} event=page_received window_id={} rows={} cached={cached}",
            flight.id,
            window.id,
            window.messages.len()
        );
        self.viewport.replace(
            &window,
            &self.outbox,
            self.address.route.owner.user_id.parse().ok(),
            position,
            flight.id,
        );
        self.state = State::Ready(window);
        if let Some(sending) = &self.sending {
            sending.view.update(cx, |editor, cx| {
                editor.check_destination(&sending.source, cx)
            });
        }
        cx.notify();
    }

    /// Загружает сторону текущего окна, если сервер дал маркер и запрос ещё не запущен.
    fn extend(&mut self, direction: HistoryDirection, cx: &mut Context<Self>) {
        let State::Ready(window) = &self.state else {
            return;
        };
        if self
            .action_error
            .is_some_and(|error| matches!(error.load, Load::Open(_)))
        {
            return;
        }
        let edge = match direction {
            HistoryDirection::Older => &window.older,
            HistoryDirection::Newer => &window.newer,
        };
        if matches!(edge, HistoryEdge::More(_)) {
            self.start(Load::Extend(window.id, direction), cx);
        }
    }

    /// Догружает край после жеста прокрутки. После ошибки ждёт явного повтора.
    fn scroll(&mut self, visible: std::ops::Range<usize>, count: usize, cx: &mut Context<Self>) {
        if self.active.is_some() || self.action_error.is_some() || count == 0 {
            return;
        }
        if visible.start == 0 && visible.end < count {
            self.extend(HistoryDirection::Older, cx);
        } else if visible.end >= count && visible.start > 0 {
            self.extend(HistoryDirection::Newer, cx);
        }
    }

    /// Переходит к последним сообщениям, запрашивая подтверждённый хвост при разрыве или переходе к UUID.
    fn latest(&mut self, cx: &mut Context<Self>) {
        if self.active.is_some() {
            return;
        }
        if self.entry == ChatEntry::Latest
            && let State::Ready(window) = &self.state
            && window.newer == HistoryEdge::End
        {
            let op_id = Uuid::new_v4();
            self.viewport.replace(
                window,
                &self.outbox,
                self.address.route.owner.user_id.parse().ok(),
                Position::Tail,
                op_id,
            );
            cx.notify();
        } else {
            self.entry = ChatEntry::Latest;
            self.restart(cx);
        }
    }

    /// Повторяет последнее неудачное действие или снова проверяет доступ к истории.
    fn retry(&mut self, cx: &mut Context<Self>) {
        if self.active.is_some() {
            return;
        }
        if let Some(failure) = self.action_error.take() {
            self.start(failure.load, cx);
        } else {
            self.restart(cx);
        }
    }
}

impl Render for Conversation {
    /// Показывает историю и действия, доступные для текущих краёв или ошибки.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(ThemeMode::from(window.appearance()));
        let ready = self.has_access();
        let busy = self.active.is_some();
        let toolbar = match &self.state {
            State::Ready(history) => {
                let mut older = Button::new("history-older", "Старые сообщения");
                let edge_invalid = self
                    .action_error
                    .is_some_and(|error| matches!(error.load, Load::Open(_)));
                older.disabled = busy || edge_invalid;
                let mut newer = Button::new("history-newer", "Новые сообщения");
                newer.disabled = busy || edge_invalid;
                let mut latest = Button::new("history-latest", "К последним");
                latest.disabled = busy;
                Some(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .p_2()
                        .when(matches!(history.older, HistoryEdge::More(_)), |toolbar| {
                            toolbar.child(
                                older
                                    .render(
                                        palette,
                                        None,
                                        action(cx, |view, cx| {
                                            view.extend(HistoryDirection::Older, cx)
                                        }),
                                    )
                                    .debug_selector(|| "history-older".into()),
                            )
                        })
                        .when(matches!(history.newer, HistoryEdge::More(_)), |toolbar| {
                            toolbar.child(
                                newer
                                    .render(
                                        palette,
                                        None,
                                        action(cx, |view, cx| {
                                            view.extend(HistoryDirection::Newer, cx)
                                        }),
                                    )
                                    .debug_selector(|| "history-newer".into()),
                            )
                        })
                        .child(
                            latest
                                .render(palette, None, action(cx, Conversation::latest))
                                .debug_selector(|| "history-latest".into()),
                        ),
                )
            }
            _ => None,
        };
        let problem = match self.state {
            State::Failed(Failure::AccessLost) => Some("Доступ к разговору отозван"),
            State::Failed(Failure::MissingMessage) => Some("Сообщение больше недоступно"),
            State::Failed(_) => Some("Не удалось загрузить историю"),
            State::Ready(_) if self.action_error.is_some() => Some("Не удалось обновить историю"),
            _ => None,
        };
        div().flex().flex_col().size_full().min_h_0().min_w_0().bg(palette.background)
            .children(toolbar)
            .when(matches!(self.state, State::Opening), |view| view.child(label(palette, TextRole::Body, "Загружаем историю…").p_3()))
            .when_some(problem, |view, problem| {
                let mut retry = Button::new("history-retry", "Повторить");
                retry.disabled = busy;
                view.child(div().flex().flex_wrap().items_center().gap_2().p_2()
                    .child(label(palette, TextRole::Body, problem).text_color(palette.danger).debug_selector(|| "history-error".into()))
                    .when(!matches!(self.state, State::Failed(Failure::AccessLost | Failure::MissingMessage)), |bar| {
                        bar.child(retry.render(palette, None, action(cx, Conversation::retry)).debug_selector(|| "history-retry".into()))
                    })
                    .when(matches!(self.state, State::Failed(Failure::MissingMessage)), |bar| {
                        bar.child(Button::new("history-latest", "К последним").render(palette, None,
                            action(cx, Conversation::latest)).debug_selector(|| "history-latest".into()))
                    }))
            })
            .when(matches!(&self.state, State::Ready(history) if history.messages.is_empty() && self.outbox.is_empty()), |view| {
                view.child(label(palette, TextRole::Body, "Сообщений пока нет").p_3())
            })
            .when(ready, |view| {
                view.child(self.viewport.render(palette))
                    .children(self.sending.as_ref().map(|sending| sending.view.clone()))
            })
    }
}

/// Проверяет, относится ли событие темы к показанной области истории.
fn includes(current: HistoryScope, changed: HistoryScope) -> bool {
    current.stream_id == changed.stream_id
        && (current.topic_id.is_none() || current.topic_id == changed.topic_id)
}

/// Доставляет нажатие кнопки экрану, если оболочка всё ещё его показывает.
fn action(
    cx: &Context<Conversation>,
    handle: fn(&mut Conversation, &mut Context<Conversation>),
) -> impl Fn(&mut Window, &mut App) + 'static {
    let target = cx.entity().downgrade();
    move |_, app| {
        let _ = target.update(app, handle);
    }
}

#[cfg(all(test, feature = "gpui-test"))]
#[path = "history/tests.rs"]
mod tests;

#[cfg(all(test, feature = "gpui-test"))]
#[path = "composer/tests.rs"]
mod send_tests;
