//! Проверяет отправку границы прочтения через локальный HTTPS и смену входа.

use std::sync::Arc;

use rustls::{
    ServerConfig,
    crypto::ring,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::Notify,
    time::{Duration, timeout},
};
use tokio_rustls::TlsAcceptor;
use uuid::Uuid;
use workspace_core::{
    auth::AuthError,
    messenger::{MessengerSource, read::ReadError},
    storage::{Cursor, HistoryScope, Storage},
    sync::{Event, Sync},
};

use super::Fixture;

const CA: &[u8] =
    include_bytes!("../../../workspace-desktop/src/shell/background_message/tls/ca.pem");
const CERT: &[u8] =
    include_bytes!("../../../workspace-desktop/src/shell/background_message/tls/localhost.pem");
const KEY: &[u8] =
    include_bytes!("../../../workspace-desktop/src/shell/background_message/tls/localhost-key.pem");

/// Проверяет ответ, ошибки, отсутствие повтора и отказ старому поколению.
#[tokio::test]
async fn read_boundary_uses_current_session_and_checks_response() {
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
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (entered_server, release_server) = (entered.clone(), release.clone());
    let stream = Uuid::from_u128(10);
    let topic = Uuid::from_u128(11);
    let message = Uuid::from_u128(12);
    let worker = tokio::spawn(async move {
        for mode in 0..5 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(tls.accept(socket).await.unwrap());
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert_eq!(
                line,
                format!(
                    "POST /api/workspace/v1/messenger/messages/{message}/actions/read_up_to/invoke HTTP/1.1\r\n"
                )
            );
            let mut authorized = false;
            loop {
                line.clear();
                socket.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if line.trim() == "authorization: Bearer second" {
                    authorized = true;
                }
            }
            assert!(authorized);
            if mode == 4 {
                entered_server.notify_one();
                release_server.notified().await;
            }
            let mut row = json!({"uuid":message,"stream_uuid":stream,"topic_uuid":topic,"project_id":"project","user_uuid":"second","author_uuid":message,"created_at":"2026-01-01T12:00:00Z","payload":{"kind":"markdown","content":"hello"}});
            if mode == 2 {
                row["topic_uuid"] = json!(Uuid::from_u128(99));
            }
            if mode == 3 {
                row = json!({});
            }
            let status = if mode == 1 { "403 Forbidden" } else { "200 OK" };
            let body = row.to_string();
            socket.get_mut().write_all(format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        assert!(
            timeout(Duration::from_millis(300), listener.accept())
                .await
                .is_err(),
            "лишний POST"
        );
    });
    let fixture = Fixture::new(|_, _| unreachable!());
    let owner = fixture.login_at(&server, "second");
    let generation = fixture.auth.snapshot()[0].generation;
    let sync = Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    );
    let cursor = Cursor {
        epoch_generation: "test".into(),
        epoch_version: 3,
    };
    sync.apply(
        &owner,
        generation,
        "messenger",
        Event::Checkpoint(cursor.clone()),
    )
    .unwrap();
    let source =
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap();
    let scope = HistoryScope {
        stream_id: stream,
        topic_id: Some(topic),
    };
    assert!(
        source
            .read_up_to(&sync, &owner, generation, scope, message)
            .await
            .is_ok()
    );
    assert!(matches!(
        source
            .read_up_to(&sync, &owner, generation, scope, message)
            .await,
        Err(ReadError::Http(403))
    ));
    for _ in 0..2 {
        assert!(matches!(
            source
                .read_up_to(&sync, &owner, generation, scope, message)
                .await,
            Err(ReadError::Unknown)
        ));
    }
    let request = source.read_up_to(&sync, &owner, generation, scope, message);
    let relogin = async {
        timeout(Duration::from_secs(3), entered.notified())
            .await
            .expect("POST не дошёл до сервера");
        fixture.auth.logout(&owner.account_id).unwrap();
        fixture.login_at(&server, "second");
        release.notify_one();
    };
    let (result, ()) = tokio::join!(request, relogin);
    assert!(matches!(
        result,
        Err(ReadError::Auth(AuthError::SessionUnavailable))
    ));
    assert!(matches!(
        source
            .read_up_to(&sync, &owner, generation, scope, message)
            .await,
        Err(ReadError::Auth(_))
    ));
    assert_eq!(sync.cursor(&owner, "messenger").unwrap(), Some(cursor));
    worker.await.unwrap();
}
