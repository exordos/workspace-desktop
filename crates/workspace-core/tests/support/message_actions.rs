//! Проверяет удаление и чтение квитанций через штатный HTTPS-клиент.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use rustls::{
    ServerConfig,
    crypto::ring,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use uuid::Uuid;
use workspace_core::{
    auth::AuthError,
    context::{OwnerContext, SessionGeneration},
    messenger::{
        MessengerSource,
        message_actions::{
            DeleteError, MessageTarget, MutationReceipt, MutationStatus, MutationStatusError,
        },
    },
    runtime::SourceError,
    storage::HistoryScope,
};

use super::Fixture;

const CA: &[u8] = include_bytes!("../../../../tests/fixtures/tls/ca.pem");
const CERT: &[u8] = include_bytes!("../../../../tests/fixtures/tls/localhost.pem");
const KEY: &[u8] = include_bytes!("../../../../tests/fixtures/tls/localhost-key.pem");
const API: &str = "/api/workspace/v1/messenger";
const USER: &str = "43f78a40-99a0-4197-990c-3a05c9032021";
const OTHER_USER: &str = "43f78a40-99a0-4197-990c-3a05c9032022";
const STREAM: Uuid = Uuid::from_u128(41);
const TOPIC: Uuid = Uuid::from_u128(42);
const MESSAGE: Uuid = Uuid::from_u128(43);
const RECEIPT: Uuid = Uuid::from_u128(44);
const SIBLING: Uuid = Uuid::from_u128(45);

/// Один ожидаемый запрос и управляемый ответ локального сервера.
struct Step {
    request: String,     // Метод HTTP и путь запроса.
    user: &'static str,  // Владелец тестового токена.
    status: Option<u16>, // Код ответа. None означает обрыв соединения.
    body: Option<Value>, // JSON успешного ответа.
    headers: String,     // Дополнительные заголовки ответа.
    paused: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>, // Остановить ответ до смены входа.
}

impl Step {
    /// Добавляет ожидаемый GET с JSON-ответом.
    fn get(path: String, user: &'static str, body: Value) -> Self {
        Self {
            request: format!("GET {API}/{path}"),
            user,
            status: Some(200),
            body: Some(body),
            headers: String::new(),
            paused: None,
        }
    }

    /// Добавляет ровно одну попытку удаления по публичному UUID.
    fn delete(status: Option<u16>, headers: String) -> Self {
        Self {
            request: format!("DELETE {API}/messages/{MESSAGE}"),
            user: USER,
            status,
            body: None,
            headers,
            paused: None,
        }
    }
}

/// Локальный сервер со списком ожидаемых запросов.
struct Server {
    origin: String,              // Корневой HTTPS-адрес временного сервера.
    finish: oneshot::Sender<()>, // Сигнал завершить проверку списка.
    worker: JoinHandle<()>,      // Задача сервера, передаёт сбои проверок тесту.
}

impl Server {
    /// Поднимает HTTPS и проверяет точный путь и токен каждого запроса.
    async fn start(steps: Vec<Step>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("https://{}", listener.local_addr().unwrap());
        let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from_pem_slice(CERT).unwrap()],
                PrivateKeyDer::from_pem_slice(KEY).unwrap(),
            )
            .unwrap();
        let tls = TlsAcceptor::from(Arc::new(config));
        let (finish, mut done) = oneshot::channel();
        let worker = tokio::spawn(async move {
            let mut remaining = VecDeque::from(steps);
            loop {
                tokio::select! {
                    biased;
                    connection = listener.accept() => {
                        let (socket, _) = connection.unwrap();
                        let step = remaining.pop_front().expect("лишний HTTPS-запрос");
                        let socket = tls.accept(socket).await.unwrap();
                        step.reply(BufReader::new(socket)).await;
                    }
                    _ = &mut done => break,
                }
            }
            assert!(
                remaining.is_empty(),
                "запрос не пришёл: {:?}",
                remaining.front().map(|step| &step.request)
            );
        });
        Self {
            origin,
            finish,
            worker,
        }
    }

    /// Проверяет, что все ожидаемые запросы выполнены без лишних вызовов.
    async fn verify(self) {
        let _ = self.finish.send(());
        timeout(Duration::from_secs(3), self.worker)
            .await
            .expect("сервер не завершил тест")
            .unwrap();
    }
}

impl Step {
    /// Сверяет запрос, при необходимости ждёт смены входа и возвращает ответ.
    async fn reply(
        self,
        mut socket: BufReader<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>,
    ) {
        let mut line = String::new();
        socket.read_line(&mut line).await.unwrap();
        assert_eq!(line, format!("{} HTTP/1.1\r\n", self.request));
        let mut authorized = false;
        loop {
            line.clear();
            assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.trim().split_once(':')
                && name.eq_ignore_ascii_case("authorization")
            {
                assert_eq!(value.trim(), format!("Bearer {}", self.user));
                authorized = true;
            }
        }
        assert!(authorized, "нет токена для {}", self.request);
        if let Some((received, resume)) = self.paused {
            received.send(()).unwrap();
            resume.await.unwrap();
        }
        let Some(status) = self.status else {
            return;
        };
        let body = self.body.map(|body| body.to_string()).unwrap_or_default();
        socket
            .get_mut()
            .write_all(
                format!(
                    "HTTP/1.1 {status} response\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\n{body}",
                    body.len(),
                    self.headers,
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.get_mut().shutdown().await.unwrap();
    }
}

/// Создаёт реальные зависимости команды вокруг тестового входа.
fn open(
    server: &Server,
    user: &str,
) -> (Fixture, OwnerContext, SessionGeneration, MessengerSource) {
    let fixture = Fixture::new(|_, _| unreachable!());
    let owner = fixture.login_at(&server.origin, user);
    let generation = fixture.auth.snapshot()[0].generation;
    let ca = CertificateDer::from_pem_slice(CA).unwrap();
    (
        fixture,
        owner,
        generation,
        MessengerSource::with_private_ca(ca.as_ref()).unwrap(),
    )
}

/// Выбирает одно и то же размещение в личке или точной теме.
fn target(direct: bool) -> MessageTarget {
    MessageTarget {
        scope: HistoryScope {
            stream_id: STREAM,
            topic_id: (!direct).then_some(TOPIC),
        },
        topic_id: TOPIC,
        message_id: MESSAGE,
    }
}

/// Отдаёт канонические поля нативного разговора.
fn stream(user: &'static str, direct: bool) -> Step {
    Step::get(
        format!("streams/{STREAM}"),
        user,
        json!({
            "uuid": STREAM, "project_id": "project", "user_uuid": user,
            "source_name": "native", "encryption": false, "is_archived": false,
            "private": direct, "direct_user_uuid": direct.then_some(OTHER_USER),
            "default_topic_uuid": SIBLING, "notification_mode": "all_messages",
        }),
    )
}

/// Подтверждает фактическую тему строки, даже когда в личке другая тема по умолчанию.
fn topic(user: &'static str) -> Step {
    Step::get(
        format!("stream_topics/{TOPIC}"),
        user,
        json!({
            "uuid": TOPIC, "stream_uuid": STREAM, "project_id": "project",
            "user_uuid": user, "notification_mode": "default",
        }),
    )
}

/// Возвращает подтверждённое текстовое сообщение от заданного автора.
fn message(user: &'static str, author: &'static str) -> Step {
    Step::get(
        format!("messages/{MESSAGE}"),
        user,
        json!({
            "uuid": MESSAGE, "stream_uuid": STREAM, "topic_uuid": TOPIC,
            "project_id": "project", "user_uuid": user, "author_uuid": author,
            "source_name": "native", "created_at": "2026-01-01T12:00:00Z",
            "payload": {"kind": "markdown", "content": "synthetic"},
        }),
    )
}

/// Отвечает на чтение квитанции для исходного токена.
fn receipt(status: &str) -> Step {
    Step::get(
        format!("message_mutations/{RECEIPT}"),
        USER,
        json!({
            "uuid": RECEIPT, "project_id": "project", "user_uuid": USER, "status": status,
            "command_count": 1, "applied_count": 0, "rejected_count": 0,
        }),
    )
}

/// Проверяет синхронный и асинхронный DELETE, а также видимые исходы чтения квитанции.
#[tokio::test]
async fn message_delete_uses_exact_placement_and_reads_mutation_status() {
    let mut wrong_owner = receipt("applied");
    wrong_owner.body.as_mut().unwrap()["user_uuid"] = json!(OTHER_USER);
    let mut missing = receipt("applied");
    missing.status = Some(404);
    let mut wrong_uuid = receipt("applied");
    wrong_uuid.body.as_mut().unwrap()["uuid"] = json!(SIBLING);
    let mut forbidden = receipt("applied");
    forbidden.status = Some(403);
    let server = Server::start(vec![
        stream(USER, true),
        topic(USER),
        message(USER, USER),
        Step::delete(Some(204), String::new()),
        stream(USER, false),
        topic(USER),
        message(USER, USER),
        Step::delete(Some(200), format!("X-Workspace-Mutation-Id: {RECEIPT}\r\n")),
        receipt("pending"),
        receipt("applied"),
        receipt("rejected"),
        wrong_owner,
        wrong_uuid,
        forbidden,
        missing,
    ])
    .await;
    let (fixture, owner, generation, source) = open(&server, USER);
    let _other = fixture.login_at(&server.origin, OTHER_USER);
    assert_eq!(
        source
            .delete_message(&fixture.auth, &owner, generation, target(true))
            .await
            .unwrap(),
        MutationReceipt::Applied,
    );
    let MutationReceipt::Accepted(ticket) = source
        .delete_message(&fixture.auth, &owner, generation, target(false))
        .await
        .unwrap()
    else {
        panic!("отсутствует квитанция принятой команды");
    };
    for status in [
        MutationStatus::Pending,
        MutationStatus::Applied,
        MutationStatus::Rejected,
    ] {
        assert_eq!(
            source
                .mutation_status(&fixture.auth, &ticket)
                .await
                .unwrap(),
            status
        );
    }
    for _ in 0..2 {
        assert!(matches!(
            source.mutation_status(&fixture.auth, &ticket).await,
            Err(MutationStatusError::Source(SourceError::InvalidResponse))
        ));
    }
    assert!(matches!(
        source.mutation_status(&fixture.auth, &ticket).await,
        Err(MutationStatusError::Source(SourceError::Forbidden))
    ));
    assert!(matches!(
        source.mutation_status(&fixture.auth, &ticket).await,
        Err(MutationStatusError::Unavailable)
    ));
    fixture.auth.logout(&owner.account_id).unwrap();
    let _new = fixture.login_at(&server.origin, USER);
    assert!(matches!(
        source.mutation_status(&fixture.auth, &ticket).await,
        Err(MutationStatusError::Auth(AuthError::SessionUnavailable))
    ));
    server.verify().await;
}

/// Проверяет, что чужое или непроверенное сообщение не получает DELETE.
#[tokio::test]
async fn message_delete_blocks_foreign_context_route_author_and_non_native_content() {
    let mut wrong_owner = message(USER, USER);
    wrong_owner.body.as_mut().unwrap()["user_uuid"] = json!(OTHER_USER);
    let mut wrong_placement = message(USER, USER);
    wrong_placement.body.as_mut().unwrap()["topic_uuid"] = json!(SIBLING);
    let mut encryption = stream(USER, false);
    encryption.body.as_mut().unwrap()["encryption"] = json!(true);
    let mut bridged_stream = stream(USER, false);
    bridged_stream.body.as_mut().unwrap()["source_name"] = json!("zulip");
    let mut bridged_message = message(USER, USER);
    bridged_message.body.as_mut().unwrap()["source_name"] = json!("zulip");
    let mut e2ee_message = message(USER, USER);
    e2ee_message.body.as_mut().unwrap()["payload"]["kind"] = json!("e2ee");
    let mut lost_stream = stream(USER, false);
    lost_stream.status = Some(404);
    let server = Server::start(vec![
        stream(OTHER_USER, false),
        topic(OTHER_USER),
        message(OTHER_USER, USER),
        stream(USER, false),
        topic(USER),
        wrong_owner,
        stream(USER, false),
        topic(USER),
        wrong_placement,
        encryption,
        bridged_stream,
        stream(USER, false),
        topic(USER),
        bridged_message,
        stream(USER, false),
        topic(USER),
        e2ee_message,
        lost_stream,
    ])
    .await;
    let (fixture, owner, generation, source) = open(&server, USER);
    assert!(matches!(
        source
            .delete_message(
                &fixture.auth,
                &owner,
                generation,
                MessageTarget {
                    topic_id: SIBLING,
                    ..target(false)
                }
            )
            .await,
        Err(DeleteError::InvalidTarget)
    ));
    let other = fixture.login_at(&server.origin, OTHER_USER);
    let other_generation = fixture
        .auth
        .snapshot()
        .into_iter()
        .find(|session| session.context == other)
        .unwrap()
        .generation;
    assert!(matches!(
        source
            .delete_message(&fixture.auth, &other, other_generation, target(false))
            .await,
        Err(DeleteError::NotAuthor)
    ));
    let delete = || source.delete_message(&fixture.auth, &owner, generation, target(false));
    assert!(matches!(
        delete().await,
        Err(DeleteError::Source(SourceError::InvalidResponse))
    ));
    assert!(matches!(
        delete().await,
        Err(DeleteError::MessageUnavailable)
    ));
    for _ in 0..4 {
        assert!(matches!(delete().await, Err(DeleteError::Unsupported)));
    }
    assert!(matches!(delete().await, Err(DeleteError::AccessLost)));
    server.verify().await;
}

/// Проверяет отказ, потерянный ответ и отсутствие автоматического повторного DELETE.
#[tokio::test]
async fn message_delete_distinguishes_http_failure_and_unknown_outcome() {
    let result_cases = [
        (Some(403), String::new()),
        (Some(404), String::new()),
        (Some(429), String::new()),
        (Some(408), String::new()),
        (None, String::new()),
        (Some(503), String::new()),
        (Some(204), "X-Workspace-Mutation-Id: invalid\r\n".into()),
        (
            Some(204),
            format!("X-Workspace-Mutation-Id: {RECEIPT}\r\nX-Workspace-Mutation-Id: {RECEIPT}\r\n"),
        ),
    ];
    let mut steps = Vec::new();
    for (status, headers) in result_cases {
        steps.extend([
            stream(USER, false),
            topic(USER),
            message(USER, USER),
            Step::delete(status, headers),
        ]);
    }
    let server = Server::start(steps).await;
    let (fixture, owner, generation, source) = open(&server, USER);
    let delete = || source.delete_message(&fixture.auth, &owner, generation, target(false));
    assert!(matches!(delete().await, Err(DeleteError::Forbidden)));
    assert!(matches!(
        delete().await,
        Err(DeleteError::MessageUnavailable)
    ));
    assert!(matches!(delete().await, Err(DeleteError::Rejected(429))));
    for _ in 0..5 {
        assert!(matches!(delete().await, Err(DeleteError::Unknown)));
    }
    server.verify().await;
}

/// Проверяет смену поколения после проверки строки и после отправки DELETE.
#[tokio::test]
async fn message_delete_discards_responses_from_ended_sessions() {
    for after_delete in [false, true] {
        let (received, sent) = oneshot::channel();
        let (resume, paused) = oneshot::channel();
        let mut steps = vec![stream(USER, false), topic(USER), message(USER, USER)];
        if after_delete {
            steps.push(Step::delete(
                Some(204),
                format!("X-Workspace-Mutation-Id: {RECEIPT}\r\n"),
            ));
        }
        steps.last_mut().unwrap().paused = Some((received, paused));
        let server = Server::start(steps).await;
        let (fixture, owner, generation, source) = open(&server, USER);
        let auth = fixture.auth.clone();
        let account = owner.account_id.clone();
        let operation = tokio::spawn(async move {
            source
                .delete_message(&auth, &owner, generation, target(false))
                .await
        });
        timeout(Duration::from_secs(3), sent)
            .await
            .expect("сервер не получил задержанный запрос")
            .unwrap();
        fixture.auth.logout(&account).unwrap();
        let _new = fixture.login_at(&server.origin, USER);
        resume.send(()).unwrap();
        assert!(matches!(
            operation.await.unwrap(),
            Err(DeleteError::Auth(AuthError::SessionUnavailable))
        ));
        server.verify().await;
    }
}
