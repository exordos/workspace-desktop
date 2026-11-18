//! Проверяет загрузку фото по UUID, повторное открытие базы и обновление открытых подписок.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    },
    time::Duration,
};

use reqwest::Url;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use serde_json::json;
use tokio::{
    io::AsyncWriteExt,
    sync::{Notify, watch},
    time::timeout,
};
use uuid::Uuid;
use workspace_core::{
    context::{OwnerContext, SessionGeneration},
    directory::{AvatarState, AvatarStore},
    messenger::{MessengerSource, SOURCE_ID},
    storage::{Cursor, Storage},
    sync::{AvatarUpdate, Event, Sync},
};

use super::{CA, Fixture, HistoryApi, HttpsSocket, serve};

const PERSON: Uuid = Uuid::from_u128(51);
const FIRST: Uuid = Uuid::from_u128(61);
const SECOND: Uuid = Uuid::from_u128(62);
const THIRD: Uuid = Uuid::from_u128(63);
const DEADLINE: Duration = Duration::from_secs(5);

/// Считает чтения профиля и скачивания, позволяя задержать один файл.
pub(super) struct Api {
    profiles: AtomicUsize,    // Число запросов профиля по UUID человека.
    downloads: AtomicUsize,   // Число запросов по UUID файла.
    hold: Uuid,               // Идентификатор файла, ответ на который ждёт разрешения.
    received: Notify,         // Запрос задержанного файла дошёл до сервера.
    release: Notify,          // Сервер может закончить задержанный ответ.
    fail_profile: AtomicBool, // Следующее чтение профиля отвечает временной ошибкой.
    profile_failed: Notify,   // Сервер отправил ошибку профиля.
}

impl Api {
    /// Создаёт управляемые ответы для одного человека и трёх версий его фото.
    fn new(hold: Uuid) -> Self {
        Self {
            profiles: AtomicUsize::new(0),
            downloads: AtomicUsize::new(0),
            hold,
            received: Notify::new(),
            release: Notify::new(),
            fail_profile: AtomicBool::new(false),
            profile_failed: Notify::new(),
        }
    }

    /// Возвращает профиль и байты с сигнатурой PNG, различающиеся по версии файла.
    pub(super) async fn reply(
        &self,
        mut socket: HttpsSocket,
        request: Url,
        method: &str,
        headers: &str,
    ) {
        assert_eq!(method, "GET");
        assert!(headers.contains("authorization: Bearer first"));
        let (mime, body) = if request.path() == format!("/api/workspace/v1/users/{PERSON}") {
            self.profiles.fetch_add(1, Relaxed);
            if self.fail_profile.swap(false, Relaxed) {
                socket.get_mut().write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                self.profile_failed.notify_one();
                return;
            }
            (
                "application/json",
                json!({"uuid": PERSON, "avatar": format!("urn:image:{FIRST}")})
                    .to_string()
                    .into_bytes(),
            )
        } else {
            let file: Uuid = request
                .path()
                .strip_prefix("/api/workspace/v1/messenger/files/")
                .unwrap()
                .strip_suffix("/actions/download")
                .unwrap()
                .parse()
                .unwrap();
            assert!([FIRST, SECOND, THIRD].contains(&file));
            self.downloads.fetch_add(1, Relaxed);
            if file == self.hold {
                self.received.notify_one();
                self.release.notified().await;
            }
            let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
            bytes.extend(file.as_bytes());
            ("image/png", bytes)
        };
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let server = socket.get_mut();
        // Отменённая загрузка может закрыть соединение до ответа.
        if server.write_all(header.as_bytes()).await.is_ok() {
            let _ = server.write_all(&body).await;
        }
    }
}

/// Запускает уже используемый тестами HTTPS-сервер со сценариями аватаров.
async fn server(api: Arc<Api>) -> (String, tokio::task::JoinHandle<()>) {
    serve(Arc::new(HistoryApi {
        avatars: Some(api),
        ..HistoryApi::default()
    }))
    .await
}

/// Открывает общий клиент Workspace с тестовым центром сертификации.
fn source() -> Arc<MessengerSource> {
    let ca = CertificateDer::from_pem_slice(CA).unwrap();
    Arc::new(MessengerSource::with_private_ca(ca.as_ref()).unwrap())
}

/// Ждёт снимок, который удовлетворяет проверке, и возвращает его копию.
async fn wait_state(
    receiver: &mut watch::Receiver<AvatarState>,
    check: impl FnMut(&AvatarState) -> bool,
) -> AvatarState {
    timeout(DEADLINE, receiver.wait_for(check))
        .await
        .expect("снимок аватара не пришёл вовремя")
        .expect("подписка закрылась до ожидаемого снимка")
        .clone()
}

/// Ждёт байты заданной версии изображения.
async fn wait_image(receiver: &mut watch::Receiver<AvatarState>, file_id: Uuid) -> AvatarState {
    wait_state(receiver, |state| matches!(state, AvatarState::Ready(image) if image.bytes.ends_with(file_id.as_bytes()))).await
}

/// Передаёт Sync новый аватар пользователя через тот же маршрут, что использует журнал.
fn update(
    sync: &Sync,
    owner: &OwnerContext,
    generation: SessionGeneration,
    version: u64,
    avatar: AvatarUpdate,
) {
    sync.apply(
        owner,
        generation,
        SOURCE_ID,
        Event::UserUpdated {
            cursor: Cursor {
                epoch_generation: "avatars".into(),
                epoch_version: version,
            },
            user_id: PERSON,
            avatar,
            sidebar: None,
        },
    )
    .unwrap();
}

/// Незнакомый UUID загружается один раз для двух наблюдателей и доступен из SQLite после открытия.
#[tokio::test]
async fn avatar_store_shares_requests_and_reads_bytes_after_reopen() {
    let api = Arc::new(Api::new(FIRST));
    let (url, worker) = server(api.clone()).await;
    let fixture = Fixture::new(|_, _| unreachable!());
    let owner = fixture.login_at(&url, "first");
    let generation = fixture.auth.snapshot()[0].generation;
    let profile = tempfile::tempdir().unwrap();
    let database = profile.path().join("workspace.sqlite");
    let sync = Arc::new(Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::open(&database).unwrap()),
    ));
    let store = AvatarStore::new(sync.clone(), source());
    let mut first = store.subscribe(&owner, generation, PERSON).unwrap();
    timeout(DEADLINE, api.received.notified())
        .await
        .expect("не дошёл запрос первого файла");
    let mut other = AvatarStore::new(sync.clone(), source())
        .subscribe(&owner, generation, PERSON)
        .unwrap();
    api.release.notify_one();
    assert_eq!(
        wait_image(&mut first, FIRST).await,
        wait_image(&mut other, FIRST).await
    );
    assert_eq!(api.profiles.load(Relaxed), 1);
    assert_eq!(api.downloads.load(Relaxed), 1);

    drop((first, other, store, sync));
    worker.abort();
    let reopened = Arc::new(Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::open(&database).unwrap()),
    ));
    let store = AvatarStore::new(reopened, source());
    let mut cached = store.subscribe(&owner, generation, PERSON).unwrap();
    wait_image(&mut cached, FIRST).await;
    fixture.auth.logout(&owner.account_id).unwrap();
    wait_state(&mut cached, |state| *state == AvatarState::SessionEnded).await;
    assert!(store.subscribe(&owner, generation, PERSON).is_err());
}

/// Смена ссылки отменяет устаревшую загрузку, а сброс обновляет ту же подписку.
#[tokio::test]
async fn avatar_store_replaces_cached_photo_and_rejects_delayed_version() {
    let api = Arc::new(Api::new(SECOND));
    let (url, worker) = server(api.clone()).await;
    let fixture = Fixture::new(|_, _| unreachable!());
    let owner = fixture.login_at(&url, "first");
    let generation = fixture.auth.snapshot()[0].generation;
    let sync = Arc::new(Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    ));
    let store = AvatarStore::new(sync.clone(), source());
    let mut watcher = store.subscribe(&owner, generation, PERSON).unwrap();
    wait_image(&mut watcher, FIRST).await;

    update(
        &sync,
        &owner,
        generation,
        1,
        AvatarUpdate::Reference(Some(format!("urn:image:{SECOND}"))),
    );
    timeout(DEADLINE, api.received.notified())
        .await
        .expect("не началось скачивание второй версии");
    update(
        &sync,
        &owner,
        generation,
        2,
        AvatarUpdate::Reference(Some(format!("urn:image:{THIRD}"))),
    );
    let newest = wait_image(&mut watcher, THIRD).await;
    api.release.notify_one();

    api.fail_profile.store(true, Relaxed);
    update(&sync, &owner, generation, 3, AvatarUpdate::Reconcile);
    timeout(DEADLINE, api.profile_failed.notified())
        .await
        .expect("не началась сверка после неполного события");
    assert_eq!(*watcher.borrow(), newest);
    let mut retry = store.subscribe(&owner, generation, PERSON).unwrap();
    wait_image(&mut retry, FIRST).await;
    wait_image(&mut watcher, FIRST).await;

    update(&sync, &owner, generation, 4, AvatarUpdate::Reference(None));
    wait_state(&mut watcher, |state| *state == AvatarState::Missing).await;
    assert_eq!(api.profiles.load(Relaxed), 3);
    assert_eq!(api.downloads.load(Relaxed), 4);
    worker.abort();
}
