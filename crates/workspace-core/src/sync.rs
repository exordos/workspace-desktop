//! Связывает получение событий с локальной базой.
//! Проверяет сессию перед записью и публикует изменения для оболочки после записи.

use std::{cmp::Ordering, collections::HashSet, path::PathBuf, sync::Arc};

use thiserror::Error;
use tokio::sync::{broadcast, watch};
use uuid::Uuid;

mod fragment_origins;
mod quote_origins;
pub(crate) use fragment_origins::FragmentOriginsPermit;
use fragment_origins::{CommittedOrigins, FragmentOrigins, OriginsPermit};
pub(crate) use quote_origins::QuoteOriginPermit;
use quote_origins::{QuoteOrigin, QuoteOrigins};

use crate::{
    auth::{Auth, AuthError},
    context::{OwnerContext, SessionGeneration},
    directory::AvatarLoads,
    messenger::{
        details::{AvatarCache, SidebarCache, SidebarChange},
        encryption::fragments::has_protected_draft,
        files::{MediaLoads, OpenMediaError},
        quick_reactions::{self, QuickReactionUsage},
        quote::QuoteSourceFence,
        send::PlainSendTarget,
    },
    notification_target::{NotificationTarget, NotificationTargetError},
    storage::{
        AvatarEvent, ConfirmedEdit, Cursor, DeliveryStatus, DraftAccess, DraftOpening, DraftScope,
        DraftVersion, DraftWriteOutcome, EncryptedEditVersion, EventWrite, FolderChange,
        HistoryChange, HistoryMessage, HistoryMetadata, HistoryPageEdges, HistoryPageGuard,
        HistoryPageOutcome, HistoryPageTarget, HistoryScope, HistoryWindow, KeyRequestCursor,
        KeyRequestPage, KeyResponseCursor, KeyResponsePage, KeyServicePayload, Message,
        MessageBody, MessagePlacement, OutboxEntry, PlaintextDraft, PreparedSend, PreparedSendBody,
        Storage, StorageError,
        media::{
            MediaCache, MediaCacheError, MediaCachePolicy, MediaClearReport, MediaCopyPage,
            MediaCopyQuery, MediaEstimate, MediaFilter,
        },
    },
};

pub use crate::storage::ApplyOutcome;

/// Версия серверного файла, которую удерживает чтение или декодированное превью.
pub(crate) type ProtectedFilePermit = OriginsPermit<Uuid>;

pub(crate) const MAX_MESSAGE_CHARACTERS: usize = 40_000;

/// Вычисляет UUID размещения из темы и строкового канонического UUID, как Workspace API.
fn public_send_id(topic_id: uuid::Uuid, canonical_id: uuid::Uuid) -> uuid::Uuid {
    uuid::Uuid::new_v5(&topic_id, canonical_id.to_string().as_bytes())
}

#[cfg(test)]
mod send_identity_tests {
    /// Сверяет формулу с контрольным примером Workspace UI.
    #[test]
    fn public_id_matches_workspace_reference() {
        let topic = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let canonical = "00000000-0000-4000-8000-000000000002".parse().unwrap();
        assert_eq!(
            super::public_send_id(topic, canonical).to_string(),
            "d2919e10-8c62-5b88-89e6-ed7da20fc912"
        );
    }
}

/// Изменение, которое оболочка показывает для одного аккаунта и проекта.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncUpdate {
    pub context: OwnerContext, // Аккаунт и проект, получившие изменение.
    pub generation: SessionGeneration, // Вход, для которого изменение ещё актуально.
    pub change: SyncChange,    // Новое значение счётчика или цель уведомления.
}

/// Изменения после записи в локальную базу.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncChange {
    DraftPurgePending, // Локальный черновик закрыт, осталось удалить прежнюю серверную версию.
    Unread(u32),       // Готовое число непрочитанных в папке «Все чаты».
    MessengerReset,    // Кэш очищен. Счётчик неизвестен до снимка с сервера.
    HistoryMessageCreated {
        scope: HistoryScope,    // Стрим и тема сохранённого сообщения.
        message_id: uuid::Uuid, // UUID новой строки в базе.
    },
    HistoryMessageChanged {
        message_id: Uuid, // UUID исправленной или удалённой строки.
    },
    HistoryMessageReactionsChanged {
        message_id: Uuid, // UUID строки с изменившимися реакциями.
    },
    QuickReactionUsageChanged, // Подтверждённое добавление обновило общий список быстрых реакций.
    QuoteOriginChanged {
        message_id: Uuid, // UUID оригинала, чья проверенная версия перестала действовать.
    },
    KeyServiceMessage {
        scope: HistoryScope,         // Точные стрим и тема ответа на запрос ключа.
        message_id: Uuid,            // Публичный UUID для авторитетной проверки ответа.
        key_id: Uuid,                // Поколение ключа, о котором сообщило событие.
        recipient_device_id: String, // Устройство, которому адресован ответ.
    },
    HistoryAccessRevoked(HistoryRevocation), // Область, которую больше нельзя показывать.
    MediaFileInvalidated {
        file_id: Uuid, // Файл изменился или удалён, прежнее превью нужно убрать.
    },
    SidebarDetailsChanged {
        stream_id: Uuid, // Стрим, чья открытая панель должна получить обновление.
    },
    AvatarChanged {
        user_id: Uuid, // Пользователь, чей аватар нужно перечитать.
    },
    OutboxChanged {
        scope: HistoryScope, // Исходная тема, очередь которой нужно перечитать.
    },
    IncomingDirectMessage {
        conversation_id: String, // Идентификатор личного разговора на сервере.
        message_id: String,      // Идентификатор нового сообщения.
    },
}

/// Идентификатор области истории, удалённой по событию мессенджера.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryRevocation {
    Stream(Uuid), // UUID стрима, доступ к которому отозван.
    Topic {
        stream_id: Uuid, // Стрим, включая его окно без фильтра темы.
        topic_id: Uuid,  // Удалённая или отозванная тема.
    },
}

/// Сведения об аватаре пользователя из события справочника.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AvatarUpdate {
    Reference(Option<String>), // Подтверждённая ссылка или явное отсутствие изображения.
    Reconcile,                 // Ссылка неизвестна. Нужно сверить профиль.
}

/// Изменение, которое источник передаёт для сохранения.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    MessageCreated {
        cursor: Cursor,                      // Позиция, которую сохраняем вместе с сообщением.
        message: Message,                    // Новое сообщение для локальной базы.
        placement: Option<MessagePlacement>, // Стрим и тема из снимка.
        metadata: Option<HistoryMetadata>,   // Автор и время, если снимок пригоден для истории.
    },
    IncomingDirectMessage {
        cursor: Cursor,          // Позиция нового события после готовности WebSocket.
        message: Message,        // Входящее непрочитанное сообщение для локальной базы.
        conversation_id: String, // Проверенный идентификатор личного разговора.
        placement: Option<MessagePlacement>, // Стрим и тема из снимка.
        metadata: Option<HistoryMetadata>, // Автор и время, если снимок пригоден для истории.
    },
    HistoryChanged {
        cursor: Cursor,        // Позиция изменения в журнале.
        change: HistoryChange, // Правка, удаление или отзыв доступа.
    },
    OwnMessageReactionChanged {
        cursor: Cursor,   // Позиция события реакции в журнале.
        message_id: Uuid, // Публичный UUID размещения.
        emoji: String,    // Ключ эмодзи из ответа сервера.
        selected: bool,   // Есть ли реакция после события.
    },
    SidebarChanged {
        cursor: Cursor,        // Позиция в общем журнале Workspace.
        change: SidebarChange, // Изменение профиля, стрима, темы или состава.
    },
    UserUpdated {
        cursor: Cursor,                      // Позиция снимка пользователя в журнале.
        user_id: Uuid,                       // UUID пользователя, чей профиль изменился.
        avatar: AvatarUpdate,                // Подтверждённая ссылка или запрос сверки аватара.
        sidebar: Option<Box<SidebarChange>>, // Снимок правой панели, если его удалось прочитать.
    },
    FileDeleted {
        cursor: Cursor, // Позиция удаления в журнале Workspace.
        file_id: Uuid,  // UUID файла, на который мог ссылаться профиль.
    },
    FileUpdated {
        cursor: Cursor, // Позиция изменения метаданных в журнале Workspace.
        file_id: Uuid,  // UUID файла, сохранённые сведения о котором устарели.
    },
    UnreadChanged {
        cursor: Cursor, // Позиция события обновления папки в журнале.
        count: u32,     // Новый счётчик папки «Все чаты».
    },
    FolderChanged {
        cursor: Cursor,                      // Позиция изменения в журнале.
        change: FolderChange,                // Удаление или область, которую нужно сверить.
        sidebar: Option<Box<SidebarChange>>, // Сведения сайдбара из снимка стрима, если он полный.
    },
    CatalogCountsChanged {
        cursor: Cursor, // Позиция события со снимком счётчиков.
        counts: crate::messenger::catalog::CatalogCounts, // Абсолютные значения канала или темы.
        folder: Option<FolderChange>, // Проверка появления или восстановления канала из того же события.
        sidebar: Option<Box<SidebarChange>>, // Сведения сайдбара из того же серверного снимка.
    },
    UnreadBaseline {
        cursor: Cursor, // Позиция сервера перед чтением счётчика. Курсор сообщений не меняется.
        count: u32,     // Счётчик, загруженный отдельным запросом при подключении.
    },
    SourceReady(Cursor), // WebSocket готов, но отдельный снимок счётчика недоступен.
    Checkpoint(Cursor),  // Новая позиция чтения без сообщения.
}

impl Event {
    /// Возвращает позицию, до которой нужно продвинуть чтение после записи.
    pub fn cursor(&self) -> &Cursor {
        match self {
            Self::MessageCreated { cursor, .. }
            | Self::IncomingDirectMessage { cursor, .. }
            | Self::HistoryChanged { cursor, .. }
            | Self::OwnMessageReactionChanged { cursor, .. }
            | Self::SidebarChanged { cursor, .. }
            | Self::UserUpdated { cursor, .. }
            | Self::FileDeleted { cursor, .. }
            | Self::FileUpdated { cursor, .. }
            | Self::UnreadChanged { cursor, .. }
            | Self::FolderChanged { cursor, .. }
            | Self::CatalogCountsChanged { cursor, .. }
            | Self::UnreadBaseline { cursor, .. }
            | Self::SourceReady(cursor)
            | Self::Checkpoint(cursor) => cursor,
        }
    }

    /// Возвращает сообщение, если оно есть в событии.
    pub fn message(&self) -> Option<&Message> {
        match self {
            Self::MessageCreated { message, .. } | Self::IncomingDirectMessage { message, .. } => {
                Some(message)
            }
            _ => None,
        }
    }
}

/// Разрешение записать страницу, запрошенную в действующей сессии.
#[derive(Debug)]
pub struct HistoryRequest {
    owner: OwnerContext,           // Владелец, для которого запросили страницу.
    generation: SessionGeneration, // Поколение сессии на момент запроса.
    guard: HistoryPageGuard,       // Версия кэша и края окна перед запросом.
}

impl HistoryRequest {
    /// Возвращает маркер, если страница продолжает существующее окно.
    pub fn marker(&self) -> Option<&str> {
        self.guard.marker()
    }

    /// Запоминает точное серверное подтверждение для UUID со старой отметкой.
    pub(crate) fn verify_legacy_encrypted_message(&mut self, id: Uuid) {
        self.guard.verify_legacy_encrypted_message(id);
    }
}

/// Причина, по которой событие не удалось обработать.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Ошибка проверки действующей сессии.
    #[error(transparent)]
    Storage(#[from] StorageError), // Ошибка локального хранилища.
}

/// Причина отказа подготовки снимка отправки.
#[derive(Debug, Error)]
pub enum PrepareSendError {
    #[error(transparent)]
    Sync(#[from] SyncError), // Сессия недействительна или база недоступна.
    #[error("the generated message identifier is already in use")]
    IdCollision, // Случайный UUID уже занят: исходная запись осталась без изменений.
    #[error("a message must contain non-whitespace text up to 40000 Unicode code points")]
    InvalidText, // Пустой текст или превышение лимита Workspace.
    #[error("the forwarded message or its scope changed before queuing")]
    SourceChanged, // Исходная строка изменилась или отозван доступ к чату.
}

/// Проверяет право сессии на запись и передаёт события в хранилище.
pub struct Sync {
    pub(crate) avatar_cache: AvatarCache, // Временные файлы аватаров по владельцу и сессии.
    pub(crate) avatar_loads: AvatarLoads, // Общие запросы аватаров по UUID пользователя.
    pub(crate) sidebar_cache: SidebarCache, // Временные снимки панелей на время сессии.
    auth: Auth,                           // Авторизация для проверки владельца и поколения сессии.
    storage: Arc<Storage>,                // Общая локальная база сообщений, позиций и счётчиков.
    updates: broadcast::Sender<SyncUpdate>, // Канал изменений. Оформление окна получает их после записи.
    fragment_origins: FragmentOrigins,      // Версии доступа исходных чатов открытых фрагментов.
    quote_origins: QuoteOrigins,            // Проверенные версии исходников защищённых цитат.
    protected_files: CommittedOrigins<Uuid>, // Проверенные версии серверных файлов по владельцу.
    media_cache: Option<MediaCache>,        // Дисковые копии вложений, если профиль указал каталог.
    media_authority: Arc<()>,               // Отличает допуск этого экземпляра от другого профиля.
    media_updates: watch::Sender<()>,       // Сигнал повторно проверить незавершённое чтение.
    pub(crate) media_loads: MediaLoads,     // Одна работа на вложение и поколение сессии.
}

impl Sync {
    /// Создаёт синхронизацию с указанной авторизацией и базой.
    pub fn new(auth: Auth, storage: Arc<Storage>) -> Self {
        Self {
            auth,
            storage,
            sidebar_cache: SidebarCache::default(),
            avatar_cache: AvatarCache::default(),
            avatar_loads: AvatarLoads::default(),
            updates: broadcast::channel(128).0,
            fragment_origins: FragmentOrigins::default(),
            quote_origins: QuoteOrigins::default(),
            protected_files: CommittedOrigins::default(),
            media_cache: None,
            media_authority: Arc::new(()),
            media_updates: watch::channel(()).0,
            media_loads: MediaLoads::default(),
        }
    }

    /// Открывает кэш вложений профиля рядом с переданной базой.
    pub fn with_media_cache(
        auth: Auth,
        storage: Arc<Storage>,
        root: PathBuf,
        policy: MediaCachePolicy,
    ) -> Result<Self, MediaCacheError> {
        let cache = MediaCache::new(storage.clone(), root, policy)?;
        Ok(Self {
            media_cache: Some(cache),
            ..Self::new(auth, storage)
        })
    }

    /// Даёт файловому модулю доступ к копиям того же профиля.
    pub(crate) fn media_cache(&self) -> Option<&MediaCache> {
        self.media_cache.as_ref()
    }

    /// Возвращает идентичность профиля для непрозрачного допуска к медиа.
    pub(crate) fn media_authority(&self) -> &Arc<()> {
        &self.media_authority
    }

    /// Даёт выполняющимся загрузкам сигнал перечитать границы доступа после фиксации.
    pub(crate) fn media_changes(&self) -> watch::Receiver<()> {
        self.media_updates.subscribe()
    }

    /// Уведомляет загрузки об изменении проекции и запускает фоновую очистку.
    pub(crate) fn note_media_change(&self) {
        self.media_updates.send_replace(());
    }

    /// Убирает отозванные байты и повторяет вытеснение после закрытия читателей.
    pub(crate) async fn run_media_cleanup(&self) {
        let Some(cache) = self.media_cache() else {
            return std::future::pending::<()>().await;
        };
        let mut changes = self.media_changes();
        let mut retry = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tokio::select! {
                delta = changes.changed() => {
                    if delta.is_err() {
                        return;
                    }
                }
                _ = retry.tick() => {}
            }
            let cache = cache.clone();
            let removed = tokio::task::spawn_blocking(move || cache.enforce_budget()).await;
            if let Ok(Err(error)) = removed {
                log::warn!("Не удалось убрать отозванную копию вложения: {error}");
            }
        }
    }

    /// Возвращает страницу локальных копий только для действующего входа.
    pub fn list_media_copies(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        query: MediaCopyQuery,
    ) -> Result<MediaCopyPage, OpenMediaError> {
        let cache = self.media_cache().ok_or(OpenMediaError::Unconfigured)?;
        Ok(self
            .auth
            .with_active_generation(owner, generation, || cache.list(owner, query))??)
    }

    /// Подсчитывает место по выбранным копиям действующего входа.
    pub fn estimate_media(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        filter: &MediaFilter,
    ) -> Result<MediaEstimate, OpenMediaError> {
        let cache = self.media_cache().ok_or(OpenMediaError::Unconfigured)?;
        Ok(self
            .auth
            .with_active_generation(owner, generation, || cache.estimate(owner, filter))??)
    }

    /// Сразу отзывает выдачу выбранных копий, затем пытается освободить их место.
    pub fn clear_media(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        filter: &MediaFilter,
    ) -> Result<MediaClearReport, OpenMediaError> {
        let cache = self.media_cache().ok_or(OpenMediaError::Unconfigured)?;
        let candidates = self
            .auth
            .with_active_generation(owner, generation, || cache.begin_clear(owner, filter))??;
        self.note_media_change();
        let report = cache.finish_clear(candidates)?;
        let operation = Uuid::new_v4();
        log::debug!(
            "Очистка кэша вложений: op_id={operation} freed_bytes={} pending_bytes={}",
            report.freed_bytes,
            report.pending_bytes
        );
        Ok(report)
    }

    /// Даёт фоновой задаче доступ к авторизации и состояниям сессий.
    pub(crate) fn auth(&self) -> &Auth {
        &self.auth
    }

    /// Даёт фоновым обработчикам тот же профиль, в который пишутся события.
    pub(crate) fn storage(&self) -> &Arc<Storage> {
        &self.storage
    }

    /// Возвращает последнюю записанную позицию источника для владельца.
    pub fn cursor(&self, owner: &OwnerContext, source: &str) -> Result<Option<Cursor>, SyncError> {
        Ok(self.storage.cursor(owner, source)?)
    }

    /// Возвращает сохранённый счётчик. `None` означает, что сервер его ещё не прислал.
    pub fn unread(&self, owner: &OwnerContext) -> Result<Option<u32>, SyncError> {
        Ok(self.storage.unread(owner)?)
    }

    /// Загружает историю популярности для действующего входа.
    pub fn quick_reaction_usage(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<QuickReactionUsage, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            let first_day = quick_reactions::first_day(quick_reactions::utc_day());
            Ok(self.storage.quick_reaction_usage(owner, first_day)?)
        })?
    }

    /// Учитывает подтверждённое добавление и обновляет общий список у открытых разговоров.
    pub(crate) fn record_quick_reaction(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        emoji: &str,
    ) -> Result<(), SyncError> {
        let today = quick_reactions::utc_day();
        self.auth.with_active_generation(owner, generation, || {
            self.storage.record_quick_reaction(
                owner,
                quick_reactions::first_day(today),
                today,
                &quick_reactions::canonical_reaction_name(emoji),
            )?;
            self.publish(owner, generation, SyncChange::QuickReactionUsageChanged);
            Ok(())
        })?
    }

    /// Сбрасывает данные источника, пока поколение сессии имеет право на запись.
    pub(crate) fn reset_source(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        source: &str,
    ) -> Result<(), SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            if source == crate::messenger::SOURCE_ID {
                self.quote_origins.commit_matching(
                    owner,
                    || {
                        self.fragment_origins.commit(
                            owner,
                            None,
                            || self.storage.reset_source(owner, source),
                            |_| true,
                        )
                    },
                    |_, _, _| true,
                )?;
                self.sidebar_cache
                    .invalidate(owner, generation, None, false);
                self.note_media_change();
                self.publish(owner, generation, SyncChange::MessengerReset);
            } else {
                self.storage.reset_source(owner, source)?;
            }
            Ok(())
        })?
    }

    /// Снимает версии доступа до авторитетной проверки исходных чатов.
    pub(crate) fn prepare_fragment_origins(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        streams: &[Uuid],
    ) -> Result<FragmentOriginsPermit, AuthError> {
        self.auth.with_active_generation(owner, generation, || {
            self.fragment_origins.capture(owner, streams)
        })
    }

    /// Запоминает оригинал после проверки в той же блокировке авторизации, что и запись истории.
    pub(crate) fn capture_quote_origin(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        original: &HistoryMessage,
    ) -> Option<QuoteOriginPermit> {
        let (permit, revoked) =
            self.quote_origins
                .capture(owner, generation, QuoteOrigin::from_message(original));
        if revoked {
            self.note_media_change();
            self.publish(
                owner,
                generation,
                SyncChange::QuoteOriginChanged {
                    message_id: original.id,
                },
            );
        }
        permit
    }

    /// Закрепляет проверенную версию файла под той же блокировкой Auth, что и запись событий.
    pub(crate) fn capture_protected_file(
        &self,
        owner: &OwnerContext,
        file_id: Uuid,
    ) -> ProtectedFilePermit {
        self.protected_files.capture(owner, &[file_id])
    }

    /// Фиксирует условия записи до сетевого запроса. Доступ к стриму или теме проверяет вызывающий модуль.
    pub fn prepare_history_page(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        target: HistoryPageTarget,
    ) -> Result<Option<HistoryRequest>, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            Ok(self
                .storage
                .prepare_history_page(owner, scope, target)?
                .map(|guard| HistoryRequest {
                    owner: owner.clone(),
                    generation,
                    guard,
                }))
        })?
    }

    /// Применяет страницу той же сессии, которая начала запрос, не двигая курсор событий.
    pub fn save_history_page(
        &self,
        request: HistoryRequest,
        messages: &[HistoryMessage],
        edges: HistoryPageEdges,
    ) -> Result<HistoryPageOutcome, SyncError> {
        self.auth
            .with_active_generation(&request.owner, request.generation, || {
                let mut changed = HashSet::new();
                let saved = self.quote_origins.commit_matching(
                    &request.owner,
                    || {
                        self.storage.save_history_page(
                            &request.owner,
                            request.guard,
                            messages,
                            edges,
                        )
                    },
                    |origin, conflicted, saved| {
                        if matches!(saved, HistoryPageOutcome::Saved(_))
                            && messages.iter().any(|message| {
                                if message.id != origin.id {
                                    return false;
                                }
                                let snapshot = QuoteOrigin::from_message(message);
                                // При неопределённом порядке сохранённая история разрешает спор.
                                // Снимок с более ранней правкой известную версию не заменяет.
                                snapshot.compare_time(origin) != Some(Ordering::Less)
                                    && (conflicted || snapshot != *origin)
                            })
                        {
                            changed.insert(origin.id);
                            true
                        } else {
                            false
                        }
                    },
                )?;
                if !changed.is_empty() {
                    self.note_media_change();
                }
                for message_id in changed {
                    self.publish(
                        &request.owner,
                        request.generation,
                        SyncChange::HistoryMessageChanged { message_id },
                    );
                }
                Ok(saved)
            })?
    }

    /// Читает окна активной сессии. Вызывающий модуль заранее подтверждает доступ к области.
    pub fn history_windows(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<Vec<HistoryWindow>, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            Ok(self.storage.history_windows(owner, scope)?)
        })?
    }

    /// Перечитывает подтверждённое окно того же входа без запроса к серверу.
    pub fn refresh_history_window(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        id: i64,
    ) -> Result<Option<HistoryWindow>, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            Ok(self
                .storage
                .history_window(owner, scope, id)?
                .filter(|window| !window.needs_refresh))
        })?
    }

    /// Применяет подтверждение реакции для того же размещения и поколения сессии.
    pub(crate) fn confirm_message_reaction(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        target: crate::messenger::message_actions::MessageTarget,
        emoji: &str,
        selected: bool,
    ) -> Result<(), SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            if self.storage.confirm_message_reaction(
                owner,
                target.message_id,
                target.scope,
                target.topic_id,
                emoji,
                selected,
            )? {
                self.publish(
                    owner,
                    generation,
                    SyncChange::HistoryMessageReactionsChanged {
                        message_id: target.message_id,
                    },
                );
            }
            Ok(())
        })?
    }

    /// Закрепляет действующую версию строки, которую показал редактор, перед PUT.
    pub(crate) fn prepare_encrypted_edit(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        expected: &HistoryMessage,
    ) -> Result<Option<EncryptedEditVersion>, SyncError> {
        Ok(self.auth.with_active_generation(owner, generation, || {
            self.storage.prepare_encrypted_edit(owner, expected)
        })??)
    }

    /// Публикует подтверждённую правку точной строки без продвижения курсора журнала.
    pub(crate) fn confirm_encrypted_edit(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        expected: &HistoryMessage,
        version: EncryptedEditVersion,
        confirmed: &HistoryMessage,
    ) -> Result<bool, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            let outcome = self.quote_origins.commit_matching(
                owner,
                || {
                    self.storage
                        .confirm_encrypted_edit(owner, expected, version, confirmed)
                },
                |origin, _, outcome| {
                    *outcome == ConfirmedEdit::Applied && origin.id == confirmed.id
                },
            )?;
            if outcome == ConfirmedEdit::Applied {
                self.note_media_change();
                self.publish(
                    owner,
                    generation,
                    SyncChange::HistoryMessageChanged {
                        message_id: confirmed.id,
                    },
                );
            }
            Ok(outcome != ConfirmedEdit::Stale)
        })?
    }

    /// Сверяет конверт восстановления. Вызывать под блокировкой того же поколения Auth.
    pub(crate) fn matches_key_recovery_source(
        &self,
        owner: &OwnerContext,
        expected: &HistoryMessage,
    ) -> Result<bool, StorageError> {
        self.storage.matches_encrypted_edit(owner, expected)
    }

    /// Находит сохранённые ответы текущего устройства вне открытого окна истории.
    pub fn key_service_responses(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        recipient_device_id: Uuid,
        after: Option<KeyResponseCursor>,
    ) -> Result<KeyResponsePage, SyncError> {
        Ok(self.auth.with_active_generation(owner, generation, || {
            self.storage
                .key_service_responses(owner, recipient_device_id, after)
        })??)
    }

    /// Читает кандидатов для выдачи из сохранённых тем этого стрима и действующего входа.
    /// Право на каждую выдачу проверяет `KeyExchange` по текущим данным сервера.
    pub fn key_service_requests(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        after: Option<KeyRequestCursor>,
    ) -> Result<KeyRequestPage, SyncError> {
        Ok(self.auth.with_active_generation(owner, generation, || {
            self.storage.key_service_requests(owner, stream_id, after)
        })??)
    }

    /// Принимает ответ отправки только для действующего входа и подходящего снимка.
    pub fn confirm_http_send(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        message: &HistoryMessage,
    ) -> Result<bool, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            let confirmed = self.storage.confirm_http_send(owner, message)?;
            if confirmed {
                let scope = HistoryScope {
                    stream_id: message.stream_id,
                    topic_id: Some(message.topic_id),
                };
                self.publish(
                    owner,
                    generation,
                    SyncChange::HistoryMessageCreated {
                        scope,
                        message_id: message.id,
                    },
                );
                self.publish(owner, generation, SyncChange::OutboxChanged { scope });
            }
            Ok(confirmed)
        })?
    }

    /// Читает статус ожидающих сообщений в области истории действующей сессии.
    pub fn outbox(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<Vec<OutboxEntry>, SyncError> {
        self.read_outbox(owner, generation, Some(scope))
    }

    /// Читает очередь указанного входа для фоновой обработки или экрана.
    pub(crate) fn read_outbox(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: Option<HistoryScope>,
    ) -> Result<Vec<OutboxEntry>, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            Ok(self.storage.outbox(owner, scope)?)
        })?
    }

    /// Отмечает незавершённые запросы после запуска фоновой задачи.
    pub(crate) fn recover_sends(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<(), SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            for scope in self.storage.recover_interrupted_sends(owner)? {
                self.publish(owner, generation, SyncChange::OutboxChanged { scope });
            }
            Ok(())
        })?
    }

    /// Разрешает первый POST только после записи захвата в действующей сессии.
    pub(crate) fn claim_send(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        canonical_id: Uuid,
    ) -> Result<Option<PreparedSend>, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            let claimed = self.storage.claim_first_send(owner, canonical_id)?;
            if let Some(snapshot) = &claimed {
                self.publish_send(owner, generation, snapshot);
            }
            Ok(claimed)
        })?
    }

    /// Фиксирует итог активной попытки или предварительный отказ для новой записи.
    pub(crate) fn set_send_status(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        snapshot: &PreparedSend,
        claimed: bool,
        status: DeliveryStatus,
    ) -> Result<(), SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            if self
                .storage
                .set_send_status(owner, snapshot.canonical_id, claimed, status)?
            {
                self.publish_send(owner, generation, snapshot);
            }
            Ok(())
        })?
    }

    /// Захватывает ревизию доступа до запроса обычной темы.
    pub fn prepare_draft_opening(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
    ) -> Result<DraftOpening, SyncError> {
        Ok(self.auth.with_active_generation(owner, generation, || {
            self.storage.prepare_draft_opening(owner, stream_id)
        })??)
    }

    /// Открывает черновик после сетевой проверки точной темы и разрешения обычного текста.
    pub fn open_draft(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        opening: DraftOpening,
        scope: DraftScope,
    ) -> Result<(DraftAccess, Option<PlaintextDraft>), SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            let access = self.storage.finish_draft_opening(owner, opening, scope)?;
            let draft = self.storage.read_plaintext_draft(&access, None)?;
            Ok((access, draft))
        })?
    }

    /// Пишет ввод в ранее подтверждённую тему, включая временную потерю сети.
    pub fn save_draft(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        access: &DraftAccess,
        draft: &PlaintextDraft,
    ) -> Result<DraftWriteOutcome, SyncError> {
        if has_protected_draft(&draft.markdown)
            || draft.replies.as_ref().is_some_and(|replies| {
                replies.tabs.iter().any(|tab| {
                    has_protected_draft(&tab.answer)
                        || tab.fragment.as_deref().is_some_and(has_protected_draft)
                })
            })
        {
            return Err(StorageError::InvalidDraftRecord.into());
        }
        self.save_draft_text(owner, generation, access, draft)
    }

    /// Сохраняет буквальные маркеры только для отдельно подтверждённого внешнего Markdown.
    pub fn save_external_draft(
        &self,
        target: &PlainSendTarget,
        access: &DraftAccess,
        draft: &PlaintextDraft,
    ) -> Result<DraftWriteOutcome, SyncError> {
        let (owner, generation, scope) = target.address();
        if !target.literal_markers_allowed() || access.scope() != scope {
            return Err(StorageError::StaleDraftAccess.into());
        }
        self.save_draft_text(owner, generation, access, draft)
    }

    /// Записывает уже классифицированный текст в область действующего входа.
    fn save_draft_text(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        access: &DraftAccess,
        draft: &PlaintextDraft,
    ) -> Result<DraftWriteOutcome, SyncError> {
        if !access.belongs_to(owner) {
            return Err(StorageError::StaleDraftAccess.into());
        }
        Ok(self
            .auth
            .with_confirmed_generation(owner, generation, || {
                self.storage.save_plaintext_draft(access, draft)
            })??)
    }

    /// Отзывает обычную запись версии, посылая в хранилище только её адрес.
    pub fn protect_draft(
        &self,
        target: &PlainSendTarget,
        access: &DraftAccess,
        version: DraftVersion,
    ) -> Result<bool, SyncError> {
        let (owner, generation, scope) = target.address();
        if !target.fragments_allowed() || !access.belongs_to(owner) || access.scope() != scope {
            return Err(StorageError::StaleDraftAccess.into());
        }
        let recorded = self
            .auth
            .with_confirmed_generation(owner, generation, || {
                self.storage.protect_plaintext_draft(access, version)
            })??;
        if recorded {
            let _ = self.updates.send(SyncUpdate {
                context: owner.clone(),
                generation,
                change: SyncChange::DraftPurgePending,
            });
        }
        Ok(recorded)
    }

    /// Создаёт снимок отправки и возвращает его только после записи для действующего входа.
    pub fn prepare_send(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: uuid::Uuid,
        topic_id: uuid::Uuid,
        markdown: String,
    ) -> Result<PreparedSend, PrepareSendError> {
        self.prepare_send_with_draft(owner, generation, stream_id, topic_id, markdown, None)
    }

    /// Готовит очередь вместе с передачей ответственности за конкретную версию черновика.
    pub fn prepare_send_with_draft(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        topic_id: Uuid,
        markdown: String,
        draft: Option<(&DraftAccess, DraftVersion)>,
    ) -> Result<PreparedSend, PrepareSendError> {
        if markdown.trim().is_empty() || markdown.chars().count() > MAX_MESSAGE_CHARACTERS {
            return Err(PrepareSendError::InvalidText);
        }
        self.prepare_send_body(
            owner,
            generation,
            stream_id,
            topic_id,
            PreparedSendBody::Markdown(markdown),
            draft,
        )
    }

    /// Фиксирует уже подготовленное содержимое и выдаёт устойчивые UUID отправки.
    pub(crate) fn prepare_send_body(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        topic_id: Uuid,
        body: PreparedSendBody,
        draft: Option<(&DraftAccess, DraftVersion)>,
    ) -> Result<PreparedSend, PrepareSendError> {
        if matches!(&body, PreparedSendBody::Markdown(text) if has_protected_draft(text)) {
            return Err(PrepareSendError::InvalidText);
        }
        self.save_prepared_body(
            owner,
            generation,
            DraftScope {
                stream_id,
                topic_id,
            },
            body,
            draft,
            None,
        )
    }

    /// Принимает Markdown внешнего провайдера с буквальными маркерами в подтверждённой теме.
    pub fn prepare_external_send(
        &self,
        target: &PlainSendTarget,
        markdown: String,
        draft: Option<(&DraftAccess, DraftVersion)>,
    ) -> Result<PreparedSend, PrepareSendError> {
        if !target.literal_markers_allowed()
            || markdown.trim().is_empty()
            || markdown.chars().count() > MAX_MESSAGE_CHARACTERS
        {
            return Err(PrepareSendError::InvalidText);
        }
        let (owner, generation, scope) = target.address();
        self.save_prepared_body(
            owner,
            generation,
            scope,
            PreparedSendBody::Markdown(markdown),
            draft,
            None,
        )
    }

    /// Сохраняет готовую пересылку, пока оригинал и наблюдаемые чаты ещё доступны.
    pub(crate) fn prepare_forward(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: DraftScope,
        markdown: String,
        origin: (&HistoryMessage, QuoteSourceFence),
        access: &FragmentOriginsPermit,
    ) -> Result<PreparedSend, PrepareSendError> {
        self.save_prepared_body(
            owner,
            generation,
            scope,
            PreparedSendBody::Markdown(markdown),
            None,
            Some((origin, access)),
        )
    }

    /// Фиксирует уже проверенное тело и точный адрес очереди.
    fn save_prepared_body(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: DraftScope,
        body: PreparedSendBody,
        draft: Option<(&DraftAccess, DraftVersion)>,
        forward: Option<((&HistoryMessage, QuoteSourceFence), &FragmentOriginsPermit)>,
    ) -> Result<PreparedSend, PrepareSendError> {
        let DraftScope {
            stream_id,
            topic_id,
        } = scope;
        self.auth
            .with_active_generation(owner, generation, || {
                let save = || {
                    let canonical_id = uuid::Uuid::new_v4();
                    let message = PreparedSend {
                        canonical_id,
                        public_id: public_send_id(topic_id, canonical_id),
                        stream_id,
                        topic_id,
                        body,
                    };
                    if !self
                        .storage
                        .save_prepared_send_with_draft(owner, &message, draft)
                        .map_err(SyncError::from)?
                    {
                        return Err(PrepareSendError::IdCollision);
                    }
                    self.publish_send(owner, generation, &message);
                    Ok(message)
                };
                match forward {
                    Some(((origin, fence), access)) => access
                        .with_current(|| {
                            if !fence.scope_still_accessible(&self.storage, owner, origin)?
                                || !self
                                    .storage
                                    .quote_source_matches(owner, origin)
                                    .map_err(SyncError::from)?
                            {
                                return Err(PrepareSendError::SourceChanged);
                            }
                            save()
                        })
                        .ok_or(PrepareSendError::SourceChanged)?,
                    None => save(),
                }
            })
            .map_err(SyncError::from)?
    }

    /// Повторяет защищённую отправку с тем же конвертом и адресом после явного действия.
    pub fn retry_encrypted_send(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        canonical_id: Uuid,
    ) -> Result<bool, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            let snapshot = self.storage.retry_encrypted_send(owner, canonical_id)?;
            if let Some(snapshot) = &snapshot {
                self.publish_send(owner, generation, snapshot);
            }
            Ok(snapshot.is_some())
        })?
    }

    /// Подписывает оболочку на изменения. После пропуска событий счётчик можно перечитать.
    pub fn subscribe(&self) -> broadcast::Receiver<SyncUpdate> {
        self.updates.subscribe()
    }

    /// Проверяет цель клика в действующей сессии и возвращает подтверждённый адрес сообщения.
    pub fn resolve_notification_target(
        &self,
        target: NotificationTarget,
        generation: SessionGeneration,
    ) -> Result<NotificationTarget, NotificationTargetError> {
        let exists = self
            .auth
            .with_active_generation(&target.context, generation, || {
                self.storage.contains_notification_target(&target)
            })??;
        if exists {
            Ok(target)
        } else {
            Err(NotificationTargetError::UnknownTarget)
        }
    }

    /// Записывает событие, пока поколение сессии остаётся действующим.
    /// Проверка сессии и запись защищены одной блокировкой аккаунта.
    pub fn apply(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        source: &str,
        event: Event,
    ) -> Result<ApplyOutcome, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            if matches!(event, Event::SourceReady(_)) {
                return Ok(ApplyOutcome::Applied);
            }
            if let Event::UnreadBaseline { cursor, count } = &event {
                if self.storage.apply_unread_baseline(owner, cursor, *count)? {
                    self.publish(owner, generation, SyncChange::Unread(*count));
                }
                return Ok(ApplyOutcome::Applied);
            }

            let unread = match &event {
                Event::UnreadChanged { count, .. } => Some(*count),
                _ => None,
            };
            let conversation = match &event {
                Event::IncomingDirectMessage {
                    conversation_id,
                    message,
                    ..
                } if message.body.markdown().is_some() => Some(conversation_id.as_str()),
                _ => None,
            };
            let placement = match &event {
                Event::MessageCreated { placement, .. }
                | Event::IncomingDirectMessage { placement, .. } => placement.as_ref(),
                _ => None,
            };
            let metadata = match &event {
                Event::MessageCreated { metadata, .. }
                | Event::IncomingDirectMessage { metadata, .. } => metadata.as_ref(),
                _ => None,
            };
            let history = match &event {
                Event::HistoryChanged { change, .. } => Some(change),
                _ => None,
            };
            let own_reaction = match &event {
                Event::OwnMessageReactionChanged {
                    message_id,
                    emoji,
                    selected,
                    ..
                } => Some((*message_id, emoji.as_str(), *selected)),
                _ => None,
            };
            let folder = match &event {
                Event::FolderChanged { change, .. } => Some(*change),
                Event::CatalogCountsChanged { folder, .. } => *folder,
                Event::UnreadChanged { .. } => Some(FolderChange::Refresh(Some(Uuid::nil()))),
                _ => None,
            };
            let catalog_counts = match &event {
                Event::CatalogCountsChanged { counts, .. } => Some(counts),
                _ => None,
            };
            let avatar = match (&event, source) {
                (
                    Event::UserUpdated {
                        user_id, avatar, ..
                    },
                    crate::messenger::SOURCE_ID,
                ) => Some(match avatar {
                    AvatarUpdate::Reference(reference) => AvatarEvent::Snapshot {
                        user_id: *user_id,
                        reference: reference.as_deref(),
                    },
                    AvatarUpdate::Reconcile => AvatarEvent::Reconcile { user_id: *user_id },
                }),
                (Event::FileDeleted { file_id, .. }, crate::messenger::SOURCE_ID) => {
                    Some(AvatarEvent::FileDeleted { file_id: *file_id })
                }
                _ => None,
            };
            let media_invalidated = match (&event, source) {
                (
                    Event::FileDeleted { file_id, .. } | Event::FileUpdated { file_id, .. },
                    crate::messenger::SOURCE_ID,
                ) => Some(*file_id),
                _ => None,
            };
            let write = || {
                self.storage.apply(
                    owner,
                    source,
                    event.cursor(),
                    EventWrite {
                        message: event.message(),
                        unread,
                        notification_conversation: conversation,
                        placement,
                        metadata,
                        history,
                        own_reaction,
                        folder,
                        catalog_counts,
                        avatar,
                        media_invalidated,
                        checkpoint: matches!(event, Event::Checkpoint(_)),
                    },
                )
            };
            let revocation = match history {
                Some(HistoryChange::RemoveStream(id)) if source == crate::messenger::SOURCE_ID => {
                    id.parse().ok()
                }
                _ => None,
            };
            let saved = if let Some(file_id) = media_invalidated {
                self.protected_files
                    .commit_matching(owner, write, |candidate, saved| {
                        *candidate == file_id && saved.outcome == ApplyOutcome::Applied
                    })?
            } else if let Some(change) = history.filter(|_| source == crate::messenger::SOURCE_ID) {
                self.quote_origins.commit_matching(
                    owner,
                    || {
                        if let Some(stream) = revocation {
                            self.fragment_origins
                                .commit(owner, Some(stream), write, |saved| saved.history_changed)
                        } else {
                            write()
                        }
                    },
                    |origin, _, saved| saved.history_changed && origin.affected_by(change),
                )?
            } else {
                write()?
            };
            if saved.outcome == ApplyOutcome::Applied
                && source == crate::messenger::SOURCE_ID
                && (saved.history_changed || media_invalidated.is_some())
            {
                self.note_media_change();
                if let Some(file_id) = media_invalidated {
                    self.publish(
                        owner,
                        generation,
                        SyncChange::MediaFileInvalidated { file_id },
                    );
                }
            }
            if let Some(count) = unread.filter(|_| saved.unread_changed) {
                self.publish(owner, generation, SyncChange::Unread(count));
            }
            if let (Some(placement), Some(_), Some(message)) =
                (placement, metadata, event.message())
                && let (Ok(stream_id), Ok(topic_id), Ok(message_id)) = (
                    placement.stream_id.parse(),
                    placement.topic_id.parse(),
                    message.id.parse(),
                )
            {
                let scope = HistoryScope {
                    stream_id,
                    topic_id: Some(topic_id),
                };
                if saved.message_inserted {
                    self.publish(
                        owner,
                        generation,
                        SyncChange::HistoryMessageCreated { scope, message_id },
                    );
                }
                // REST может записать эту строку первым. Допуск рабочего процесса зависит от
                // нового курсора WS, а не от результата INSERT для общего тела истории.
                if saved.outcome == ApplyOutcome::Applied
                    && source == crate::messenger::SOURCE_ID
                    && let Some(change) = key_service_update(scope, message_id, &message.body)
                {
                    self.publish(owner, generation, change);
                }
                if saved.send_confirmed {
                    self.publish(owner, generation, SyncChange::OutboxChanged { scope });
                }
            }
            if saved.outcome == ApplyOutcome::Applied && source == crate::messenger::SOURCE_ID {
                if let Some(message_id) = saved.reaction_message {
                    self.publish(
                        owner,
                        generation,
                        SyncChange::HistoryMessageReactionsChanged { message_id },
                    );
                }
                for &user_id in &saved.avatar_changed {
                    self.publish_avatar_change(owner, generation, user_id);
                }
                let sidebar = match &event {
                    Event::SidebarChanged { change, .. } => Some(change),
                    Event::FolderChanged { sidebar, .. }
                    | Event::UserUpdated { sidebar, .. }
                    | Event::CatalogCountsChanged { sidebar, .. } => sidebar.as_deref(),
                    _ => None,
                };
                if let Some(change) = sidebar {
                    for stream_id in self.sidebar_cache.apply(owner, generation, change) {
                        self.publish(
                            owner,
                            generation,
                            SyncChange::SidebarDetailsChanged { stream_id },
                        );
                    }
                }
                if saved.history_changed
                    && let Some(change) = history.and_then(history_update)
                {
                    match &change {
                        SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(id)) => {
                            self.sidebar_cache
                                .invalidate(owner, generation, Some(*id), true);
                        }
                        SyncChange::HistoryAccessRevoked(HistoryRevocation::Topic {
                            stream_id,
                            topic_id,
                        }) => {
                            for stream_id in self.sidebar_cache.apply(
                                owner,
                                generation,
                                &SidebarChange::RemoveTopic {
                                    stream_id: *stream_id,
                                    topic_id: *topic_id,
                                },
                            ) {
                                self.publish(
                                    owner,
                                    generation,
                                    SyncChange::SidebarDetailsChanged { stream_id },
                                );
                            }
                        }
                        _ => {}
                    }
                    self.publish(owner, generation, change);
                }
            }
            if saved.message_inserted
                && let Event::IncomingDirectMessage {
                    message,
                    conversation_id,
                    ..
                } = event
                && message.body.markdown().is_some()
            {
                self.publish(
                    owner,
                    generation,
                    SyncChange::IncomingDirectMessage {
                        conversation_id,
                        message_id: message.id,
                    },
                );
            }
            Ok(saved.outcome)
        })?
    }

    /// Сообщает, что очередь указанной темы изменилась.
    fn publish_send(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        snapshot: &PreparedSend,
    ) {
        self.publish(
            owner,
            generation,
            SyncChange::OutboxChanged {
                scope: HistoryScope {
                    stream_id: snapshot.stream_id,
                    topic_id: Some(snapshot.topic_id),
                },
            },
        );
    }

    /// Освобождает сведения и изображения после завершения сессии.
    pub(crate) fn clear_sidebar_session(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) {
        self.sidebar_cache.clear_session(owner, generation);
        self.avatar_cache.clear_session(owner, generation);
    }

    /// Перечитывает панель после подтверждённого действия текущего пользователя.
    pub(crate) fn refresh_sidebar(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
    ) -> Result<(), AuthError> {
        self.auth.with_active_generation(owner, generation, || {
            self.sidebar_cache
                .invalidate(owner, generation, Some(stream_id), false);
            self.publish(
                owner,
                generation,
                SyncChange::SidebarDetailsChanged { stream_id },
            );
        })
    }

    /// Скрывает историю сразу после выхода. Запись в базу обновит событие `stream.deleted`.
    pub(crate) fn complete_sidebar_leave(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        leave_grant: u64,
    ) -> Result<(), AuthError> {
        self.auth.with_active_generation(owner, generation, || {
            if self
                .sidebar_cache
                .complete_leave(owner, generation, stream_id, leave_grant)
            {
                self.publish(
                    owner,
                    generation,
                    SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(stream_id)),
                );
            }
        })
    }

    /// Сообщает подписчикам аватара об изменении после записи в базу в действующей сессии.
    pub(crate) fn publish_avatar_change(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        user_id: Uuid,
    ) {
        self.publish(owner, generation, SyncChange::AvatarChanged { user_id });
    }

    /// Публикует изменение под блокировкой действующей сессии после записи в базу.
    fn publish(&self, owner: &OwnerContext, generation: SessionGeneration, change: SyncChange) {
        let _ = self.updates.send(SyncUpdate {
            context: owner.clone(),
            generation,
            change,
        });
    }
}

/// Даёт рабочему процессу только адрес ответа. Перед применением он заново читает серверные записи.
fn key_service_update(
    scope: HistoryScope,
    message_id: Uuid,
    body: &MessageBody,
) -> Option<SyncChange> {
    let MessageBody::KeyService { payload, .. } = body else {
        return None;
    };
    let target = match payload {
        KeyServicePayload::Grant { target, .. } | KeyServicePayload::Reject(target) => target,
        _ => return None,
    };
    Some(SyncChange::KeyServiceMessage {
        scope,
        message_id,
        key_id: target.key_uuid,
        recipient_device_id: target.recipient_device_id.clone(),
    })
}

/// Выбирает изменение, которое открытая история должна показать после записи.
fn history_update(change: &HistoryChange) -> Option<SyncChange> {
    let update = match change {
        HistoryChange::Edit { message, .. } => SyncChange::HistoryMessageChanged {
            message_id: message.id.parse().ok()?,
        },
        HistoryChange::RemoveMessage(id) => SyncChange::HistoryMessageChanged {
            message_id: id.parse().ok()?,
        },
        HistoryChange::RemoveStream(id) => {
            SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(id.parse().ok()?))
        }
        HistoryChange::RemoveTopic(address) => {
            SyncChange::HistoryAccessRevoked(HistoryRevocation::Topic {
                stream_id: address.stream_id.parse().ok()?,
                topic_id: address.topic_id.parse().ok()?,
            })
        }
    };
    Some(update)
}
