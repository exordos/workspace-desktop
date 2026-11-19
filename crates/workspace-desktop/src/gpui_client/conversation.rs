//! Экран открытого разговора. Здесь находятся запросы, переходы истории и отображение состояний.

#[path = "history/attachments.rs"]
mod attachments;
mod details;
#[path = "history/fixtures.rs"]
mod history_fixtures;
#[path = "history/lab.rs"]
pub mod history_lab;
#[path = "history/source.rs"]
mod source;
#[path = "history/viewport.rs"]
mod viewport;

use std::collections::HashMap;

use futures::channel::oneshot;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, Pixels, Render,
    ScrollWheelEvent, Subscription, Window, canvas, div, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{
    bootstrap::ChatList,
    storage::{HistoryDirection, HistoryEdge, HistoryScope, HistoryWindow, OutboxEntry},
    sync::{HistoryRevocation, SyncChange, SyncUpdate},
};

use self::{
    attachments::Attachments,
    details::Panel,
    source::{Failure, Load, LoadResult, Snapshot, Source, runtime_source},
    viewport::{Position, Viewport},
};
use super::{
    avatars::AvatarImages,
    composer::{Composer, ComposerEvent, SendSource, Submission},
    contracts::{AccountSession, ChatEntry, ConversationAddress},
    right_panel::RightPanel,
    runtime::RuntimeClient,
    ui::{
        Avatar, Button, ButtonAppearance, Icon, PANEL_RADIUS, Palette, SURFACE_RADIUS, TextRole,
        label,
    },
};

const HISTORY_PAGE_AHEAD: Pixels = px(1200.);
const HISTORY_PAGING_HEIGHT: Pixels = px(32.);

/// Точка чтения, которую рабочее окно может восстановить при повторном открытии чата.
#[derive(Clone, Copy)]
pub(super) struct ReadingAnchor {
    pub id: Uuid,  // UUID подтверждённого сообщения.
    pub y: Pixels, // Верх пузыря относительно области прокрутки, в логических px.
}

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

/// Подписи разговора из каталога того же входа.
#[derive(Default, PartialEq, Eq)]
struct Heading {
    chat: Option<String>,         // Имя собеседника или канала.
    topic: Option<String>,        // Название темы, если открыт её адрес.
    direct: bool,                 // В заголовке лички оставляем место под аватар.
    direct_peer_id: Option<Uuid>, // UUID собеседника из каталога этого входа, если он известен.
}

/// Переход из правой панели, который должна подтвердить оболочка.
pub(super) struct OpenDetailsTopic {
    pub address: ConversationAddress, // Полный адрес панели, откуда пришло действие.
    pub topic_id: Uuid,               // UUID выбранной темы в том же канале.
}

impl EventEmitter<OpenDetailsTopic> for Conversation {}

/// История, адрес открытия и очередь сообщений одного экрана.
pub struct Conversation {
    address: ConversationAddress, // Владелец, область, поколение и ревизия от оболочки.
    entry: ChatEntry,             // Текущая точка открытия, включая переход к последним.
    restore: Option<ReadingAnchor>, // Сохранённая опора до первого успешного снимка.
    source: Source,               // Переносит запросы на рабочий поток.
    state: State,                 // Текущее подтверждённое окно или сообщение о проблеме.
    active: Option<Flight>,       // Один запрос истории, пока не применён его последний ответ.
    read_pending: bool, // Во время запроса пришло событие сохранённой истории или очереди.
    action_error: Option<Flight>, // Запрос, который можно повторить, пока открыта история.
    outbox: Vec<OutboxEntry>, // Последний снимок очереди в области разговора.
    viewport: Viewport, // Виртуальный список и его положение.
    scroll_direction: Option<HistoryDirection>, // Последний жест, до него проверяем заполнение окна.
    attachments: Attachments, // Карточки и одиночный просмотр вложений этого разговора.
    screen_focus: FocusHandle, // Запасной фокус при исчезновении истории и панели.
    history_focus: FocusHandle, // Точка клавиатурной прокрутки в открытом разговоре.
    sending: Option<Sending>, // Рабочий редактор; изолированные тесты истории могут его не подключать.
    heading: Heading,         // Отображаемые названия из текущего каталога.
    avatar_runtime: Option<RuntimeClient>, // Рабочий поток аватаров. Отсутствует в тестах истории без Core.
    avatars_visible: bool,                 // Разговор показан окном и может запрашивать фото.
    heading_avatars: AvatarImages,         // Подписка на фото собеседника, указанного в шапке.
    history_avatars: AvatarImages,         // Подписки на фото авторов в видимой части истории.
    history_avatar_users: Vec<Uuid>, // UUID из последнего размещения истории, в порядке сортировки.
    details: Option<Panel>,          // Панель штатного разговора с доступом к рабочему потоку.
}

impl Conversation {
    /// Регистрирует клавиши просмотра фото для рабочего окна.
    pub(super) fn bind_attachment_keys(app: &mut App) {
        Attachments::bind_keys(app);
    }

    /// Создаёт экран и сразу запрашивает историю в адресе, переданном оболочкой.
    pub(super) fn open(
        address: ConversationAddress,
        runtime: RuntimeClient,
        right_panel: Entity<RightPanel>,
        restore: Option<ReadingAnchor>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::with_source(address, runtime_source(runtime.clone()), restore, cx);
        view.attach_composer(SendSource::runtime(runtime.clone()), cx);
        view.attachments.runtime = Some(runtime.clone());
        view.avatar_runtime = Some(runtime.clone());
        view.details = Some(Panel::new(right_panel, view.address.clone(), runtime, cx));
        view
    }

    /// Создаёт экран с заданным источником, в том числе с управляемыми ответами в GPUI-тестах.
    fn with_source(
        address: ConversationAddress,
        source: Source,
        restore: Option<ReadingAnchor>,
        cx: &mut Context<Self>,
    ) -> Self {
        let entry = address.route.entry;
        let mut conversation = Self {
            address,
            entry,
            restore: restore.filter(|_| entry == ChatEntry::Latest),
            source,
            state: State::Opening,
            active: None,
            read_pending: false,
            action_error: None,
            outbox: Vec::new(),
            viewport: Viewport::with_page_ahead(HISTORY_PAGE_AHEAD),
            scroll_direction: None,
            attachments: Attachments::new(cx),
            screen_focus: cx.focus_handle(),
            history_focus: cx.focus_handle().tab_stop(true),
            sending: None,
            heading: Heading::default(),
            avatar_runtime: None,
            avatars_visible: false,
            heading_avatars: AvatarImages::default(),
            history_avatars: AvatarImages::default(),
            history_avatar_users: Vec::new(),
            details: None,
        };
        conversation.start(Load::Open(conversation.opening_entry()), cx);
        conversation
    }

    /// Снимает точку чтения при уходе с экрана. Подтверждённый хвост открывается штатным Latest.
    pub(super) fn reading_position(&self) -> Option<ReadingAnchor> {
        match &self.state {
            State::Ready(window) if window.newer == HistoryEdge::End && self.viewport.at_tail() => {
                None
            }
            State::Ready(_) => self.viewport.saved_anchor(),
            State::Opening | State::Failed(Failure::Unavailable | Failure::Changed) => self.restore,
            State::Failed(_) => None,
        }
    }

    /// Использует обычную загрузку вокруг UUID для восстановления разрешённого окна.
    fn opening_entry(&self) -> ChatEntry {
        self.restore
            .map_or(self.entry, |anchor| ChatEntry::Around(anchor.id))
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

    /// Обновляет названия по адресу экрана, когда каталог того же входа меняется.
    pub(super) fn set_catalog(
        &mut self,
        session: Option<&AccountSession>,
        catalog: Option<&ChatList>,
        cx: &mut Context<Self>,
    ) {
        let scope = self.address.route.scope;
        let session = session.filter(|session| {
            session.owner == self.address.route.owner
                && session.generation == self.address.generation
        });
        let heading = session
            .and(catalog)
            .filter(|catalog| catalog.generation == self.address.generation)
            .and_then(|catalog| catalog.chats.as_deref())
            .and_then(|chats| chats.iter().find(|chat| chat.id == scope.stream_id))
            .map(|chat| Heading {
                chat: Some(chat.name.clone()),
                topic: chat
                    .topics
                    .iter()
                    .find(|topic| Some(topic.id) == scope.topic_id)
                    .map(|topic| topic.name.clone()),
                direct: chat.direct,
                direct_peer_id: chat
                    .metadata
                    .direct_peer_id
                    .filter(|id| chat.direct && !id.is_nil()),
            })
            .unwrap_or_default();
        if heading != self.heading {
            self.heading = heading;
            self.refresh_heading_avatar(cx);
            cx.notify();
        }
    }

    /// Включает фото, только пока разговор виден в рабочем разделе окна.
    pub(super) fn set_avatars_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.avatars_visible == visible {
            return;
        }
        self.avatars_visible = visible;
        self.refresh_heading_avatar(cx);
        if !visible {
            self.clear_history_avatars();
            self.attachments.invalidate(|_| true, cx);
            self.attachments.take_restore();
        }
        cx.notify();
    }

    /// Подписывает шапку на фото собеседника, выбранного из каталога текущего входа.
    fn refresh_heading_avatar(&mut self, cx: &mut Context<Self>) {
        if self.avatars_visible
            && let Some(runtime) = self.avatar_runtime.as_ref()
        {
            let session = self.avatar_session();
            self.heading_avatars
                .refresh(self.heading.direct_peer_id, runtime, &session, cx);
        } else {
            self.heading_avatars.clear();
        }
    }

    /// Возвращает тот вход, которому оболочка открыла этот разговор.
    fn avatar_session(&self) -> AccountSession {
        AccountSession {
            owner: self.address.route.owner.clone(),
            generation: self.address.generation,
        }
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
        match &update.change {
            SyncChange::HistoryAccessRevoked(_) => {
                // Файл может принадлежать другой области того же владельца.
                self.attachments.invalidate(|_| true, cx);
            }
            SyncChange::HistoryMessageChanged { message_id } => {
                self.attachments
                    .invalidate(|key| key.message == *message_id, cx);
            }
            SyncChange::MediaFileInvalidated { file_id } => {
                self.attachments.invalidate(|key| key.file == *file_id, cx);
            }
            _ => {}
        }
        let scope = self.address.route.scope;
        match update.change {
            SyncChange::MessengerReset => self.restart(cx),
            SyncChange::SidebarDetailsChanged { stream_id } if scope.stream_id == stream_id => {
                self.refresh_open_details(cx);
            }
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

    /// Перепроверяет окно, удерживая текущую точку чтения.
    fn restart(&mut self, cx: &mut Context<Self>) {
        self.restore = self.reading_position();
        self.reopen(cx);
    }

    /// Скрывает прежние строки и начинает загрузку выбранного окна.
    fn reopen(&mut self, cx: &mut Context<Self>) {
        if let Some(details) = &mut self.details {
            details.clear(&self.address, None, cx);
        }
        self.invalidate_send_destination(cx);
        self.state = State::Opening;
        self.active = None;
        self.read_pending = false;
        self.action_error = None;
        self.scroll_direction = None;
        self.outbox.clear();
        self.viewport.clear();
        self.attachments.reset(cx);
        self.clear_history_avatars();
        self.start(Load::Open(self.opening_entry()), cx);
    }

    /// Скрывает историю и делает любые запоздавшие ответы неприменимыми.
    fn fail(&mut self, error: Failure, cx: &mut Context<Self>) {
        if let Some(details) = &mut self.details {
            let fallback = matches!(error, Failure::AccessLost).then_some(&self.screen_focus);
            details.clear(&self.address, fallback, cx);
        }
        self.invalidate_send_destination(cx);
        self.state = State::Failed(error);
        if matches!(error, Failure::AccessLost | Failure::MissingMessage) {
            self.restore = None;
        }
        self.active = None;
        self.read_pending = false;
        self.action_error = None;
        self.outbox.clear();
        self.viewport.clear();
        self.attachments.reset(cx);
        self.clear_history_avatars();
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
            Err(Failure::MissingMessage) if self.abandon_restore(flight, cx) => return,
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

    /// При исчезновении сохранённой опоры открывает обычный хвост того же разговора.
    fn abandon_restore(&mut self, flight: Flight, cx: &mut Context<Self>) -> bool {
        if self.restore.is_some() && flight.load == Load::Open(self.opening_entry()) {
            self.restore = None;
            self.entry = ChatEntry::Latest;
            self.reopen(cx);
            return true;
        }
        false
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
        let Snapshot {
            window,
            outbox,
            media_access,
        } = snapshot;
        if window.scope != self.address.route.scope {
            self.fail(Failure::Changed, cx);
            return;
        }
        if let Load::Open(ChatEntry::Around(id)) = flight.load
            && !window.messages.iter().any(|row| row.id == id)
        {
            if !self.abandon_restore(flight, cx) {
                self.fail(Failure::MissingMessage, cx);
            }
            return;
        }
        let stalled = match (&self.state, flight.load) {
            (State::Ready(previous), Load::Extend(_, HistoryDirection::Older)) => {
                matches!(window.older, HistoryEdge::More(_))
                    && previous.messages.first().map(|row| row.id)
                        == window.messages.first().map(|row| row.id)
            }
            (State::Ready(previous), Load::Extend(_, HistoryDirection::Newer)) => {
                matches!(window.newer, HistoryEdge::More(_))
                    && previous.messages.last().map(|row| row.id)
                        == window.messages.last().map(|row| row.id)
            }
            _ => false,
        };
        let position = match &self.state {
            State::Ready(previous)
                if cached && previous.id != window.id && self.entry == ChatEntry::Latest =>
            {
                Position::Tail
            }
            State::Ready(previous)
                if previous.newer == HistoryEdge::End
                    && window.newer == HistoryEdge::End
                    && self.viewport.at_tail() =>
            {
                Position::Tail
            }
            State::Ready(_) => Position::Reading,
            _ if self.restore.is_some() => Position::Restore(self.restore.take().unwrap()),
            _ => match self.entry {
                ChatEntry::Latest => Position::Tail,
                ChatEntry::Around(id) => Position::Around(id),
            },
        };
        if let Some(outbox) = outbox {
            self.outbox = outbox;
        }
        if matches!(flight.load, Load::Open(_)) {
            self.attachments.access = media_access;
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
        self.attachments
            .retain(|key| self.viewport.has_attachment(key), cx);
        if window.messages.is_empty() {
            self.clear_history_avatars();
        }
        self.state = State::Ready(window);
        if stalled {
            self.action_error = Some(flight);
        }
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

    /// Догружает короткое окно или запрашивает страницу до края по геометрии готового кадра.
    fn prefetch(&mut self, cx: &mut Context<Self>) {
        if self.active.is_some() || self.action_error.is_some() {
            return;
        }
        if let State::Ready(window) = &self.state
            && let Some(need) = self.viewport.page_need(
                &window.older,
                &window.newer,
                self.scroll_direction,
                HISTORY_PAGE_AHEAD,
            )
        {
            self.extend(need.direction, cx);
            if let Some(flight) = self.active {
                log::debug!(
                    "gpui_history op_id={} event=page_ahead direction={:?} distance_px={}",
                    flight.id,
                    need.direction,
                    f32::from(need.distance)
                );
            }
        }
    }

    /// Запоминает направление жеста. Сами строки прокручивает GPUI.
    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(20.)).y;
        if delta != px(0.) {
            self.scroll_direction = Some(if delta > px(0.) {
                HistoryDirection::Older
            } else {
                HistoryDirection::Newer
            });
            cx.notify();
        }
    }

    /// Переходит к последним сообщениям, запрашивая подтверждённый хвост при разрыве или переходе к UUID.
    fn latest(&mut self, cx: &mut Context<Self>) {
        if self.active.is_some() && !self.has_access() {
            return;
        }
        self.restore = None;
        self.scroll_direction = None;
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
            self.reopen(cx);
        }
    }

    /// Повторяет последнее неудачное действие или снова проверяет доступ к истории.
    fn retry(&mut self, cx: &mut Context<Self>) {
        if self.active.is_some() {
            return;
        }
        if let Some(failure) = self.action_error.take() {
            if matches!(failure.load, Load::Open(_)) {
                self.restart(cx);
            } else {
                self.start(failure.load, cx);
            }
        } else {
            self.restart(cx);
        }
    }

    /// Прокручивает историю, когда фокус в списке, и догружает край следующим нажатием.
    fn scroll_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        let list = self.viewport.list();
        let height = list.viewport_bounds().size.height;
        let step = (height - px(40.)).max(px(40.));
        let offset = list.logical_scroll_top();
        let at_start = offset.item_ix == 0 && offset.offset_in_item <= px(0.);
        let at_tail = self.viewport.at_tail();
        let key = event.keystroke.key.as_str();
        let distance = match key {
            "up" => -px(40.),
            "down" => px(40.),
            "pageup" | "home" => -step,
            "pagedown" => step,
            "end" => px(0.),
            _ => return,
        };
        cx.stop_propagation();
        if key == "end" {
            self.latest(cx);
            return;
        }
        self.scroll_direction = Some(if distance < px(0.) {
            HistoryDirection::Older
        } else {
            HistoryDirection::Newer
        });
        if (distance < px(0.) && at_start) || (distance > px(0.) && at_tail) {
            if self.active.is_none() && self.action_error.is_none() {
                let direction = if distance < px(0.) {
                    HistoryDirection::Older
                } else {
                    HistoryDirection::Newer
                };
                self.extend(direction, cx);
            }
            return;
        }
        if key == "home" {
            list.scroll_to(gpui::ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
        } else {
            // В режиме хвоста GPUI хранит смещение за последней строкой.
            // Первый шаг вверх отсчитываем от верхнего края видимой области.
            let distance = if offset.item_ix == list.item_count() {
                distance - height
            } else {
                distance
            };
            list.scroll_by(distance);
        }
        cx.notify();
    }

    /// Отмечает клавиатурный фокус внутри истории, не меняя размеры списка.
    fn history_frame(
        &self,
        palette: Palette,
        viewport: impl IntoElement,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let target = cx.entity().downgrade();
        div()
            .id("conversation-history-focus")
            .debug_selector(|| "conversation-history-focus".into())
            .track_focus(&self.history_focus)
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .border_1()
            .border_color(gpui::transparent_black())
            .when(palette.keyboard_focus_visible(), |frame| {
                frame.focus(|style| style.border_color(palette.focus))
            })
            .on_key_down(cx.listener(Self::scroll_key))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .child(viewport)
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, (), _, app| {
                        let target = target.clone();
                        app.defer(move |app| {
                            let _ = target.update(app, Self::prefetch);
                        });
                    },
                )
                .absolute()
                .inset_0(),
            )
            .when(
                self.avatars_visible && self.avatar_runtime.is_some(),
                |frame| {
                    frame.child(
                        self.viewport.track_visible_avatars(
                            cx.entity().downgrade(),
                            &self.history_avatar_users,
                        ),
                    )
                },
            )
    }

    /// Подключает только авторов, чьи аватары нужны в видимой части подтверждённой истории.
    fn update_history_avatars(&mut self, users: Vec<Uuid>, cx: &mut Context<Self>) {
        let Some(runtime) = self.avatar_runtime.as_ref() else {
            return;
        };
        if !self.avatars_visible || self.history_avatar_users == users || !self.has_access() {
            return;
        }
        let session = self.avatar_session();
        self.history_avatars
            .refresh(users.iter().copied(), runtime, &session, cx);
        self.history_avatar_users = users;
        cx.notify();
    }

    /// Освобождает подписки, когда история скрывается или теряет доступ.
    fn clear_history_avatars(&mut self) {
        self.history_avatars.clear();
        self.history_avatar_users.clear();
    }
}

impl Conversation {
    /// Рисует название чата или тему и название её канала из каталога.
    fn render_heading(&self, palette: Palette, cx: &Context<Self>) -> gpui::Div {
        let chat = self.heading.chat.as_deref();
        let (title, detail) = if self.address.route.scope.topic_id.is_some() {
            (self.heading.topic.as_deref().unwrap_or("Тема"), chat)
        } else {
            (chat.unwrap_or("Разговор"), None)
        };
        div()
            .debug_selector(|| "conversation-heading".into())
            .h(px(54.))
            .flex_none()
            .flex()
            .min_w_0()
            .items_center()
            .gap_3()
            .px_4()
            .rounded(PANEL_RADIUS)
            .bg(palette.surface)
            .when(self.heading.direct, |heading| {
                let initial = title
                    .chars()
                    .next()
                    .map(|first| first.to_uppercase().collect::<String>())
                    .unwrap_or_default();
                let mut avatar = Avatar::from_initials(initial);
                avatar.image = self
                    .heading
                    .direct_peer_id
                    .and_then(|id| self.heading_avatars.image(id, cx));
                heading.child(
                    avatar
                        .render(palette)
                        .debug_selector(|| "conversation-heading-avatar".into()),
                )
            })
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .child(
                        label(palette, TextRole::Label, title.to_owned())
                            .w_full()
                            .truncate()
                            .debug_selector(|| "conversation-title".into()),
                    )
                    .children(detail.map(|detail| {
                        label(palette, TextRole::Caption, detail.to_owned())
                            .w_full()
                            .truncate()
                            .debug_selector(|| "conversation-channel".into())
                    })),
            )
            .children(
                self.details
                    .as_ref()
                    .filter(|_| self.can_open_details())
                    .map(|panel| panel.trigger(palette, cx)),
            )
    }

    /// Рисует только те действия догрузки, для которых есть маркер на стороне сервера.
    fn render_paging(
        &self,
        history: &HistoryWindow,
        palette: Palette,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let disabled = self.active.is_some()
            || self
                .action_error
                .is_some_and(|error| matches!(error.load, Load::Open(_)));
        let mut paging = div()
            .h(HISTORY_PAGING_HEIGHT)
            .flex_none()
            .flex()
            .justify_center()
            .gap_2()
            .px_2()
            .overflow_hidden();
        for (edge, id, title, direction) in [
            (
                &history.older,
                "history-older",
                "Старые сообщения",
                HistoryDirection::Older,
            ),
            (
                &history.newer,
                "history-newer",
                "Новые сообщения",
                HistoryDirection::Newer,
            ),
        ] {
            if matches!(edge, HistoryEdge::More(_)) {
                let mut button = Button::new(id, title);
                button.disabled = disabled;
                paging = paging.child(
                    button
                        .render(
                            palette,
                            None,
                            action(cx, move |view, cx| view.extend(direction, cx)),
                        )
                        .debug_selector(move || id.into()),
                );
            }
        }
        paging
    }

    /// Рисует ошибку и только то действие, которое может исправить её в этом состоянии.
    fn render_problem(
        &self,
        palette: Palette,
        cause: Option<Failure>,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let retry: fn(&mut Self, &mut Context<Self>) = Self::retry;
        let (message, command) = match cause {
            Some(Failure::AccessLost) => ("Доступ к разговору отозван", None),
            Some(Failure::MissingMessage) => (
                "Сообщение больше недоступно",
                Some((
                    "history-latest",
                    "К последним",
                    Self::latest as fn(&mut Self, &mut Context<Self>),
                )),
            ),
            Some(_) => (
                "Не удалось загрузить сообщения",
                Some(("history-retry", "Повторить", retry)),
            ),
            None => (
                "Не удалось обновить историю",
                Some(("history-retry", "Повторить", retry)),
            ),
        };
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .p_2()
            .child(
                label(palette, TextRole::Body, message).debug_selector(|| "history-error".into()),
            )
            .children(command.map(|(id, title, handler)| {
                let mut button = Button::new(id, title);
                button.appearance = if cause.is_some() {
                    ButtonAppearance::Primary
                } else {
                    ButtonAppearance::Neutral
                };
                button.disabled = self.active.is_some();
                button
                    .render(palette, None, action(cx, handler))
                    .debug_selector(move || id.into())
            }))
    }

    /// Возвращает кнопку перехода к хвосту, когда читатель находится выше последних сообщений.
    fn render_latest(&self, palette: Palette, cx: &Context<Self>) -> gpui::Div {
        let mut latest = Button::new("history-latest", "К последним");
        latest.icon = Some(Icon::ChevronDown);
        latest.disabled = self.active.is_some();
        div().absolute().bottom_3().right_3().child(
            latest
                .render(palette, None, action(cx, Self::latest))
                .debug_selector(|| "history-latest".into()),
        )
    }
}

impl Conversation {
    /// Показывает шапку и историю, отделяя первые состояния от ошибок догрузки.
    fn render_chat(&mut self, palette: Palette, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let root = div()
            .id("conversation-chat")
            .debug_selector(|| "conversation-chat".into())
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .min_h_0()
            .min_w_0()
            .bg(palette.background)
            .child(self.render_heading(palette, cx));
        let centered = div()
            .flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .m_3()
            .rounded(SURFACE_RADIUS)
            .bg(palette.field)
            .border_1()
            .border_color(palette.border);
        match &self.state {
            State::Opening => root.child(
                centered.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .debug_selector(|| "history-loading".into())
                        .child(Icon::Pending.element(px(24.), palette.accent))
                        .child(label(palette, TextRole::Body, "Загружаем историю…")),
                ),
            ),
            State::Failed(cause) => {
                root.child(centered.child(self.render_problem(palette, Some(*cause), cx)))
            }
            State::Ready(history) => {
                let latest = self.entry != ChatEntry::Latest
                    || history.newer != HistoryEdge::End
                    || !self.viewport.at_tail();
                let empty = history.messages.is_empty()
                    && (history.newer != HistoryEdge::End || self.outbox.is_empty());
                let paging = self.render_paging(history, palette, cx);
                let peer = self
                    .heading
                    .direct
                    .then(|| self.heading.chat.clone())
                    .flatten();
                let problem = self
                    .action_error
                    .map(|_| self.render_problem(palette, None, cx));
                let viewport = if empty {
                    label(palette, TextRole::Caption, "Сообщений пока нет")
                        .m_auto()
                        .debug_selector(|| "history-empty".into())
                        .into_any_element()
                } else {
                    let images: HashMap<_, _> = self
                        .history_avatar_users
                        .iter()
                        .copied()
                        .filter_map(|id| Some((id, self.history_avatars.image(id, cx)?)))
                        .collect();
                    let cards = self.attachments.cards(cx);
                    self.viewport
                        .render(palette, peer, images, Some(cards))
                        .into_any_element()
                };
                root.child(
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .child(paging)
                        .child(self.history_frame(palette, viewport, cx))
                        .children(problem.map(|problem| {
                            div()
                                .absolute()
                                .top_10()
                                .left_3()
                                .right_3()
                                .flex()
                                .justify_center()
                                .child(
                                    problem
                                        .rounded(PANEL_RADIUS)
                                        .bg(palette.surface)
                                        .border_1()
                                        .border_color(palette.border),
                                )
                        }))
                        .when(latest, |history| {
                            history.child(self.render_latest(palette, cx))
                        }),
                )
                .children(self.sending.as_ref().map(|sending| sending.view.clone()))
            }
        }
    }
}

impl Render for Conversation {
    /// Рисует историю и редактор в рабочей области разговора.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        self.restore_details_for_verified_history(window, cx);
        if let Some((key, focus)) = self.attachments.take_restore() {
            if self.viewport.has_visible_attachment(key) {
                window.focus(&focus);
            } else if self.has_access() {
                window.focus(&self.history_focus);
            } else {
                window.focus(&self.screen_focus);
            }
        }
        let photo = self.attachments.overlay(window, palette, cx);
        div()
            .id("conversation-layout")
            .relative()
            .size_full()
            .flex()
            .min_h_0()
            .min_w_0()
            .track_focus(&self.screen_focus)
            .on_key_down(cx.listener(|view, _: &KeyDownEvent, _, cx| {
                if view.attachments.is_open() {
                    cx.stop_propagation();
                }
            }))
            .child(self.render_chat(palette, cx))
            .children(photo)
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
    handle: impl Fn(&mut Conversation, &mut Context<Conversation>) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let target = cx.entity().downgrade();
    move |_, app| {
        let _ = target.update(app, &handle);
    }
}

#[cfg(all(test, feature = "gpui-test"))]
#[path = "history/tests.rs"]
mod tests;

#[cfg(all(test, feature = "gpui-test"))]
#[path = "composer/tests.rs"]
mod send_tests;
