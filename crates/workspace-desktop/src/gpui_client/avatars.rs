//! Связывает аватары GPUI с подписками Core по UUID пользователя.
//! Поверхности выбирают нужные UUID, а общая сущность декодирует фото для всех подписчиков.

mod decode;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use gpui::{
    App, AppContext, BackgroundExecutor, Context, Entity, Global, ImageSource, RenderImage,
    Subscription, Task, WeakEntity,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use uuid::Uuid;
use workspace_core::directory::{AvatarFormat, AvatarImage, AvatarState};

use super::{
    contracts::AccountSession,
    runtime::{AvatarSubscription, RequestError, RuntimeClient},
};

const CONCURRENT_SUBSCRIPTIONS: usize = 8;
const CONCURRENT_DECODES: usize = 2;
const DECODED_BYTE_LIMIT: usize = 64 * 1024 * 1024;
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(40);

/// Подписки одного экрана. Экран назначает только UUID, нужные для отрисовки.
#[derive(Default)]
pub(super) struct AvatarImages {
    scope: Option<AvatarScope>, // Какой вход и рабочий поток выбраны экраном.
    entries: HashMap<Uuid, AvatarObservation>, // Живые аватары и уведомления экрана.
}

/// Область, в которой экраны могут разделять один аватар пользователя.
#[derive(Clone, Eq, Hash, PartialEq)]
struct AvatarScope {
    runtime_id: u64,         // Рабочий поток именно этого профиля приложения.
    session: AccountSession, // Аккаунт, проект и поколение входа.
}

/// Связывает общую сущность аватара с текущим экраном.
struct AvatarObservation {
    photo: Entity<AvatarPhoto>, // Последнее декодированное изображение.
    _changes: Subscription,     // Будит экран при появлении и удалении изображения.
}

/// Ключ аватара по UUID человека в выбранном рабочем профиле.
#[derive(Eq, Hash, PartialEq)]
struct AvatarKey {
    scope: AvatarScope, // Владелец данных и поколение сессии.
    user_id: Uuid,      // UUID человека на фотографии.
}

/// Индекс открытых аватаров App. Экран удерживает сущность, пока она ему нужна.
#[derive(Default)]
struct AvatarRegistry {
    entries: HashMap<AvatarKey, WeakEntity<AvatarPhoto>>, // Одна сущность на ключ, пока есть наблюдатели.
    workload: Arc<AvatarWorkload>, // Общие пределы для подписок и декодирования.
}

impl Global for AvatarRegistry {}

/// Ограничивает количество одновременных операций и объём готовых изображений.
struct AvatarWorkload {
    requests: Semaphore, // Места для коротких запросов подписки через RuntimeClient.
    decodes: Arc<Semaphore>, // Места для фонового декодирования, пока выполняется работа.
    pixels: Arc<Semaphore>, // Разрешения в байтах для готовых кадров.
}

impl Default for AvatarWorkload {
    /// Готовит пределы работы для всех экранов приложения.
    fn default() -> Self {
        Self {
            requests: Semaphore::new(CONCURRENT_SUBSCRIPTIONS),
            decodes: Arc::new(Semaphore::new(CONCURRENT_DECODES)),
            pixels: Arc::new(Semaphore::new(DECODED_BYTE_LIMIT)),
        }
    }
}

/// Изображение в памяти GPUI с учётом занимаемых байтов.
struct DecodedAvatar {
    image: Arc<RenderImage>, // Общие пиксели для всех виджетов одного аватара.
    _pixels: OwnedSemaphorePermit, // Освобождает бюджет при сбросе снимка.
}

/// Принимает снимки Core и публикует последнее готовое изображение.
struct AvatarPhoto {
    decoded: Option<DecodedAvatar>, // None оставляет инициалы или штатную иконку.
    _watch: Task<()>,               // Отмена освобождает подписку Core.
}

impl AvatarImages {
    /// Синхронизирует подписки с нужными экрану UUID в активном входе.
    pub(super) fn refresh<V: 'static>(
        &mut self,
        users: impl IntoIterator<Item = Uuid>,
        runtime: &RuntimeClient,
        session: &AccountSession,
        cx: &mut Context<V>,
    ) {
        let scope = AvatarScope {
            runtime_id: runtime.avatar_runtime_id(),
            session: session.clone(),
        };
        if self.scope.as_ref() != Some(&scope) {
            self.entries.clear();
            self.scope = Some(scope.clone());
        }
        let mut required = HashSet::new();
        for user_id in users.into_iter().filter(|id| !id.is_nil()) {
            required.insert(user_id);
            self.entries.entry(user_id).or_insert_with(|| {
                let key = AvatarKey {
                    scope: scope.clone(),
                    user_id,
                };
                let photo = shared_photo(key, runtime, cx);
                let changes = cx.observe(&photo, |_, _, cx| cx.notify());
                AvatarObservation {
                    photo,
                    _changes: changes,
                }
            });
        }
        self.entries.retain(|user_id, _| required.contains(user_id));
    }

    /// Возвращает готовое фото человека или оставляет оформление заглушки экрану.
    pub(super) fn image(&self, user_id: Uuid, app: &App) -> Option<ImageSource> {
        let image = &self
            .entries
            .get(&user_id)?
            .photo
            .read(app)
            .decoded
            .as_ref()?
            .image;
        Some(ImageSource::Render(image.clone()))
    }

    /// Освобождает подписки закрытого экрана или прежнего входа.
    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.scope = None;
    }
}

/// Переиспользует сущность аватара, пока она нужна хотя бы одному экрану App.
fn shared_photo(key: AvatarKey, runtime: &RuntimeClient, app: &mut App) -> Entity<AvatarPhoto> {
    let registry = app.default_global::<AvatarRegistry>();
    if let Some(photo) = registry.entries.get(&key).and_then(WeakEntity::upgrade) {
        return photo;
    }
    let workload = registry.workload.clone();
    let photo = app.new(|cx| AvatarPhoto::new(&key, runtime.clone(), workload, cx));
    let entries = &mut app.default_global::<AvatarRegistry>().entries;
    // Слабые ссылки на закрытые экраны не занимают место в бюджете изображений.
    if entries.len() == entries.capacity() {
        entries.retain(|_, photo| photo.upgrade().is_some());
    }
    entries.insert(key, photo.downgrade());
    photo
}

impl AvatarPhoto {
    /// Подписывается на Core и передаёт обработку байтов фоновому исполнителю GPUI.
    fn new(
        key: &AvatarKey,
        runtime: RuntimeClient,
        workload: Arc<AvatarWorkload>,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = key.scope.session.clone();
        let user_id = key.user_id;
        let background = cx.background_executor().clone();
        cx.on_release(|photo, app| {
            photo.remove_image(app);
        })
        .detach();
        let task = cx.spawn(async move |photo, cx| {
            let Some(mut updates) =
                subscribe(runtime, session, user_id, &workload, &background).await
            else {
                return;
            };
            loop {
                let state = updates.borrow_and_update().clone();
                // Новый снимок освобождает резерв старого, пока ждём место для новых пикселей.
                if photo
                    .update(cx, |photo, cx| {
                        if photo.remove_image(cx) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
                if let AvatarState::Ready(avatar) = state {
                    let decoded = tokio::select! {
                        biased;
                        changed = updates.changed() => {
                            if changed.is_err() { break; }
                            continue;
                        }
                        decoded = render_image(avatar, &workload, &background) => decoded,
                    };
                    if let Some(decoded) = decoded {
                        match updates.has_changed() {
                            Ok(true) => continue,
                            Err(_) => break,
                            Ok(false) => {}
                        }
                        if photo
                            .update(cx, |photo, cx| {
                                photo.decoded = Some(decoded);
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                if updates.changed().await.is_err() {
                    break;
                }
            }
            let _ = photo.update(cx, |photo, cx| {
                if photo.remove_image(cx) {
                    cx.notify();
                }
            });
        });
        Self {
            decoded: None,
            _watch: task,
        }
    }

    /// Освобождает пиксели и текстуры изображения после снятия последней подписки или замены.
    fn remove_image(&mut self, app: &mut App) -> bool {
        let Some(decoded) = self.decoded.take() else {
            return false;
        };
        app.defer(move |app| app.drop_image(decoded.image, None));
        true
    }
}

/// Ждёт бюджет до декодирования. При нехватке места для анимации использует один кадр GIF.
async fn render_image(
    avatar: Arc<AvatarImage>,
    workload: &AvatarWorkload,
    background: &BackgroundExecutor,
) -> Option<DecodedAvatar> {
    let animation = (avatar.format == AvatarFormat::Gif)
        .then(|| {
            workload
                .pixels
                .clone()
                .try_acquire_many_owned(decode::MAX_ANIMATED_BYTES)
                .ok()
        })
        .flatten();
    let animated = animation.is_some();
    let mut pixels = match animation {
        Some(pixels) => pixels,
        None => workload
            .pixels
            .clone()
            .acquire_many_owned(decode::MAX_STILL_BYTES)
            .await
            .ok()?,
    };
    let permit = workload.decodes.clone().acquire_owned().await.ok()?;
    background
        .spawn(async move {
            let _permit = permit;
            let image = decode::decode(&avatar, animated)?;
            let bytes = (0..image.frame_count())
                .filter_map(|frame| image.as_bytes(frame))
                .map(<[u8]>::len)
                .sum::<usize>();
            Some(DecodedAvatar {
                image: Arc::new(image),
                _pixels: pixels.split(bytes)?,
            })
        })
        .await
}

/// Получает наблюдение короткой командой и ждёт свободную очередь после временной перегрузки.
async fn subscribe(
    runtime: RuntimeClient,
    session: AccountSession,
    user_id: Uuid,
    workload: &AvatarWorkload,
    background: &BackgroundExecutor,
) -> Option<watch::Receiver<AvatarState>> {
    loop {
        let answer = {
            let _permit = workload.requests.acquire().await.ok()?;
            runtime.subscribe_avatar(session.clone(), user_id).await
        };
        match answer {
            Ok(AvatarSubscription::Ready(updates)) => return Some(updates),
            Ok(AvatarSubscription::Waiting(ready)) => {
                if !ready.await {
                    return None;
                }
            }
            Err(RequestError::Busy) => background.timer(BUSY_RETRY_DELAY).await,
            Err(RequestError::Stopped) => return None,
        }
    }
}
