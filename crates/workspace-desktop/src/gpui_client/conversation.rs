//! Экран открытого разговора. Здесь находятся запросы, переходы истории и отображение состояний.

#[path = "history/attachments.rs"]
mod attachments;
mod calls;
mod details;
pub(in crate::gpui_client) use details::{
    ContactOrigin, ContactPanel, ContactRequest, ContactTarget,
};
#[path = "history/encryption.rs"]
mod encryption;
#[path = "history/forward.rs"]
mod forward;
#[path = "history/fragments.rs"]
mod fragments;
mod gallery_source;
mod heading;
#[path = "history/fixtures.rs"]
mod history_fixtures;
#[path = "history/lab.rs"]
pub mod history_lab;
#[path = "history/inline_references.rs"]
mod inline_references;
#[path = "history/key_exchange.rs"]
mod key_exchange;
#[path = "history/loading.rs"]
mod loading;
#[path = "history/media_album.rs"]
mod media_album;
#[path = "history/message_delete.rs"]
mod message_delete;
#[path = "history/message_menu.rs"]
mod message_menu;
#[path = "history/message_navigation.rs"]
mod message_navigation;
pub(super) use message_navigation::{
    MessageNavigationSource, OpenMessageTarget, resolve_message_target,
};
#[path = "history/protected_body.rs"]
mod protected_body;
#[path = "history/protected_quotes.rs"]
mod protected_quotes;
#[path = "history/quick_actions.rs"]
mod quick_actions;
#[path = "history/reactions.rs"]
mod reactions;
#[path = "history/read.rs"]
mod read;
#[path = "history/references.rs"]
mod references;
#[path = "history/reply.rs"]
mod reply;
pub(super) use references::OpenConversationReference;
#[path = "history/source.rs"]
mod source;
pub(super) use source::Failure as MessageHistoryError;
#[path = "history/viewport.rs"]
mod viewport;
pub(super) use viewport::GuardedText;

use std::{cell::Cell, collections::HashMap, rc::Rc, sync::Arc, time::Duration};

use futures::channel::oneshot;
use gpui::{
    AnyView, App, BackgroundExecutor, Context, Entity, EventEmitter, FocusHandle, IntoElement,
    KeyDownEvent, ObjectFit, Pixels, Render, ScrollWheelEvent, StyleRefinement, StyledImage,
    Subscription, Task, Window, canvas, div, img, prelude::*, px,
};
use tokio::sync::broadcast::{Receiver, error::TryRecvError};
use uuid::Uuid;
use workspace_core::{
    bootstrap::ChatList,
    messenger::details::SidebarDetails,
    messenger::{
        catalog::Chat,
        encryption::{EncryptionActivation, KeyChange},
    },
    storage::{HistoryDirection, HistoryEdge, HistoryScope, HistoryWindow, OutboxEntry},
    sync::{HistoryRevocation, SyncChange, SyncUpdate},
};

use self::{
    attachments::{Attachments, PhotoReturn},
    details::Panel,
    encryption::{HistoryEncryption, PrepareEncrypted, PreparedBody, ciphertexts},
    gallery_source::GalleryExport,
    message_menu::MessageMenu,
    reactions::Reactions,
    reply::ReplyControls,
    source::{Failure, Load, LoadResult, Snapshot, Source, runtime_source},
    viewport::{Position, Viewport},
};
use super::{
    avatars::AvatarImages,
    composer::{
        Composer, ComposerEvent, DestinationError, QueueWriteOutcome, SendSource, Submission,
    },
    contracts::{AccountSession, ChatEntry, ConversationAddress, ConversationReply},
    preferences::{Language, Preferences},
    right_panel::RightPanel,
    runtime::RuntimeClient,
    ui::{
        Button, ButtonAppearance, ConversationNotice, Icon, PANEL_RADIUS, Palette, SURFACE_RADIUS,
        TextRole, animation::Motion, label, popup::tooltip,
    },
};

const HISTORY_PAGE_AHEAD: Pixels = px(1200.);
const MESSAGE_MARK_DURATION: Duration = Duration::from_secs(2);

/// Точка чтения, которую рабочее окно может восстановить при повторном открытии чата.
#[derive(Clone, Copy)]
pub(super) struct ReadingAnchor {
    pub id: Uuid,  // UUID подтверждённого сообщения.
    pub y: Pixels, // Верх пузыря относительно области прокрутки, в логических px.
}

/// Окно для перехода, проверенное и подготовленное до смены видимой ленты.
pub(super) struct PreparedMessageHistory {
    address: ConversationAddress, // Точный адрес запроса, включая ревизию перехода.
    snapshot: Snapshot,           // Окно с целевым UUID, краями и допуском к вложениям.
    checked: Vec<PreparedBody>,   // Отзываемые представления целых зашифрованных сообщений.
    encryption_revision: Option<u64>, // Ревизия защиты уже открытого экрана той же темы.
    key_changes: Option<MessageHistoryKeys>, // Изменения ключей между подготовкой и показом.
    flight: Flight,               // Один идентификатор для загрузки, применения и замера.
}

/// Подготовка с версиями ключей и тел действующего экрана той же темы.
pub(super) struct MessageHistoryEncryption {
    account: AccountSession,       // Аккаунт и поколение уже открытого окна.
    scope: HistoryScope,           // Тема открытого окна.
    revision: u64,                 // Версия состояния на момент сбора подготовки.
    preparation: PrepareEncrypted, // Условия приёма расшифрованных сообщений.
}

/// Следит за ключами до подписки нового экрана и до применения окна в действующем.
struct MessageHistoryKeys {
    changes: Receiver<KeyChange>, // Подписка, начатая перед загрузкой цели.
    activation: EncryptionActivation, // Точный выбор ключевого профиля.
}

impl PreparedMessageHistory {
    /// Проверяет, остались ли приготовленные состояния ключей пригодными для первого кадра.
    pub(super) fn ready_for_show(&mut self) -> bool {
        let Some(keys) = &mut self.key_changes else {
            return true;
        };
        let window = &self.snapshot.window;
        let mut output = true;
        loop {
            match keys.changes.try_recv() {
                Ok(change) => {
                    if change.activation == keys.activation
                        && change.stream_id == window.scope.stream_id
                        && ciphertexts(window, self.snapshot.outbox.as_deref().unwrap_or_default())
                            .any(|(_, key, _)| key == change.key_id)
                    {
                        output = false;
                    }
                }
                Err(TryRecvError::Empty) => return output,
                Err(TryRecvError::Lagged(_) | TryRecvError::Closed) => return false,
            }
        }
    }
}

/// Запрашивает подтверждённое окно вокруг цели, пока прежний разговор остаётся видимым.
pub(super) async fn prepare_message_history(
    runtime: RuntimeClient,
    address: ConversationAddress,
    background: BackgroundExecutor,
    existing: Option<MessageHistoryEncryption>,
) -> Result<PreparedMessageHistory, MessageHistoryError> {
    let ChatEntry::Around(id) = address.route.entry else {
        return Err(Failure::MissingMessage);
    };
    let flight = Flight {
        id: Uuid::new_v4(),
        load: Load::Open(address.route.entry),
    };
    let account = AccountSession {
        owner: address.route.owner.clone(),
        generation: address.generation,
    };
    let existing = existing.filter(|encryption| {
        encryption.account == account && encryption.scope == address.route.scope
    });
    let encryption_revision = existing.as_ref().map(|encryption| encryption.revision);
    let key_profile = runtime.message_encryption(&account);
    let key_changes = key_profile
        .as_ref()
        .map(|(engine, activation)| MessageHistoryKeys {
            changes: engine.subscribe(),
            activation: activation.clone(),
        });
    let preparation = match existing {
        Some(encryption) => Some(encryption.preparation),
        None => key_profile.map(|(engine, activation)| {
            PrepareEncrypted::for_opening(engine, activation, address.route.scope.stream_id)
        }),
    };
    let source = runtime_source(runtime);
    let (cached, _) = oneshot::channel();
    let response = source(address.clone(), flight.load, flight.id, cached).await;
    let snapshot = target_snapshot(response, &address, id)?;
    let checked = match preparation {
        Some(preparation) => {
            preparation
                .window(
                    &snapshot.window,
                    snapshot.outbox.as_deref().unwrap_or_default(),
                    &background,
                )
                .await
        }
        None => Vec::new(),
    };
    // Пока готовился защищённый текст, журнал мог заменить или отозвать сохранённые строки.
    let (cached, _) = oneshot::channel();
    let response = source(
        address.clone(),
        Load::Read(snapshot.window.id),
        flight.id,
        cached,
    )
    .await;
    let mut latest = target_snapshot(response, &address, id)?;
    if key_changes.is_some()
        && !ciphertexts(
            &snapshot.window,
            snapshot.outbox.as_deref().unwrap_or_default(),
        )
        .eq(ciphertexts(
            &latest.window,
            latest.outbox.as_deref().unwrap_or_default(),
        ))
    {
        return Err(Failure::Changed);
    }
    latest.media_access = snapshot.media_access;
    Ok(PreparedMessageHistory {
        address,
        snapshot: latest,
        checked,
        encryption_revision,
        key_changes,
        flight,
    })
}

/// Принимает только окно точной области, в котором подтверждён целевой UUID.
fn target_snapshot(
    response: ConversationReply<LoadResult>,
    address: &ConversationAddress,
    id: Uuid,
) -> Result<Snapshot, MessageHistoryError> {
    let snapshot = response
        .for_current(Some(address))
        .ok_or(Failure::Changed)??
        .filter(|snapshot| {
            snapshot.window.scope == address.route.scope && !snapshot.window.needs_refresh
        })
        .ok_or(Failure::Changed)?;
    if !snapshot
        .window
        .messages
        .iter()
        .any(|message| message.id == id)
    {
        return Err(Failure::MissingMessage);
    }
    Ok(snapshot)
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
    view: Entity<Composer>,                    // Ввод текущего разговора.
    source: SendSource,                        // Проверка адреса и запись на исполнителе.
    _intents: Subscription,                    // Передаёт действия редактора только этому экрану.
    _measure: Subscription, // Изменение редактора отзывает его последнее измерение.
    measured: Rc<Cell<Option<ComposerFrame>>>, // Для переиспользования представления той же высоты.
}

/// Измерение композера, допустимое только при том же окружении рабочего экрана.
#[derive(Clone, Copy)]
struct ComposerFrame {
    style: ComposerFrameStyle, // Ширина и параметры, влияющие на текст и оформление.
    height: Pixels,            // Высота уже размещённого композера.
}

/// Условия, при которых рабочее окно сохраняет размер и оформление композера.
#[derive(Clone, Copy, PartialEq)]
struct ComposerFrameStyle {
    width: Pixels,      // Ширина центральной колонки после дока боковых панелей.
    palette: Palette,   // Тема и видимость рамок фокуса.
    language: Language, // Язык подписей и сообщений редактора.
}

/// Подписи разговора из каталога того же входа.
#[derive(Default, PartialEq, Eq)]
struct Heading {
    chat: Option<String>,         // Имя собеседника или канала.
    topic: Option<String>,        // Название темы, если открыт её адрес.
    direct: bool,                 // В заголовке лички оставляем место под аватар.
    direct_peer_id: Option<Uuid>, // UUID собеседника из каталога этого входа, если он известен.
    color: Option<u32>,           // Цвет аватара канала из каталога.
    encrypted: Option<bool>,      // Серверный режим шифрования независимо от доступности редактора.
    topic_closed: bool,           // Последнее известное состояние выбранной темы.
    default_topic: Option<Uuid>,  // Тема лички для отзыва до завершения проверки отправки.
    native_markdown: bool,        // Каталог подтвердил обычный нативный разговор.
}

impl Heading {
    /// Берёт имя и фото из строки каталога для указанной темы.
    fn from_chat(chat: &Chat, scope: HistoryScope) -> Self {
        Self {
            chat: Some(chat.name.clone()),
            topic: chat
                .topics
                .iter()
                .find(|topic| Some(topic.id) == scope.topic_id)
                .map(|topic| topic.name.clone()),
            direct: chat.direct,
            color: chat.metadata.color.filter(|color| *color <= 0xFF_FF_FF),
            encrypted: chat.metadata.encrypted,
            default_topic: chat.metadata.default_topic_id,
            topic_closed: chat
                .topics
                .iter()
                .find(|topic| {
                    Some(topic.id)
                        == scope.topic_id.or_else(|| {
                            chat.direct
                                .then_some(chat.metadata.default_topic_id)
                                .flatten()
                        })
                })
                .is_some_and(|topic| topic.metadata.is_done),
            native_markdown: chat.metadata.native_markdown,
            direct_peer_id: chat
                .metadata
                .direct_peer_id
                .filter(|id| chat.direct && !id.is_nil()),
        }
    }
}

/// Переход из правой панели, который должна подтвердить оболочка.
pub(super) struct OpenDetailsTopic {
    pub address: ConversationAddress, // Полный адрес панели, откуда пришло действие.
    pub topic_id: Uuid,               // UUID выбранной темы в том же канале.
}

impl EventEmitter<OpenDetailsTopic> for Conversation {}

/// Действие профиля, для которого нужен маршрут рабочего окна.
#[derive(Clone, Copy)]
pub(super) enum DetailsProfileAction {
    Favorites, // Перейти в избранное текущего пользователя.
    Edit,      // Открыть редактирование личных данных.
}

/// Запрос со страницы своего профиля, ещё привязанный к открытому разговору.
#[derive(Clone)]
pub(super) struct DetailsProfileIntent {
    pub address: ConversationAddress, // Адрес панели для отбрасывания устаревшего действия.
    pub action: DetailsProfileAction, // Выбранный переход.
    pub opener: FocusHandle,          // Кнопка, к которой можно вернуться после редактирования.
}

impl EventEmitter<DetailsProfileIntent> for Conversation {}

/// Пользователь явно выбрал хвост вместо ожидаемого перехода к сообщению.
pub(super) struct HistoryTargetCleared;

impl EventEmitter<HistoryTargetCleared> for Conversation {}

/// История, адрес открытия и очередь сообщений одного экрана.
pub struct Conversation {
    address: ConversationAddress, // Владелец, область, поколение и ревизия от оболочки.
    entry: ChatEntry,             // Текущая точка открытия, включая переход к последним.
    restore: Option<ReadingAnchor>, // Сохранённая опора до первого успешного снимка.
    source: Source,               // Переносит запросы на рабочий поток.
    state: State,                 // Текущее подтверждённое окно или сообщение о проблеме.
    active: Option<Flight>,       // Один запрос истории, пока не применён его последний ответ.
    read_pending: bool, // Во время запроса пришло событие сохранённой истории или очереди.
    read_revalidation: Option<Option<HistoryScope>>, // Область серверной сверки. Some(None) означает всё окно.
    read_revalidation_waiting: bool, // После ошибки повторяем по фокусу или следующему сигналу Sync.
    reading: read::Reading,          // Видимость и ход серверных отметок для открытого окна.
    action_error: Option<Flight>,    // Запрос, который можно повторить, пока открыта история.
    outbox: Vec<OutboxEntry>,        // Последний снимок очереди в области разговора.
    view_width: Cell<Option<Pixels>>, // Ширина рабочей области от владельца раскладки.
    viewport: Viewport,              // Виртуальный список и его положение.
    message_mark: Option<Task<()>>,  // Снимает отметку последнего перехода после показа цели.
    encryption: Option<HistoryEncryption>, // Только живые представления текущего адреса.
    scroll_direction: Option<HistoryDirection>, // Последний жест, до него проверяем заполнение окна.
    attachments: Attachments, // Карточки и одиночный просмотр вложений этого разговора.
    gallery_export: Option<GalleryExport>, // Выбор места и запись одного файла из галереи.
    calls: calls::Calls,      // Настройки Jitsi и ожидаемая доставка того же разговора.
    screen_focus: FocusHandle, // Запасной фокус при исчезновении истории и панели.
    history_focus: FocusHandle, // Точка клавиатурной прокрутки в открытом разговоре.
    message_menu: MessageMenu, // Меню выбранного подтверждённого сообщения.
    reactions: Reactions,     // Популярность и ожидающие реакции открытого разговора.
    message_delete: message_delete::MessageDelete, // Подтверждение и ход удаления одного сообщения.
    time_reference: Option<references::TimeReference>, // Панель момента из активной строки истории.
    sending: Option<Sending>, // Рабочий редактор; изолированные тесты истории могут его не подключать.
    heading: Heading,         // Отображаемые названия из текущего каталога.
    local_key: Option<bool>,  // Наличие локального ключа, None до завершения чтения.
    local_key_revision: u64,  // Позволяет отбросить результат старого чтения ключей.
    server_topic_closed: bool, // Серверная проверка отправки подтвердила закрытую тему.
    local_draft_request: Option<(Uuid, Option<Uuid>)>, // Последняя локально запрошенная тема и UUID.
    opening_draft_id: Option<Uuid>, // Явно выбранный UUID черновика, если он есть.
    heading_details: Option<Arc<SidebarDetails>>, // Подтверждённое присутствие или состав.
    heading_request: Option<Task<()>>, // Чтение сведений для шапки при закрытой панели.
    favorites_presentation: bool,   // Показывает заголовок «Избранное» для чата с собой.
    avatar_runtime: Option<RuntimeClient>, // Рабочий поток аватаров. Отсутствует в тестах истории без Core.
    avatars_visible: bool,                 // Разговор показан окном и может запрашивать фото.
    heading_avatars: AvatarImages,         // Подписка на фото собеседника, указанного в шапке.
    history_avatars: AvatarImages,         // Подписки на фото авторов в видимой части истории.
    history_avatar_users: Vec<Uuid>, // UUID из последнего размещения истории, в порядке сортировки.
    details: Option<Panel>,          // Панель штатного разговора с доступом к рабочему потоку.
}

impl Conversation {
    /// Возвращает фокус истории и открытого редактора для обхода рабочего окна.
    pub(super) fn history_and_editor_focus(&self, app: &App) -> (FocusHandle, Option<FocusHandle>) {
        let editor = self
            .sending
            .as_ref()
            .map(|sending| sending.view.read(app))
            .filter(|_| self.local_composer_visible())
            .map(|editor| editor.editor_focus(app));
        (self.history_focus.clone(), editor)
    }

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
        Self::open_with_draft(address, runtime, right_panel, restore, None, cx)
    }

    /// Открывает тот же разговор и передаёт редактору выбранный UUID черновика.
    pub(super) fn open_with_draft(
        address: ConversationAddress,
        runtime: RuntimeClient,
        right_panel: Entity<RightPanel>,
        restore: Option<ReadingAnchor>,
        draft_id: Option<Uuid>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::configured(address, runtime, right_panel, restore, draft_id, cx);
        view.start(view.opening_load(), cx);
        view
    }

    /// Показывает заранее подготовленное окно в первом кадре нового разговора.
    pub(super) fn open_prepared(
        address: ConversationAddress,
        runtime: RuntimeClient,
        right_panel: Entity<RightPanel>,
        prepared: PreparedMessageHistory,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::configured(address, runtime, right_panel, None, None, cx);
        if view.address == prepared.address {
            view.show_prepared_message(prepared, cx);
        } else {
            view.start(view.opening_load(), cx);
        }
        view
    }

    /// Возвращается к положению ленты из истории переходов, сохраняя адрес экрана.
    pub(super) fn open_at_reading_position(
        address: ConversationAddress,
        runtime: RuntimeClient,
        right_panel: Entity<RightPanel>,
        restore: Option<ReadingAnchor>,
        draft_id: Option<Uuid>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::configured(address, runtime, right_panel, None, draft_id, cx);
        if !view.is_archived() {
            view.entry = ChatEntry::Latest;
            view.restore = restore;
        }
        view.start(Load::Open(view.opening_entry()), cx);
        view
    }

    /// Подключает редактор, панели и разрешение шифрования до первого показа истории.
    fn configured(
        address: ConversationAddress,
        runtime: RuntimeClient,
        right_panel: Entity<RightPanel>,
        restore: Option<ReadingAnchor>,
        draft_id: Option<Uuid>,
        cx: &mut Context<Self>,
    ) -> Self {
        let account = AccountSession {
            owner: address.route.owner.clone(),
            generation: address.generation,
        };
        let activated = runtime.message_encryption(&account);
        let activation = activated.as_ref().map(|(_, activation)| activation.clone());
        let encryption = activated.map(|(engine, activation)| {
            HistoryEncryption::new(engine, activation, address.route.scope.stream_id, cx)
        });
        let mut view = Self::unopened(
            address,
            runtime_source(runtime.clone()),
            restore,
            encryption,
            cx,
        );
        view.opening_draft_id = draft_id;
        if !view.is_archived() {
            view.attach_composer(
                SendSource::runtime(runtime.clone(), draft_id, activation),
                cx,
            );
            view.details = Some(Panel::new(
                right_panel,
                view.address.clone(),
                runtime.clone(),
                cx,
            ));
        }
        view.attachments.runtime = Some(runtime.clone());
        view.avatar_runtime = Some(runtime.clone());
        view.reading.runtime = Some(runtime.clone());
        view.calls.set_runtime(runtime.clone());
        view.load_reaction_usage(cx);
        if let Some(sending) = &view.sending {
            sending.view.update(cx, |composer, cx| {
                composer.attach_drafts(runtime.clone());
                composer.attach_early_draft(draft_id, cx);
            });
        }
        view.refresh_local_key(false, cx);
        view
    }

    /// Создаёт экран с заданным источником, в том числе с управляемыми ответами в GPUI-тестах.
    #[cfg(all(test, feature = "gpui-test"))]
    fn with_source(
        address: ConversationAddress,
        source: Source,
        restore: Option<ReadingAnchor>,
        encryption: Option<HistoryEncryption>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut conversation = Self::unopened(address, source, restore, encryption, cx);
        conversation.start(Load::Open(conversation.opening_entry()), cx);
        conversation
    }

    /// Создаёт состояние истории, которое ещё не запускает запросы и не отдаёт строки в окно.
    fn unopened(
        address: ConversationAddress,
        source: Source,
        restore: Option<ReadingAnchor>,
        encryption: Option<HistoryEncryption>,
        cx: &mut Context<Self>,
    ) -> Self {
        let entry = match address.route.entry {
            ChatEntry::Archived => ChatEntry::Latest,
            entry => entry,
        };
        Self {
            address,
            entry,
            restore: restore.filter(|_| entry == ChatEntry::Latest),
            source,
            state: State::Opening,
            active: None,
            read_pending: false,
            read_revalidation: None,
            read_revalidation_waiting: false,
            reading: read::Reading::default(),
            action_error: None,
            outbox: Vec::new(),
            view_width: Cell::new(None),
            viewport: Viewport::with_page_ahead(HISTORY_PAGE_AHEAD),
            message_mark: None,
            encryption,
            scroll_direction: None,
            attachments: Attachments::new(cx),
            gallery_export: None,
            calls: calls::Calls::new(cx),
            screen_focus: cx.focus_handle(),
            history_focus: cx.focus_handle().tab_stop(true),
            message_menu: MessageMenu::new(cx.focus_handle()),
            reactions: Reactions::default(),
            message_delete: message_delete::MessageDelete::default(),
            time_reference: None,
            sending: None,
            heading: Heading::default(),
            local_key: None,
            local_key_revision: 0,
            server_topic_closed: false,
            local_draft_request: None,
            opening_draft_id: None,
            heading_details: None,
            heading_request: None,
            favorites_presentation: false,
            avatar_runtime: None,
            avatars_visible: false,
            heading_avatars: AvatarImages::default(),
            history_avatars: AvatarImages::default(),
            history_avatar_users: Vec::new(),
            details: None,
        }
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

    /// Проверяет, что цель уже находится в действующем, подтверждённом окне.
    pub(super) fn can_reveal_message(&self, id: Uuid) -> bool {
        matches!(&self.state, State::Ready(window) if !window.needs_refresh)
            && !self.read_pending
            && !self
                .active
                .is_some_and(|flight| matches!(flight.load, Load::Read(_)))
            && self.viewport.contains_confirmed(id)
    }

    /// Снимает правила подготовки текущей темы для фонового запроса нового окна.
    pub(super) fn message_history_encryption(&self) -> Option<MessageHistoryEncryption> {
        let encryption = self.encryption.as_ref()?;
        Some(MessageHistoryEncryption {
            account: self.avatar_session(),
            scope: self.address.route.scope,
            revision: encryption.revision(),
            preparation: encryption.prepare_for_navigation(),
        })
    }

    /// Показывает цель в существующем списке, сохраняя редактор и его фокус.
    pub(super) fn reveal_message(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.can_reveal_message(id) {
            self.mark_message(id, Uuid::new_v4(), cx);
        }
    }

    /// Применяет готовое окно в текущей теме, сохраняя живой экран и редактор.
    pub(super) fn apply_prepared_message(
        &mut self,
        prepared: PreparedMessageHistory,
        cx: &mut Context<Self>,
    ) -> bool {
        let target = &prepared.address;
        if !self.has_access()
            || self.is_archived()
            || self.address.route.scope != target.route.scope
            || self.address.route.owner != target.route.owner
            || self.address.generation != target.generation
            || prepared.encryption_revision.is_some_and(|revision| {
                self.encryption.as_ref().map(HistoryEncryption::revision) != Some(revision)
            })
        {
            return false;
        }
        self.show_prepared_message(prepared, cx);
        true
    }

    /// Устанавливает подтверждённые строки и помечает цель до первого кадра.
    fn show_prepared_message(&mut self, prepared: PreparedMessageHistory, cx: &mut Context<Self>) {
        let PreparedMessageHistory {
            address,
            snapshot,
            checked,
            flight,
            ..
        } = prepared;
        if let ChatEntry::Around(id) = address.route.entry {
            let reread = self.read_pending
                || self
                    .active
                    .is_some_and(|flight| matches!(flight.load, Load::Read(_)));
            self.active = None;
            self.read_pending = false;
            self.reading.clear();
            self.action_error = None;
            self.restore = None;
            self.entry = ChatEntry::Around(id);
            self.show(snapshot, checked, flight, false, cx);
            if self.has_access() && self.viewport.contains_confirmed(id) {
                self.mark_message(id, flight.id, cx);
                if reread {
                    self.queue_read(cx);
                }
            }
        }
    }

    /// Возвращает положение чтения на месте, когда нужное окно уже открыто.
    pub(super) fn restore_reading_position(
        &mut self,
        position: Option<ReadingAnchor>,
        cx: &mut Context<Self>,
    ) -> bool {
        let usable = match position {
            Some(position) => self.can_reveal_message(position.id),
            None => matches!(&self.state, State::Ready(window)
                if !window.needs_refresh && window.newer == HistoryEdge::End),
        };
        if usable {
            self.clear_message_mark();
            self.reading.clear_observations();
            self.entry = ChatEntry::Latest;
            self.scroll_direction = None;
            self.viewport.restore_position(position);
            cx.notify();
        }
        usable
    }

    /// Ставит одноразовую отметку и заменяет таймер предыдущей цели.
    fn mark_message(&mut self, id: Uuid, op_id: Uuid, cx: &mut Context<Self>) {
        if !self.viewport.has_visible_message(id) {
            self.reading.clear_observations();
        }
        self.message_mark = None;
        self.scroll_direction = None;
        self.viewport.center_message(id, op_id);
        cx.notify();
    }

    /// Начинает отсчёт после кадра, в котором впервые выставлена нужная позиция.
    fn start_message_mark_timer(&mut self, op_id: Uuid, cx: &mut Context<Self>) {
        if !self.viewport.has_message_mark(op_id) {
            return;
        }
        let delay = cx.background_executor().timer(MESSAGE_MARK_DURATION);
        self.message_mark = Some(cx.spawn(async move |entity, cx| {
            delay.await;
            let _ = entity.update(cx, |view, cx| {
                if view.viewport.has_message_mark(op_id) {
                    view.viewport.clear_message_mark();
                    cx.notify();
                }
            });
        }));
    }

    /// Убирает подсветку и отменяет одноразовое центрирование при смене места чтения.
    fn clear_message_mark(&mut self) {
        self.message_mark = None;
        self.viewport.clear_message_mark();
    }

    /// Использует обычную загрузку вокруг UUID для восстановления разрешённого окна.
    fn opening_entry(&self) -> ChatEntry {
        self.restore
            .map_or(self.entry, |anchor| ChatEntry::Around(anchor.id))
    }

    /// Выбирает первый непрочитанный только для обычного входа без сохранённой цели.
    fn opening_load(&self) -> Load {
        let entry = self.opening_entry();
        if entry == ChatEntry::Latest && !self.is_archived() {
            Load::OpenUnread
        } else {
            Load::Open(entry)
        }
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
                    ComposerEvent::DestinationChanged => {
                        let editor = sender.read(cx);
                        if editor.destination_error() == Some(DestinationError::TopicClosed) {
                            conversation.server_topic_closed = true;
                        } else if editor.is_available() {
                            conversation.server_topic_closed = false;
                        }
                        conversation.load_call_settings(cx);
                        cx.notify();
                    }
                    ComposerEvent::EditPrevious => {
                        if conversation
                            .sending
                            .as_ref()
                            .is_some_and(|sending| sending.view.read(cx).can_edit_protected())
                        {
                            conversation.edit_previous_protected(cx);
                        } else {
                            conversation.edit_previous_fragment(cx);
                        }
                    }
                    ComposerEvent::DraftRestored => {
                        sender.update(cx, |composer, cx| {
                            composer.refresh_draft_quotes(
                                |id, fragment| conversation.restored_reply_quote(id, fragment),
                                cx,
                            );
                        });
                    }
                },
            );
        let measured = Rc::new(Cell::new(None));
        let reset = measured.clone();
        let measure = cx.observe(&view, move |_, _, _| reset.set(None));
        self.sending = Some(Sending {
            view,
            source,
            _intents: intents,
            _measure: measure,
            measured,
        });
        if let Some(sending) = &self.sending {
            sending.view.update(cx, |composer, cx| {
                composer.check_destination(&sending.source, cx)
            });
        }
    }

    /// Повторяет проверку отправки, пока история этого разговора доступна.
    fn retry_send_destination(&mut self, cx: &mut Context<Self>) {
        if let Some(sending) = &self.sending {
            sending.view.update(cx, |editor, cx| {
                editor.retry_destination(&sending.source, cx)
            });
        }
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
                editor.finish(submission.attempt, QueueWriteOutcome::WriteFailed, cx)
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
                let Some(outcome) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                let saved = matches!(outcome, QueueWriteOutcome::Saved(_));
                if view
                    .sending
                    .as_ref()
                    .is_some_and(|sending| sending.view.downgrade() == editor)
                {
                    let _ = editor.update(cx, |editor, cx| editor.finish(attempt, outcome, cx));
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

    /// Выбирает оформление чата с собой сразу после создания экрана.
    pub(super) fn set_favorites_presentation(&mut self, favorites: bool) {
        self.favorites_presentation = favorites;
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
        let chats = session
            .and(catalog)
            .filter(|catalog| catalog.generation == self.address.generation)
            .and_then(|catalog| catalog.chats.as_ref());
        if let Some(sending) = &self.sending {
            sending.view.update(cx, |composer, cx| {
                composer.set_reference_chats(chats.cloned(), cx)
            });
        }
        let heading = chats
            .and_then(|chats| chats.iter().find(|chat| chat.id == scope.stream_id))
            .map(|chat| Heading::from_chat(chat, scope));
        if !self.is_archived() || heading.is_some() {
            self.set_heading(heading.unwrap_or_default(), cx);
        }
        if let Some(chat) =
            chats.and_then(|chats| chats.iter().find(|chat| chat.id == scope.stream_id))
            && chat.metadata.encrypted == Some(false)
            && let Some(topic) = scope.topic_id.or_else(|| {
                chat.direct
                    .then_some(chat.metadata.default_topic_id)
                    .flatten()
            })
        {
            self.load_local_draft(topic, cx);
        } else {
            self.local_draft_request = None;
        }
        if self
            .details
            .as_mut()
            .is_some_and(|panel| panel.set_catalog(chats))
        {
            cx.notify();
        }
    }

    /// Показывает подтверждённое название разговора из панели создания или архива.
    pub(super) fn set_new_chat_heading(&mut self, chat: &Chat, cx: &mut Context<Self>) {
        let scope = self.address.route.scope;
        if scope.stream_id == chat.id && !matches!(self.state, State::Failed(Failure::AccessLost)) {
            self.set_heading(Heading::from_chat(chat, scope), cx);
        }
    }

    /// Обновляет оформление шапки и подписку на фото, только если изменились данные.
    fn set_heading(&mut self, heading: Heading, cx: &mut Context<Self>) {
        if heading != self.heading {
            self.heading = heading;
            self.refresh_heading_avatar(cx);
            cx.notify();
        }
    }

    /// Различает архивный просмотр и рабочую историю того же разговора.
    fn is_archived(&self) -> bool {
        self.address.route.entry == ChatEntry::Archived
    }

    /// Решает показ поля по локальному каталогу, не ожидая истории и сервера отправки.
    fn local_composer_visible(&self) -> bool {
        !self.is_archived()
            && !matches!(self.state, State::Failed(Failure::AccessLost))
            && !self.heading.topic_closed
            && !self.server_topic_closed
            && (self.heading.chat.is_none()
                || self.heading.direct
                || self.address.route.scope.topic_id.is_some())
            && (self.heading.encrypted != Some(true) || self.local_key == Some(true))
    }

    /// Асинхронно читает локальное наличие ключа и отбрасывает устаревший результат.
    fn refresh_local_key(&mut self, keep_current: bool, cx: &mut Context<Self>) {
        self.local_key_revision = self.local_key_revision.wrapping_add(1);
        let revision = self.local_key_revision;
        if !keep_current {
            self.local_key = None;
        }
        cx.notify();
        let Some(runtime) = &self.avatar_runtime else {
            return;
        };
        let Some((engine, activation)) = runtime.message_encryption(&self.avatar_session()) else {
            self.local_key = Some(false);
            return;
        };
        let stream = self.address.route.scope.stream_id;
        let task = cx
            .background_executor()
            .spawn(async move { engine.has_local_key(&activation, stream).ok() });
        cx.spawn(async move |view, cx| {
            let available = task.await;
            let _ = view.update(cx, |view, cx| {
                if view.local_key_revision == revision {
                    view.local_key = available;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Читает локальный обычный черновик независимо от проверки отправки.
    fn load_local_draft(&mut self, topic: Uuid, cx: &mut Context<Self>) {
        let request = (topic, self.opening_draft_id);
        if self.local_draft_request == Some(request) {
            return;
        }
        let (Some(runtime), Some(sending)) = (&self.avatar_runtime, &self.sending) else {
            return;
        };
        let runtime = runtime.clone();
        let editor = sending.view.clone();
        let stamp = editor.read(cx).draft_stamp(cx);
        let address = self.address.clone();
        self.local_draft_request = Some(request);
        cx.spawn(async move |view, cx| {
            let reply = runtime
                .conversation(address, move |services, address| async move {
                    let scope = workspace_core::storage::DraftScope {
                        stream_id: address.route.scope.stream_id,
                        topic_id: topic,
                    };
                    // Не возвращаем ввод, который другая операция уже передаёт очереди.
                    services.drafts.wait_for_send(&address, scope).await;
                    let stored = services
                        .sync
                        .read_local_plaintext_draft(
                            &address.route.owner,
                            address.generation,
                            scope,
                            request.1,
                        )
                        .ok()
                        .flatten()?;
                    let pending = services
                        .drafts
                        .current(&address, scope, request.1)
                        .filter(|(_, pending, _)| request.1.is_none_or(|id| id == pending.id));
                    Some(match (stored, pending) {
                        (Some(saved), Some((_, pending, _)))
                            if saved.id == pending.id && pending.version > saved.version =>
                        {
                            Some(pending)
                        }
                        (Some(saved), Some((_, pending, _)))
                            if request.1.is_none() && saved.id != pending.id =>
                        {
                            Some(pending)
                        }
                        (Some(saved), _) => Some(saved),
                        (None, Some((_, pending, _))) => Some(pending),
                        (None, None) => None,
                    })
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.local_draft_request != Some(request)
                    || view.heading.encrypted != Some(false)
                    || matches!(view.state, State::Failed(Failure::AccessLost))
                {
                    return;
                }
                if let Some(Ok(Some(saved))) = reply.for_current(Some(&view.address)) {
                    editor.update(cx, |editor, cx| editor.show_local_draft(saved, stamp, cx));
                }
            });
        })
        .detach();
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
            self.cancel_gallery_export();
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
            self.load_reaction_usage(cx);
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
        match &update.change {
            SyncChange::HistoryAccessRevoked(_) => {
                if let Some(sending) = &self.sending {
                    sending
                        .view
                        .update(cx, |composer, cx| composer.close_revoked_fragment_edit(cx));
                }

                // Файл может принадлежать другой области того же владельца.
                self.attachments.invalidate(|_| true, cx);
                self.cancel_gallery_export();
                if let (SyncChange::HistoryAccessRevoked(scope), Some(encryption)) =
                    (&update.change, &mut self.encryption)
                {
                    let (stream, topic) = match scope {
                        HistoryRevocation::Stream(stream) => (*stream, None),
                        HistoryRevocation::Topic {
                            stream_id,
                            topic_id,
                        } => (*stream_id, Some(*topic_id)),
                    };
                    let rows = encryption.quotes.invalidate_scope(stream, topic);
                    self.refresh_quote_rows(rows, cx);
                }
                if let SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(stream)) =
                    &update.change
                {
                    for row in self.viewport.fragment_views.invalidate_stream(*stream, cx) {
                        self.viewport.invalidate_row_height(row);
                    }
                }
            }
            SyncChange::HistoryMessageChanged { message_id } => {
                self.message_menu.invalidate(Some(*message_id));
                self.encrypted_message_changed(*message_id, cx);
                self.viewport.invalidate_fragment_row(*message_id, cx);
                if let Some(sending) = &self.sending {
                    sending.view.update(cx, |composer, cx| {
                        composer.cancel_protected_edit(Some(*message_id), cx)
                    });
                }
                self.attachments
                    .invalidate(|key| key.message == *message_id, cx);
                self.invalidate_gallery_export(|message, _| message == *message_id);
            }
            SyncChange::QuoteOriginChanged { message_id } => {
                self.quoted_origin_changed(*message_id, cx);
            }
            SyncChange::MediaFileInvalidated { file_id } => {
                self.attachments.invalidate(|key| key.file == *file_id, cx);
                self.invalidate_gallery_export(|_, file| file == *file_id);
            }
            _ => {}
        }
        self.refresh_open_gallery(&update.change, cx);
        match update.change {
            SyncChange::MessengerReset => {
                self.invalidate_send_destination(cx);
                self.restart(cx);
            }
            SyncChange::MessengerSourceReady => {
                self.rearm_read_revalidation(cx);
                if self.reading.rearm_failed() {
                    cx.notify();
                }
            }
            SyncChange::QuickReactionUsageChanged => self.load_reaction_usage(cx),
            SyncChange::SidebarDetailsChanged { stream_id } if scope.stream_id == stream_id => {
                if let Some(sending) = &self.sending {
                    sending.view.update(cx, |composer, cx| {
                        composer.refresh_mention_members(false, cx)
                    });
                }
                self.refresh_open_details(cx);
                self.refresh_heading_details(cx);
                if self.reset_key_reply_permissions(None) {
                    cx.notify();
                }
                self.check_key_reply_permissions(cx);
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
                None if (self.heading.direct || self.heading.chat.is_none())
                    && self
                        .heading
                        .default_topic
                        .is_none_or(|current| current == topic_id) =>
                {
                    self.fail(Failure::AccessLost, cx);
                    // Личка может получить другую тему по умолчанию. Старый ввод уже отозван.
                    self.heading.default_topic = None;
                    self.reopen(cx);
                }
                None => {
                    if let Some(sending) = &self.sending {
                        sending
                            .view
                            .update(cx, |composer, cx| composer.revoke_draft_topic(topic_id, cx));
                    }
                    self.restart(cx);
                }
                _ => {}
            },
            SyncChange::HistoryMessageCreated { scope: changed, .. }
            | SyncChange::OutboxChanged { scope: changed }
                if includes(scope, changed) =>
            {
                self.queue_read(cx)
            }
            SyncChange::HistoryReadChanged { scope: changed }
                if overlaps_read_scope(scope, changed) =>
            {
                self.reading.refresh_blocked(changed.topic_id);
                self.queue_read(cx)
            }
            SyncChange::HistoryReadRevalidate { scope: changed }
                if changed.is_none_or(|changed| overlaps_read_scope(scope, changed)) =>
            {
                self.request_read_revalidation(changed, cx);
            }
            SyncChange::HistoryMessageChanged { message_id }
            | SyncChange::HistoryMessageReactionsChanged { message_id }
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
        self.clear_message_mark();
        self.calls.cancel();
        self.heading_request.take();
        self.heading_details = None;
        self.cancel_gallery_export();
        if let Some(details) = &mut self.details {
            details.clear(&self.address, None, cx);
        }
        self.state = State::Opening;
        self.active = None;
        self.read_pending = false;
        self.read_revalidation = None;
        self.read_revalidation_waiting = false;
        self.reading.clear();
        self.action_error = None;
        self.scroll_direction = None;
        self.outbox.clear();
        self.viewport.reset_for_reopen();
        if let Some(encryption) = &mut self.encryption {
            encryption.clear();
        }
        self.attachments.reset(cx);
        self.clear_history_avatars();
        self.message_menu.invalidate(None);
        self.message_delete.clear();
        self.reactions.clear_visible();
        // Повтор текущего маршрута, в том числе явный переход в конец, сохраняет его цель.
        self.start(Load::Open(self.opening_entry()), cx);
        if let Some(sending) = &self.sending {
            sending.view.update(cx, |composer, cx| {
                composer.check_destination(&sending.source, cx)
            });
        }
    }

    /// Скрывает историю и делает любые запоздавшие ответы неприменимыми.
    fn fail(&mut self, error: Failure, cx: &mut Context<Self>) {
        self.clear_message_mark();
        self.calls.cancel();
        self.heading_request.take();
        self.heading_details = None;
        self.cancel_gallery_export();
        if let Some(details) = &mut self.details {
            let fallback = matches!(error, Failure::AccessLost).then_some(&self.screen_focus);
            details.clear(&self.address, fallback, cx);
        }
        if matches!(error, Failure::AccessLost)
            && let Some(sending) = &self.sending
        {
            sending.view.update(cx, |composer, cx| {
                composer.cancel_protected_edit(None, cx);
                composer.revoke_draft(cx);
                composer.refresh_mention_members(true, cx);
            });
        }
        self.invalidate_send_destination(cx);
        self.state = State::Failed(error);
        if matches!(error, Failure::AccessLost | Failure::MissingMessage) {
            self.restore = None;
        }
        if error == Failure::AccessLost && self.is_archived() {
            self.set_heading(Heading::default(), cx);
        }
        self.active = None;
        self.read_pending = false;
        self.read_revalidation = None;
        self.read_revalidation_waiting = false;
        self.reading.clear();
        self.action_error = None;
        self.outbox.clear();
        self.viewport.clear();
        if let Some(encryption) = &mut self.encryption {
            encryption.clear();
        }
        self.attachments.reset(cx);
        self.clear_history_avatars();
        self.message_menu.invalidate(None);
        self.message_delete.clear();
        self.reactions.clear_visible();
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

    /// Объединяет несколько входящих событий чтения в одну сверку видимого окна.
    fn request_read_revalidation(&mut self, changed: Option<HistoryScope>, cx: &mut Context<Self>) {
        self.merge_read_revalidation(changed);
        self.read_revalidation_waiting = false;
        self.start_read_revalidation(cx);
    }

    /// Сохраняет точную тему, пока сигналы не затронули несколько разных частей истории.
    fn merge_read_revalidation(&mut self, changed: Option<HistoryScope>) {
        self.read_revalidation = Some(match self.read_revalidation {
            Some(pending) if pending != changed => None,
            _ => changed,
        });
    }

    /// Повторяет отложенную сверку после фокуса или восстановления соединения.
    fn rearm_read_revalidation(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.read_revalidation_waiting) {
            self.start_read_revalidation(cx);
        }
    }

    /// Запускает серверную сверку после окончания текущей операции со снимком.
    fn start_read_revalidation(&mut self, cx: &mut Context<Self>) {
        if !self.read_revalidation_waiting
            && self.active.is_none()
            && let State::Ready(window) = &self.state
            && let Some(changed) = self.read_revalidation.take()
        {
            self.start(Load::RevalidateRead(window.id, changed), cx);
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
        let encryption = self.encryption.as_ref().map(HistoryEncryption::prepare);
        let background = cx.background_executor().clone();
        cx.spawn(async move |target, cx| {
            let (send_cache, receive_cache) = oneshot::channel();
            let response = source(address.clone(), load, flight.id, send_cache);
            let display_cache = async {
                let Ok(snapshot) = receive_cache.await else {
                    return;
                };
                let shows_outbox = snapshot.window.newer == HistoryEdge::End
                    && snapshot
                        .outbox
                        .as_ref()
                        .is_some_and(|rows| !rows.is_empty());
                // Пустой кэш не подтверждает, что в разговоре пока нет сообщений.
                if snapshot.window.messages.is_empty() && !shows_outbox {
                    return;
                }
                let checked = match encryption.clone() {
                    Some(encryption) => {
                        encryption
                            .window(
                                &snapshot.window,
                                snapshot.outbox.as_deref().unwrap_or_default(),
                                &background,
                            )
                            .await
                    }
                    None => Vec::new(),
                };
                let _ = target.update(cx, |view, cx| {
                    if view.accepts(&address, flight) {
                        view.show(snapshot, checked, flight, true, cx);
                    }
                });
            };
            let ((), response) = futures::join!(display_cache, response);
            let checked = match (encryption, &response.value) {
                (Some(encryption), Ok(Some(snapshot))) => {
                    encryption
                        .window(
                            &snapshot.window,
                            snapshot.outbox.as_deref().unwrap_or_default(),
                            &background,
                        )
                        .await
                }
                _ => Vec::new(),
            };
            let _ = target.update(cx, |view, cx| {
                if view.accepts(&address, flight)
                    && let Some(value) = response.for_current(Some(view.address()))
                {
                    view.finish(flight, value, checked, cx);
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
    fn finish(
        &mut self,
        flight: Flight,
        result: LoadResult,
        checked: Vec<PreparedBody>,
        cx: &mut Context<Self>,
    ) {
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
            Ok(Some(snapshot)) => {
                if let Load::RevalidateRead(_, changed) = flight.load {
                    self.reading
                        .refresh_blocked(changed.and_then(|scope| scope.topic_id));
                }
                self.show(snapshot, checked, flight, false, cx);
            }
            Ok(None)
                if matches!(
                    flight.load,
                    Load::Read(_) | Load::Extend(_, _) | Load::RevalidateRead(_, _)
                ) =>
            {
                self.restart(cx);
                return;
            }
            Err(Failure::AccessLost) => self.fail(Failure::AccessLost, cx),
            Err(_) if matches!(flight.load, Load::RevalidateRead(_, _)) && self.has_access() => {
                if let Load::RevalidateRead(_, changed) = flight.load {
                    // При наличии более свежего события сразу проверим объединённую область.
                    self.read_revalidation_waiting = self.read_revalidation.is_none();
                    self.merge_read_revalidation(changed);
                }
            }
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
        self.start_read_revalidation(cx);
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
    fn show(
        &mut self,
        snapshot: Snapshot,
        checked: Vec<PreparedBody>,
        flight: Flight,
        cached: bool,
        cx: &mut Context<Self>,
    ) {
        let Snapshot {
            window,
            outbox,
            media_access,
            first_unread,
            unread_checked,
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
        let unread_position = first_unread
            .filter(|_| !cached && self.entry == ChatEntry::Latest && self.restore.is_none())
            .map(Position::Unread);
        let position = unread_position.unwrap_or_else(|| match &self.state {
            State::Ready(previous)
                if cached
                    && previous.id != window.id
                    && self.entry == ChatEntry::Latest
                    && self.viewport.at_tail() =>
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
                ChatEntry::Latest | ChatEntry::Archived => Position::Tail,
                ChatEntry::Around(id) => Position::Around(id),
            },
        });
        if let Some(outbox) = outbox {
            self.outbox = outbox;
        }
        if flight.load.opens() {
            self.attachments.access = media_access;
        }
        log::debug!(
            "gpui_history op_id={} event=page_received window_id={} rows={} cached={cached}",
            flight.id,
            window.id,
            window.messages.len()
        );
        if let Some(encryption) = &mut self.encryption {
            encryption.retain(&window, &self.outbox);
            encryption.accept(&window, &self.outbox, checked);
        }
        self.reconcile_key_responses(&window, cx);
        if flight.load.opens() {
            self.reading.fresh = !cached;
            self.reading.knows_unread = !cached && unread_checked;
        }
        self.reading.accept_history(&window);
        let unread_marker = self
            .reading
            .knows_unread
            .then(|| window.messages.iter().find(|row| row.read == Some(false)))
            .flatten()
            .map(|row| row.id);
        self.viewport.set_unread_marker(unread_marker);
        self.viewport.replace(
            &window,
            &self.outbox,
            self.address.route.owner.user_id.parse().ok(),
            position,
            flight.id,
            self.encryption.as_ref(),
        );
        self.accept_reaction_rows(&window, flight.id);
        self.attachments.retain(
            |key| {
                self.viewport.has_attachment(key)
                    || self
                        .encryption
                        .as_ref()
                        .is_some_and(|encryption| encryption.has_file(key))
            },
            |source| source.is_retained(self.encryption.as_ref()),
            cx,
        );
        if window.messages.is_empty() {
            self.clear_history_avatars();
        }
        let first_view = !self.has_access();
        self.state = State::Ready(window);
        self.refresh_gallery_counts(cx);
        if first_view {
            self.refresh_heading_details(cx);
        }
        self.resolve_protected_quotes(cx);
        self.attachments.invalidate_revoked(cx);
        self.load_call_settings(cx);
        self.check_key_reply_permissions(cx);
        self.prepare_unresolved_rows(cx);
        if matches!(flight.load, Load::Extend(..)) {
            self.mark_open_gallery_needs_refresh(cx);
        }
        if stalled {
            self.action_error = Some(flight);
        }
        if let Some(sending) = &self.sending {
            sending.view.update(cx, |editor, cx| {
                editor.refresh_draft_quotes(
                    |id, fragment| self.restored_reply_quote(id, fragment),
                    cx,
                );
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
        if self.action_error.is_some_and(|error| error.load.opens()) {
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
        cx.emit(HistoryTargetCleared);
        self.clear_message_mark();
        self.restore = None;
        self.scroll_direction = None;
        self.reading.clear_observations();
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
                self.encryption.as_ref(),
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
            if failure.load.opens() {
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
            .when(self.can_observe_read(), |frame| {
                frame.child(self.viewport.track_read_frame(cx.entity().downgrade()))
            })
    }

    /// Отличает сетевую догрузку от перечитывания уже сохранённых строк.
    fn has_background_history_load(&self) -> bool {
        self.active
            .is_some_and(|flight| flight.load.opens() || matches!(flight.load, Load::Extend(_, _)))
    }

    /// Показывает фоновый запрос поверх списка, сохраняя прокрутку и действия над строками.
    fn render_background_loading(palette: Palette, locale: Language, motion: Motion) -> gpui::Div {
        use gpui::{Animation, bounce, ease_in_out};
        use std::time::Duration;

        let marks = div()
            .flex()
            .gap(px(2.))
            .children([1., 0.75, 0.5].map(|opacity| {
                div()
                    .size(px(3.))
                    .rounded_full()
                    .bg(palette.accent.opacity(opacity))
            }));
        let marks = motion.animate(
            marks,
            "history-background-loading-marks",
            1.,
            || {
                Animation::new(Duration::from_secs(1))
                    .repeat()
                    .with_easing(bounce(ease_in_out))
            },
            |marks, progress| marks.opacity(0.55 + progress * 0.45),
        );

        // У плашки нет фокуса и обработчиков ввода. Прокрутка проходит в список под ней.
        div()
            .absolute()
            .top_2()
            .left_2()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_full()
            .bg(palette.surface)
            .border_1()
            .border_color(palette.border)
            .debug_selector(|| "history-background-loading".into())
            .child(marks)
            .child(label(palette, TextRole::Caption, locale.tr("app.loading")))
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
    /// Получает ширину центральной колонки до построения дерева композера.
    pub(super) fn set_view_width(&self, width: Pixels) {
        self.view_width.set(Some(width));
    }

    /// Выбирает независимые представления, когда окно показывает E2EE и обычные строки.
    pub(super) fn has_encrypted_history(&self) -> bool {
        self.viewport.has_encrypted_rows()
    }

    /// Показывает доступный редактор или отдельное состояние отправки под историей.
    fn render_chat_footer(
        &self,
        sending: &Sending,
        palette: Palette,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let editor = sending.view.read(cx);
        if self.local_composer_visible() {
            if let Some(error) = editor.destination_error() {
                let locale = Preferences::read_for(cx, &self.address.route.owner).language;
                let key = match error {
                    DestinationError::TopicClosed => "desktop.conversation.sending.topicResolved",
                    DestinationError::TopicChanged => "desktop.conversation.sending.topicChanged",
                    DestinationError::DraftUnavailable => {
                        "desktop.conversation.sending.draftUnavailable"
                    }
                    DestinationError::KeyUnavailable => {
                        "desktop.conversation.sending.keyUnavailable"
                    }
                    DestinationError::AccessLost | DestinationError::EncryptionUnavailable => {
                        "desktop.conversation.sending.unavailable"
                    }
                    DestinationError::TopicUnavailable | DestinationError::Unavailable => {
                        "desktop.conversation.sending.checkFailed"
                    }
                };
                return div()
                    .flex()
                    .flex_col()
                    .child(
                        ConversationNotice {
                            text: locale.tr(key).into(),
                            action: Some(Button::new(
                                "conversation-send-retry",
                                locale.tr("desktop.conversation.history.checkAgain"),
                            )),
                        }
                        .render(palette, action(cx, Self::retry_send_destination)),
                    )
                    .child(sending.view.clone())
                    .into_any_element();
            }
            let Some(width) = self
                .view_width
                .get()
                .filter(|_| self.has_encrypted_history())
            else {
                sending.measured.set(None);
                return sending.view.clone().into_any_element();
            };
            let style = ComposerFrameStyle {
                width,
                palette,
                language: Preferences::read_for(cx, &self.address.route.owner).language,
            };
            let view = AnyView::from(sending.view.clone());
            let view =
                if let Some(frame) = sending.measured.get().filter(|frame| frame.style == style) {
                    view.cached(StyleRefinement::default().w_full().h(frame.height))
                } else {
                    view
                };
            let measured = sending.measured.clone();
            return div()
                .relative()
                .flex_none()
                .w_full()
                .min_w_0()
                .child(view)
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, (), _, _| {
                            measured.set(Some(ComposerFrame {
                                style,
                                height: bounds.size.height,
                            }));
                        },
                    )
                    .absolute()
                    .inset_0(),
                )
                .into_any_element();
        }

        sending.measured.set(None);
        let locale = Preferences::read_for(cx, &self.address.route.owner).language;
        if self.heading.chat.is_some()
            && !self.heading.direct
            && self.address.route.scope.topic_id.is_none()
        {
            return ConversationNotice {
                text: locale.tr("chat.selectTopic").into(),
                action: None,
            }
            .render(palette, |_, _| {})
            .into_any_element();
        }
        let error = if self.heading.topic_closed || self.server_topic_closed {
            Some(DestinationError::TopicClosed)
        } else if self.heading.encrypted == Some(true) {
            Some(DestinationError::KeyUnavailable)
        } else {
            editor.destination_error()
        };
        let key = match error {
            None => "desktop.conversation.sending.checkingAccess",
            Some(DestinationError::TopicClosed) => "desktop.conversation.sending.topicResolved",
            Some(DestinationError::AccessLost | DestinationError::EncryptionUnavailable) => {
                "desktop.conversation.sending.unavailable"
            }
            Some(DestinationError::TopicChanged) => "desktop.conversation.sending.topicChanged",
            Some(DestinationError::DraftUnavailable) => {
                "desktop.conversation.sending.draftUnavailable"
            }
            Some(DestinationError::KeyUnavailable) => "desktop.conversation.sending.keyUnavailable",
            Some(DestinationError::TopicUnavailable | DestinationError::Unavailable) => {
                "desktop.conversation.sending.checkFailed"
            }
        };
        ConversationNotice {
            text: locale.tr(key).into(),
            action: error.map(|_| {
                Button::new(
                    "conversation-send-retry",
                    locale.tr("desktop.conversation.history.checkAgain"),
                )
            }),
        }
        .render(palette, action(cx, Self::retry_send_destination))
        .into_any_element()
    }

    /// Рисует ошибку и только то действие, которое может исправить её в этом состоянии.
    fn render_problem(
        &self,
        palette: Palette,
        cause: Option<Failure>,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let locale = Preferences::read_for(cx, &self.address.route.owner).language;
        let t = |key| locale.tr(key);
        let retry: fn(&mut Self, &mut Context<Self>) = Self::retry;
        let (message, command) = match cause {
            Some(Failure::AccessLost) => (
                t("desktop.conversation.history.accessToThisConversationWasRevoked"),
                None,
            ),
            Some(Failure::MissingMessage) => (
                t("desktop.conversation.history.thisMessageIsNoLongerAvailable"),
                Some((
                    "history-latest",
                    t("desktop.conversation.history.goToLatest"),
                    Self::latest as fn(&mut Self, &mut Context<Self>),
                )),
            ),
            Some(_) => (
                t("chat.messagesLoadError"),
                Some(("history-retry", t("common.retry"), retry)),
            ),
            None => (
                t("desktop.conversation.history.couldNotRefreshHistory"),
                Some(("history-retry", t("common.retry"), retry)),
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
        let locale = Preferences::read_for(cx, &self.address.route.owner).language;
        let description: gpui::SharedString =
            locale.tr("desktop.conversation.history.goToLatest").into();
        let mut latest = Button::new("history-latest", "");
        latest.icon = Some(Icon::ChevronDown);
        latest.disabled = self.active.is_some();
        div().absolute().bottom_3().right_3().child(
            latest
                .render(palette, None, action(cx, Self::latest))
                .tooltip(move |_, app| tooltip(palette, description.clone(), app))
                .debug_selector(|| "history-latest".into()),
        )
    }
}

impl Conversation {
    /// Показывает шапку и историю, отделяя первые состояния от ошибок догрузки.
    fn render_chat(&mut self, palette: Palette, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let locale = Preferences::read_for(cx, &self.address.route.owner).language;
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
            .child(self.render_heading(palette, cx))
            .children(self.call_error(palette, cx))
            .children(self.reaction_error(palette, locale));
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
            State::Opening => root.child(loading::skeleton(palette)).children(
                self.sending
                    .as_ref()
                    .map(|sending| self.render_chat_footer(sending, palette, cx)),
            ),
            State::Failed(cause) => root
                .child(centered.child(self.render_problem(palette, Some(*cause), cx)))
                .children(
                    (!matches!(cause, Failure::AccessLost))
                        .then(|| {
                            self.sending
                                .as_ref()
                                .map(|sending| self.render_chat_footer(sending, palette, cx))
                        })
                        .flatten(),
                ),
            State::Ready(history) => {
                if self.viewport.time_zone_changed() {
                    let position = if history.newer == HistoryEdge::End && self.viewport.at_tail() {
                        Position::Tail
                    } else {
                        Position::Reading
                    };
                    self.viewport.replace(
                        history,
                        &self.outbox,
                        self.address.route.owner.user_id.parse().ok(),
                        position,
                        Uuid::new_v4(),
                        self.encryption.as_ref(),
                    );
                }
                let route = &self.address.route;
                let background = Preferences::read_for(cx, &route.owner)
                    .appearance
                    .background
                    .for_chat(Some(route.scope));
                let latest = self.entry != ChatEntry::Latest
                    || history.newer != HistoryEdge::End
                    || !self.viewport.at_tail();
                let empty = history.messages.is_empty()
                    && (history.newer != HistoryEdge::End || self.outbox.is_empty());
                let background_loading = !empty && self.has_background_history_load();
                let peer = self
                    .heading
                    .direct
                    .then(|| self.heading.chat.clone())
                    .flatten();
                let problem = self
                    .action_error
                    .map(|_| self.render_problem(palette, None, cx));
                let viewport = if empty {
                    label(
                        palette,
                        TextRole::Caption,
                        locale.tr("desktop.conversation.history.noMessagesYet"),
                    )
                    .m_auto()
                    .debug_selector(|| "history-empty".into())
                    .into_any_element()
                } else {
                    let reply = Some(ReplyControls {
                        conversation: cx.entity().downgrade(),
                        focus: self.history_focus.clone(),
                        selection: self.viewport.selection(),
                    });
                    let quick_reply = self
                        .sending
                        .as_ref()
                        .is_some_and(|sending| sending.view.read(cx).reply_available());
                    let images: HashMap<_, _> = self
                        .history_avatar_users
                        .iter()
                        .copied()
                        .filter_map(|id| Some((id, self.history_avatars.image(id, cx)?)))
                        .collect();
                    let activation = self
                        .encryption
                        .as_ref()
                        .map(|encryption| &encryption.activation);
                    let cards = self.attachments.cards(&self.address, activation, cx);
                    let fragments = self.fragment_runtime(cx);
                    self.viewport
                        .render(
                            palette,
                            peer,
                            viewport::HistoryControls {
                                pending_reads: self.reading.pending(),
                                read_motion: Some(Motion::from_preference(
                                    Preferences::animations_enabled(cx),
                                )),
                                reply,
                                quick_reply,
                                reactions: self.reaction_controls(locale, cx),
                                quick_hidden: self.message_menu.is_open(),
                                conversation: Some(cx.entity().downgrade()),
                                protected: self
                                    .encryption
                                    .as_ref()
                                    .map(HistoryEncryption::layouts)
                                    .unwrap_or_default(),
                                quotes: self
                                    .encryption
                                    .as_ref()
                                    .map(|encryption| encryption.quotes.presentations())
                                    .unwrap_or_default(),
                                edit_protected: self.sending.as_ref().is_some_and(|sending| {
                                    sending.view.read(cx).can_edit_protected()
                                }),
                                key_device: self.encryption.as_ref().map(|encryption| {
                                    encryption.activation.session().device_id.to_string()
                                }),
                                key_actions: self
                                    .encryption
                                    .as_ref()
                                    .map(|encryption| {
                                        encryption
                                            .key_actions
                                            .iter()
                                            .map(|(id, record)| (*id, record.state))
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                key_permissions: self
                                    .encryption
                                    .as_ref()
                                    .map(|encryption| {
                                        encryption
                                            .key_permissions
                                            .iter()
                                            .filter_map(|(id, entry)| {
                                                Some((*id, entry.permission?))
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                jitsi: self.calls.settings().cloned(),
                                jitsi_location: if self.heading.direct {
                                    self.heading.chat.clone()
                                } else {
                                    self.heading
                                        .topic
                                        .as_ref()
                                        .map(|topic| format!("# {topic}"))
                                        .or_else(|| self.heading.chat.clone())
                                },
                                fragments,
                                mention_names: self
                                    .details
                                    .as_ref()
                                    .map(Panel::mention_names)
                                    .unwrap_or_default(),
                                locale,
                            },
                            images,
                            Some(cards),
                        )
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
                        .children(background.map(|background| {
                            div()
                                .absolute()
                                .inset_0()
                                .overflow_hidden()
                                .debug_selector(|| "conversation-background".into())
                                .child(
                                    img(background.source())
                                        .size_full()
                                        .object_fit(ObjectFit::Cover),
                                )
                        }))
                        .child(self.history_frame(palette, viewport, cx))
                        .when(background_loading, |history| {
                            let motion =
                                Motion::from_preference(Preferences::animations_enabled(cx));
                            history.child(Self::render_background_loading(palette, locale, motion))
                        })
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
                .children(self.render_message_delete(palette, locale, cx))
                .children(self.render_attachment_copy_error(palette, locale, cx))
                .children(self.render_history_export(palette, locale, cx))
                .children(
                    self.sending
                        .as_ref()
                        .map(|sending| self.render_chat_footer(sending, palette, cx)),
                )
                .children(self.is_archived().then(|| {
                    ConversationNotice {
                        text: locale
                            .tr("desktop.sidebar.newChats.archivedConversation")
                            .into(),
                        action: None,
                    }
                    .render(palette, |_, _| {})
                    .debug_selector(|| "conversation-archive-notice".into())
                }))
            }
        }
    }
}

impl Render for Conversation {
    /// Рисует историю и редактор в рабочей области разговора.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        self.observe_read_activation(window, cx);
        self.restore_details_for_verified_history(window, cx);
        if let Some(sending) = &self.sending {
            let editor = sending.view.read(cx);
            if !self.local_composer_visible() && editor.editor_focus(cx).is_focused(window) {
                let focus = if self.has_access() {
                    &self.history_focus
                } else {
                    &self.screen_focus
                };
                window.focus(focus);
            }
        }
        let menu = self.render_message_menu(window, palette, cx);
        let time_reference = self.render_reference_time(window, palette, cx);
        if let Some(restore) = self.attachments.take_restore() {
            let focus = match &restore {
                PhotoReturn::Gallery(focus) => focus,
                PhotoReturn::History(key, focus) if self.viewport.has_visible_attachment(*key) => {
                    focus
                }
                PhotoReturn::Protected(row, focus) if self.viewport.has_visible_message(*row) => {
                    focus
                }
                _ if self.has_access() => &self.history_focus,
                _ => &self.screen_focus,
            };
            window.focus(focus);
        }
        let language = Preferences::read_for(cx, &self.address.route.owner).language;
        let photo = self.attachments.overlay(window, palette, language, cx);
        div()
            .id("conversation-layout")
            .relative()
            .size_full()
            .flex()
            .min_h_0()
            .min_w_0()
            .track_focus(&self.screen_focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if view.attachments.is_open() {
                    cx.stop_propagation();
                } else {
                    view.message_menu_hotkey(event, window, cx);
                    view.reply_hotkey(event, window, cx);
                }
            }))
            .child(self.render_chat(palette, cx))
            .children(menu.into_iter().flatten())
            .children(time_reference.into_iter().flatten())
            .children(photo)
    }
}

/// Проверяет, относится ли событие темы к показанной области истории.
fn includes(current: HistoryScope, changed: HistoryScope) -> bool {
    current.stream_id == changed.stream_id
        && (current.topic_id.is_none() || current.topic_id == changed.topic_id)
}

/// Учитывает снимок чтения для всего стрима при открытии его отдельной темы.
fn overlaps_read_scope(current: HistoryScope, changed: HistoryScope) -> bool {
    includes(current, changed) || includes(changed, current)
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
