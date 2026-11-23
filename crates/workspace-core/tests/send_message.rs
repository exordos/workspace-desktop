//! Проверяет одну отправку подготовленного сообщения через управляемый HTTPS-сервер.

use rustls::{
    ServerConfig,
    crypto::ring,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::mpsc,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use uuid::Uuid;
use workspace_core::{
    context::{AccountId, OwnerContext},
    messenger::{MessengerSource, send::SendError},
    storage::{PreparedSend, PreparedSendBody},
};

const CA: &[u8] = include_bytes!("../../../tests/fixtures/tls/ca.pem");
const CERT: &[u8] = include_bytes!("../../../tests/fixtures/tls/localhost.pem");
const KEY: &[u8] = include_bytes!("../../../tests/fixtures/tls/localhost-key.pem");

/// Принимает запросы и по очереди возвращает успех, ошибку, отказ и обрыв.
#[tokio::test]
async fn sends_once_and_distinguishes_unknown_outcomes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("https://{}", listener.local_addr().unwrap());
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
    let canonical = Uuid::from_u128(1);
    let topic = Uuid::from_u128(2);
    let stream = Uuid::from_u128(3);
    let public = Uuid::new_v5(&topic, canonical.to_string().as_bytes());
    let author = Uuid::from_u128(4);
    let snapshot = PreparedSend {
        canonical_id: canonical,
        public_id: public,
        stream_id: stream,
        topic_id: topic,
        body: PreparedSendBody::Markdown("Привет **мир**\n".into()),
    };
    let expected = snapshot.clone();
    let (observed, mut requests) = mpsc::unbounded_channel();
    let worker = tokio::spawn(async move {
        for mode in 0..7 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(tls.accept(socket).await.unwrap());
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert_eq!(
                line,
                "POST /api/workspace/v1/messenger/messages/ HTTP/1.1\r\n"
            );
            let mut length = 0;
            let mut authorized = false;
            loop {
                line.clear();
                socket.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if lower.starts_with("content-length:") {
                    length = line.split_once(':').unwrap().1.trim().parse().unwrap();
                }
                if line.trim() == "authorization: Bearer owner-token" {
                    authorized = true;
                }
            }
            assert!(authorized);
            let mut bytes = vec![0; length];
            socket.read_exact(&mut bytes).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                body,
                json!({"uuid": expected.canonical_id, "stream_uuid": expected.stream_id, "topic_uuid": expected.topic_id, "payload": {"kind":"markdown", "content": expected.body.markdown().unwrap()}})
            );
            observed.send(()).unwrap();
            if mode == 6 {
                continue;
            }
            let (status, body) = match mode {
                0 => (
                    "201 Created",
                    json!({"uuid": public, "stream_uuid": stream, "topic_uuid": topic, "project_id":"project", "user_uuid": author.to_string(), "author_uuid": author, "created_at":"2026-01-01T12:00:00Z", "payload":{"kind":"markdown", "content":expected.body.markdown().unwrap()}}),
                ),
                1 => ("201 Created", json!({"uuid": public})),
                2 => ("400 Bad Request", json!({})),
                4 => ("408 Request Timeout", json!({})),
                5 => (
                    "201 Created",
                    json!({"uuid": Uuid::from_u128(999), "stream_uuid": stream, "topic_uuid": topic, "author_uuid": author, "created_at":"2026-01-01T12:00:00Z", "payload":{"kind":"markdown", "content":expected.body.markdown().unwrap()}}),
                ),
                _ => ("500 Internal Server Error", json!({})),
            };
            let body = body.to_string();
            socket.get_mut().write_all(format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            socket.get_mut().shutdown().await.unwrap();
        }
        // Слушатель остаётся открытым: повторный запрос был бы виден тесту.
        if let Ok(Ok(_)) = timeout(Duration::from_millis(300), listener.accept()).await {
            panic!("неожиданный повтор POST");
        }
    });
    let owner = OwnerContext {
        account_id: AccountId("account".into()),
        server,
        organization_id: "org".into(),
        project_id: "project".into(),
        user_id: author.to_string(),
    };
    let source =
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap();
    let mut invalid = snapshot.clone();
    invalid.public_id = Uuid::nil();
    assert_eq!(
        source
            .post_prepared_message(&owner, "owner-token", &invalid)
            .await
            .unwrap_err(),
        SendError::InvalidInput
    );
    let result = source
        .post_prepared_message(&owner, "owner-token", &snapshot)
        .await
        .unwrap();
    assert_eq!(
        (
            result.id,
            result.stream_id,
            result.topic_id,
            result.body.markdown()
        ),
        (public, stream, topic, snapshot.body.markdown())
    );
    assert_eq!(
        source
            .post_prepared_message(&owner, "owner-token", &snapshot)
            .await
            .unwrap_err(),
        SendError::Unknown
    );
    assert_eq!(
        source
            .post_prepared_message(&owner, "owner-token", &snapshot)
            .await
            .unwrap_err(),
        SendError::Rejected(400)
    );
    assert_eq!(
        source
            .post_prepared_message(&owner, "owner-token", &snapshot)
            .await
            .unwrap_err(),
        SendError::Unknown
    );
    assert_eq!(
        source
            .post_prepared_message(&owner, "owner-token", &snapshot)
            .await
            .unwrap_err(),
        SendError::Unknown
    );
    for _ in 0..2 {
        assert_eq!(
            source
                .post_prepared_message(&owner, "owner-token", &snapshot)
                .await
                .unwrap_err(),
            SendError::Unknown
        );
    }
    for _ in 0..7 {
        timeout(Duration::from_secs(3), requests.recv())
            .await
            .unwrap()
            .unwrap();
    }
    worker.await.unwrap();
}

/// Проверяет чтение подтверждения и неопределённые ответы без повторной отправки.
#[tokio::test]
async fn lookup_confirms_only_matching_delivery() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("https://{}", listener.local_addr().unwrap());
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
    let canonical = Uuid::from_u128(1);
    let topic = Uuid::from_u128(2);
    let stream = Uuid::from_u128(3);
    let author = Uuid::from_u128(4);
    let public = Uuid::new_v5(&topic, canonical.to_string().as_bytes());
    let snapshot = PreparedSend {
        canonical_id: canonical,
        public_id: public,
        stream_id: stream,
        topic_id: topic,
        body: PreparedSendBody::Markdown("hello".into()),
    };
    let worker = tokio::spawn(async move {
        for mode in 0..10 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(tls.accept(socket).await.unwrap());
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert_eq!(
                line,
                format!("GET /api/workspace/v1/messenger/messages/{public} HTTP/1.1\r\n")
            );
            let mut authorized = false;
            loop {
                line.clear();
                socket.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if line.trim() == "authorization: Bearer owner-token" {
                    authorized = true;
                }
            }
            assert!(authorized);
            if mode == 8 {
                continue;
            }
            let mut body = json!({"uuid": public, "stream_uuid": stream, "topic_uuid": topic, "project_id":"project", "user_uuid":author, "author_uuid":author, "created_at":"2026-01-01T12:00:00Z", "payload":{"kind":"markdown", "content":"hello"}});
            match mode {
                2 => body["stream_uuid"] = json!(Uuid::from_u128(99)),
                3 => body["topic_uuid"] = json!(Uuid::from_u128(99)),
                4 => body["author_uuid"] = json!(Uuid::from_u128(99)),
                5 => body["uuid"] = json!(Uuid::from_u128(99)),
                6 => body = json!({}),
                9 => body["project_id"] = json!("other-project"),
                _ => {}
            }
            let status = match mode {
                1 => "404 Not Found",
                7 => "403 Forbidden",
                _ => "200 OK",
            };
            let body = body.to_string();
            socket.get_mut().write_all(format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            socket.get_mut().shutdown().await.unwrap();
        }
        if let Ok(Ok(_)) = timeout(Duration::from_millis(300), listener.accept()).await {
            panic!("неожиданный дополнительный запрос");
        }
    });
    let owner = OwnerContext {
        account_id: AccountId("account".into()),
        server,
        organization_id: "org".into(),
        project_id: "project".into(),
        user_id: author.to_string(),
    };
    let source =
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap();
    let found = source
        .lookup_prepared_message(&owner, "owner-token", &snapshot)
        .await
        .unwrap();
    assert_eq!(
        (
            found.id,
            found.stream_id,
            found.topic_id,
            found.body.markdown()
        ),
        (public, stream, topic, Some("hello"))
    );
    for _ in 1..10 {
        assert_eq!(
            source
                .lookup_prepared_message(&owner, "owner-token", &snapshot)
                .await
                .unwrap_err(),
            SendError::Unknown
        );
    }
    worker.await.unwrap();
}
