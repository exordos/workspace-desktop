//! Загружает аватары по UUID пользователя и передаёт общий снимок всем подписчикам.
//! SQLite хранит байты, а задача наблюдения сверяет их со справочником и событиями.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use futures::{Stream, StreamExt};
use thiserror::Error;
use tokio::sync::{Notify, Semaphore, broadcast, watch};
use uuid::Uuid;

use crate::{
    auth::{AuthError, Service, SessionSnapshot, SessionStatus},
    context::{OwnerContext, SessionGeneration},
    messenger::{MessengerSource, details::DetailsError},
    storage::{AvatarFence, AvatarSnapshot, StorageError},
    sync::{Sync, SyncChange, SyncUpdate},
};

use super::avatar_file_id;

const PROFILE_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
const PARALLEL_REQUEST_LIMIT: usize = 4;

/// Форматы изображений, которые принимает Workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AvatarFormat {
    Png,  // PNG.
    Jpeg, // JPEG.
    Gif,  // GIF.
    Webp, // WebP.
}

impl AvatarFormat {
    /// Распознаёт поддерживаемый аватар по началу файла, независимо от имени.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Some(Self::Webp)
        } else {
            None
        }
    }

    /// Возвращает канонический MIME для сохранения формата в базе.
    fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    /// Читает сохранённый формат и пропускает неизвестные значения.
    fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }
}

/// Файл аватара с проверенным форматом и размером.
#[derive(Debug, Eq, PartialEq)]
pub struct AvatarImage {
    pub format: AvatarFormat, // Формат для выбора декодера.
    pub bytes: Vec<u8>,       // Содержимое файла в пределах серверного лимита.
}

/// Текущее состояние аватара для подписчиков одной сессии.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AvatarState {
    Loading,                 // Core ищет или скачивает фото.
    Ready(Arc<AvatarImage>), // Общие байты пригодного фото.
    Missing,                 // Фото отсутствует или его источник не поддержан.
    Failed,                  // Фото сейчас недоступно. Запрос можно повторить.
    SessionEnded,            // Сессия больше не может использовать этот кэш.
}

/// Даёт доступ к аватарам по UUID человека через действующую сессию Workspace.
#[derive(Clone)]
pub struct AvatarStore {
    sync: Arc<Sync>,              // Сессии, локальная база и события профиля.
    source: Arc<MessengerSource>, // HTTPS-адаптер для чтения профиля и файла.
}

/// Определяет условия, при которых подписчики могут разделить один запрос.
#[derive(Clone, Eq, Hash, PartialEq)]
struct AvatarRequest {
    owner: OwnerContext,           // Владелец кэша и токена.
    generation: SessionGeneration, // Номер входа владельца.
    user_id: Uuid,                 // Пользователь на фотографии.
}

/// Состояние, которое разделяют все подписчики одного аватара.
struct AvatarObservers {
    state: watch::Sender<AvatarState>, // Последний снимок для всех подписчиков.
    requested: Notify,                 // Новое обращение может потребовать сверки или повтора.
}

impl AvatarObservers {
    /// Публикует только изменения, чтобы одинаковый снимок не будил подписчиков.
    fn publish(&self, next: AvatarState) {
        self.state.send_if_modified(|current| {
            if *current == next {
                return false;
            }
            *current = next;
            true
        });
    }
}

/// Объединяет подписки и ограничивает фоновую сеть всех запросов одного Sync.
pub(crate) struct AvatarLoads {
    entries: Mutex<HashMap<AvatarRequest, Weak<AvatarObservers>>>, // Слабая ссылка живёт до удаления записи.
    permits: Semaphore, // Сколько задач может одновременно читать профиль и скачивать фото.
}

impl Default for AvatarLoads {
    /// Создаёт общий лимит сетевых запросов и пустой список подписок.
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            permits: Semaphore::new(PARALLEL_REQUEST_LIMIT),
        }
    }
}

/// Определяет, какую часть аватара нужно получить с сервера.
struct AvatarWork {
    fence: AvatarFence,  // Версия профиля и граница сброса на момент принятия решения.
    check_profile: bool, // Нужно узнать актуальную ссылку на изображение.
    file_id: Option<Uuid>, // UUID исходного файла, если его байтов нет в базе.
}

/// Различает ошибки проверки сессии, локальной базы и запроса Workspace.
#[derive(Debug, Error)]
enum AvatarLoadError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Текущий вход недоступен.
    #[error(transparent)]
    Storage(#[from] StorageError), // Не удалось прочитать или записать кэш.
    #[error(transparent)]
    Network(#[from] DetailsError), // Не удалось получить профиль или изображение.
}

impl AvatarStore {
    /// Подключает справочник к общему Sync и уже настроенному клиенту Workspace.
    pub fn new(sync: Arc<Sync>, source: Arc<MessengerSource>) -> Self {
        Self { sync, source }
    }

    /// На рабочем Tokio Runtime подписывает на аватар пользователя и при необходимости загружает его.
    /// Подписка выдаёт байты из SQLite до сверки с сервером и завершает работу при выходе из сессии.
    pub fn subscribe(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        user_id: Uuid,
    ) -> Result<watch::Receiver<AvatarState>, AuthError> {
        self.sync
            .auth()
            .with_confirmed_generation(owner, generation, || {
                if user_id.is_nil() {
                    return watch::channel(AvatarState::Missing).1;
                }
                let request = AvatarRequest {
                    owner: owner.clone(),
                    generation,
                    user_id,
                };
                let mut entries = self.sync.avatar_loads.entries.lock().unwrap();
                if let Some(observers) = entries.get(&request).and_then(Weak::upgrade)
                    && !observers.state.is_closed()
                {
                    let receiver = observers.state.subscribe();
                    if *receiver.borrow() != AvatarState::Loading {
                        observers.requested.notify_one();
                    }
                    return receiver;
                }
                entries.retain(|_, entry| entry.strong_count() > 0);
                let (state, receiver) = watch::channel(AvatarState::Loading);
                let observers = Arc::new(AvatarObservers {
                    state,
                    requested: Notify::new(),
                });
                entries.insert(request.clone(), Arc::downgrade(&observers));
                let store = self.clone();
                let updates = self.sync.subscribe();
                tokio::spawn(async move { store.follow(request, &observers, updates).await });
                receiver
            })
    }

    /// Перечитывает кэш по запросу и реагирует на события профиля, пока есть подписчики.
    async fn follow(
        self,
        request: AvatarRequest,
        observers: &AvatarObservers,
        mut updates: broadcast::Receiver<SyncUpdate>,
    ) {
        let mut sessions = Box::pin(self.sync.auth().subscribe());
        let mut status = session_status(&request, &sessions.next().await.unwrap_or_default());
        loop {
            let auth = self.sync.auth();
            if auth
                .with_confirmed_generation(&request.owner, request.generation, || ())
                .is_err()
            {
                observers.publish(AvatarState::SessionEnded);
                return;
            }
            tokio::select! {
                biased;
                _ = observers.state.closed() => {
                    // Новый подписчик мог подключиться после сигнала о закрытии.
                    let _entries = self.sync.avatar_loads.entries.lock().unwrap();
                    if observers.state.is_closed() {
                        return;
                    }
                },
                _ = wait_session_change(&request, &mut sessions, &mut status) => {},
                _ = wait_avatar_change(&request, &mut updates) => {},
                _ = async {
                    let active = auth.with_active_generation(&request.owner, request.generation, || ()).is_ok();
                    if let Err(error) = self.refresh(&request, observers, active).await {
                        log::debug!("Не удалось обновить аватар: {error}");
                        self.publish_failure(&request, observers);
                    }
                    observers.requested.notified().await;
                } => {},
            }
        }
    }

    /// Выдаёт сохранённое фото и получает только недостающие или просроченные данные.
    async fn refresh(
        &self,
        request: &AvatarRequest,
        observers: &AvatarObservers,
        active: bool,
    ) -> Result<(), AvatarLoadError> {
        let auth = self.sync.auth();
        let missing = if active {
            AvatarState::Loading
        } else {
            AvatarState::Failed
        };
        let work = auth.with_confirmed_generation(&request.owner, request.generation, || {
            let snapshot = self.sync.storage().avatar_snapshot(
                &request.owner,
                request.user_id,
                unix_seconds(),
            )?;
            Ok::<_, StorageError>(publish_snapshot(observers, snapshot, missing))
        })??;
        let Some(mut work) = work.filter(|_| active) else {
            return Ok(());
        };
        // Ограничиваем профиль и скачивание одним разрешением. Блокировка сессии к этому моменту снята.
        let _permit = self
            .sync
            .avatar_loads
            .permits
            .acquire()
            .await
            .expect("avatar semaphore");
        let token = auth
            .access_token_for_generation(&request.owner, Service::Messenger, request.generation)
            .await?;
        // Ответ устаревает уже в очереди, если в это время пришло событие или сбросился журнал.
        let current = auth.with_active_generation(&request.owner, request.generation, || {
            self.sync
                .storage()
                .avatar_fence_is_current(&request.owner, request.user_id, work.fence)
        })??;
        if !current {
            return Ok(());
        }
        if work.check_profile {
            let reference = self
                .source
                .avatar_reference(&request.owner, &token, request.user_id)
                .await?;
            let next = self.save_reference(request, work.fence, reference.as_deref(), observers)?;
            let Some(next) = next else { return Ok(()) };
            work = next;
        }
        let Some(file_id) = work.file_id else {
            return Ok(());
        };
        match self
            .source
            .avatar_image(&request.owner, &token, file_id)
            .await
        {
            Ok(image) => {
                auth.with_active_generation(&request.owner, request.generation, || {
                    if self.sync.storage().save_avatar_bytes(
                        &request.owner,
                        request.user_id,
                        work.fence,
                        image.format.mime(),
                        &image.bytes,
                        unix_seconds(),
                    )? {
                        observers.publish(AvatarState::Ready(Arc::new(image)));
                    }
                    Ok::<_, StorageError>(())
                })??;
            }
            Err(DetailsError::ProfileUnavailable) => {
                // 403 и 404 подтверждают, что этот файл больше нельзя показывать.
                self.save_reference(request, work.fence, None, observers)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    /// Фиксирует результат сверки только когда профиль и сессия не изменились за время запроса.
    fn save_reference(
        &self,
        request: &AvatarRequest,
        fence: AvatarFence,
        reference: Option<&str>,
        observers: &AvatarObservers,
    ) -> Result<Option<AvatarWork>, AvatarLoadError> {
        self.sync
            .auth()
            .with_active_generation(&request.owner, request.generation, || {
                let saved = self.sync.storage().save_avatar_reference(
                    &request.owner,
                    request.user_id,
                    fence,
                    reference,
                    unix_seconds(),
                )?;
                Ok(saved.and_then(|snapshot| {
                    publish_snapshot(observers, snapshot, AvatarState::Loading)
                }))
            })?
    }

    /// При временной ошибке оставляет фото из текущего снимка, если оно ещё доступно.
    fn publish_failure(&self, request: &AvatarRequest, observers: &AvatarObservers) {
        let _ =
            self.sync
                .auth()
                .with_confirmed_generation(&request.owner, request.generation, || {
                    match self.sync.storage().avatar_snapshot(
                        &request.owner,
                        request.user_id,
                        unix_seconds(),
                    ) {
                        Ok(snapshot) => {
                            publish_snapshot(observers, snapshot, AvatarState::Failed);
                        }
                        Err(_) => observers.publish(AvatarState::Failed),
                    }
                });
    }
}

/// Выдаёт байты или состояние без фото и решает, нужно ли обращаться к серверу.
fn publish_snapshot(
    observers: &AvatarObservers,
    snapshot: AvatarSnapshot,
    pending: AvatarState,
) -> Option<AvatarWork> {
    let image = snapshot.image.and_then(|image| {
        Some(AvatarImage {
            format: AvatarFormat::from_mime(&image.format)?,
            bytes: image.bytes,
        })
    });
    let check_profile = snapshot
        .checked_at
        .is_none_or(|checked| unix_seconds().saturating_sub(checked) >= PROFILE_TTL_SECONDS);
    let file_id = if image.is_none() {
        snapshot.reference.as_deref().and_then(avatar_file_id)
    } else {
        None
    };
    let needs_network = check_profile || file_id.is_some();
    observers.publish(match image {
        Some(image) => AvatarState::Ready(Arc::new(image)),
        None if needs_network => pending,
        None => AvatarState::Missing,
    });
    needs_network.then_some(AvatarWork {
        fence: snapshot.fence,
        check_profile,
        file_id,
    })
}

/// Возвращает время для сравнения отметок SQLite в секундах от Unix-эпохи.
fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

/// Находит статус только того входа, который запросил аватар.
fn session_status(request: &AvatarRequest, sessions: &[SessionSnapshot]) -> Option<SessionStatus> {
    sessions
        .iter()
        .find(|session| {
            session.context == request.owner && session.generation == request.generation
        })
        .map(|session| session.status)
}

/// Будит загрузку при смене статуса этого входа, включая восстановление сети и выход.
async fn wait_session_change(
    request: &AvatarRequest,
    sessions: &mut (impl Stream<Item = Vec<SessionSnapshot>> + Unpin),
    previous: &mut Option<SessionStatus>,
) {
    while let Some(sessions) = sessions.next().await {
        let status = session_status(request, &sessions);
        if *previous != status {
            *previous = status;
            return;
        }
    }
    std::future::pending().await
}

/// Ждёт изменения аватара, сброса источника или пропуска, после которого перечитываем кэш.
async fn wait_avatar_change(
    request: &AvatarRequest,
    updates: &mut broadcast::Receiver<SyncUpdate>,
) {
    loop {
        match updates.recv().await {
            Ok(update)
                if update.context == request.owner && update.generation == request.generation =>
            {
                match update.change {
                    SyncChange::AvatarChanged { user_id } if user_id == request.user_id => return,
                    SyncChange::MessengerReset => return,
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => return,
            Err(broadcast::error::RecvError::Closed) => std::future::pending().await,
        }
    }
}
