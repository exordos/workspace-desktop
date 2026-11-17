//! Проверяет пограничные ответы API папок и формы команд через локальный HTTPS.

use std::{sync::Arc, time::Duration};

use rustls::{
    ServerConfig,
    crypto::ring,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;

use crate::context::AccountId;

use super::*;

const CA: &[u8] = include_bytes!("../../../../../../tests/fixtures/tls/ca.pem");
const CERT: &[u8] = include_bytes!("../../../../../../tests/fixtures/tls/localhost.pem");
const KEY: &[u8] = include_bytes!("../../../../../../tests/fixtures/tls/localhost-key.pem");
const OWNER: FolderOwner = FolderOwner {
    project_id: Uuid::from_u128(31),
    user_id: Uuid::from_u128(32),
};
const CUSTOM: Uuid = Uuid::from_u128(40);
const STREAM: Uuid = Uuid::from_u128(50);
const ITEM: Uuid = Uuid::from_u128(60);
const NOW: &str = "2026-01-01T09:30:00";

/// Создаёт ответ папки с отсутствующими полями владельца, как на стенде.
fn folder(id: Uuid, kind: &str, items: Vec<Value>) -> Value {
    json!({
        "uuid": id, "title": "test", "system_type": kind,
        "unread_count": 2, "created_at": format!("{NOW}Z"), "updated_at": format!("{NOW}Z"),
        "folder_items": items,
    })
}

/// Создаёт связь с явным проектом, пользователем и тремя счётчиками.
fn item(parent: Uuid) -> Value {
    json!({
        "uuid": ITEM, "project_id": OWNER.project_id, "user_uuid": OWNER.user_id,
        "folder_uuid": parent, "stream_uuid": STREAM, "chat_type": "group",
        "unread_count": 5, "active_unread_count": 2, "passive_unread_count": 3,
        "created_at": format!("{NOW}Z"), "updated_at": format!("{NOW}Z"),
    })
}

/// Нормализует коллекцию тем же путём, который использует HTTP.
fn decode(rows: Vec<Value>) -> Result<Vec<Folder>, SourceError> {
    normalize_folders(
        serde_json::from_value(Value::Array(rows)).map_err(|_| SourceError::InvalidResponse)?,
        OWNER,
    )
}

/// Системный UUID уточняет тип, одинаковые повторы не размножают связи и папки.
#[test]
fn normalizes_system_folders_and_identical_page_overlap() {
    let id = Uuid::from_u128(1);
    let mut row = item(id);
    row.as_object_mut().unwrap().remove("folder_uuid");
    row["folder"] = json!(id);
    row["pinned_at"] = json!(NOW);
    let personal = folder(id, "all", vec![row.clone(), row]);
    let rows = decode(vec![
        personal.clone(),
        personal,
        folder(CUSTOM, "created", vec![]),
    ])
    .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].kind, FolderKind::Personal);
    assert_eq!(rows[0].items.len(), 1);
    assert_eq!(rows[0].items[0].id, ITEM);
    assert_eq!(
        rows[0].items[0].pinned_at.as_deref(),
        Some("2026-01-01T09:30:00.000000Z")
    );
    assert_eq!(rows[0].items[0].active_unread_count, 2);
    assert_eq!(rows[0].items[0].passive_unread_count, 3);
    assert_eq!(rows[1].kind, FolderKind::Custom);
    assert!(rows[1].items.is_empty());
}

/// Пропущенный состав, чужая связь и противоречивые версии UUID не считаются снимком.
#[test]
fn rejects_partial_or_conflicting_folder_collections() {
    let row = folder(CUSTOM, "created", vec![]);
    let mut partial = row.clone();
    partial.as_object_mut().unwrap().remove("folder_items");
    let mut different = row.clone();
    different["unread_count"] = json!(9);
    let mut cross_owner = item(CUSTOM);
    cross_owner["user_uuid"] = json!(Uuid::from_u128(33));
    let mut wrong_alias = item(CUSTOM);
    wrong_alias["folder"] = json!(Uuid::from_u128(2));
    let conflicting_collections = [
        vec![partial],
        vec![row, different],
        vec![folder(CUSTOM, "created", vec![cross_owner])],
        vec![folder(CUSTOM, "created", vec![wrong_alias])],
        vec![
            folder(CUSTOM, "created", vec![item(CUSTOM)]),
            folder(Uuid::from_u128(2), "all", vec![item(Uuid::from_u128(2))]),
        ],
    ];
    for rows in conflicting_collections {
        assert_eq!(decode(rows).unwrap_err(), SourceError::InvalidResponse);
    }
}

/// Ожидаемый запрос и ответ. Отсутствующий ответ имитирует обрыв после записи.
struct Exchange {
    start_line: String,                      // Строка запроса с методом и путём.
    body: Option<Value>,                     // JSON, который должен отправить клиент.
    response: Option<(&'static str, Value)>, // Статус и JSON или закрытие соединения.
}

/// Проверяет, что запрос уходит своему владельцу и на сервер приходит ожидаемая команда.
async fn exchange(tls: &TlsAcceptor, listener: &TcpListener, step: Exchange) {
    let (socket, _) = listener.accept().await.unwrap();
    let mut socket = BufReader::new(tls.accept(socket).await.unwrap());
    let mut line = String::new();
    socket.read_line(&mut line).await.unwrap();
    assert_eq!(line.trim(), format!("{} HTTP/1.1", step.start_line));
    let mut length = 0;
    let mut authorized = false;
    loop {
        line.clear();
        assert!(socket.read_line(&mut line).await.unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        let name = line.to_ascii_lowercase();
        if name.starts_with("content-length:") {
            length = line.split_once(':').unwrap().1.trim().parse().unwrap();
        }
        authorized |= line.trim() == "authorization: Bearer owner-token";
    }
    assert!(authorized);
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes).await.unwrap();
    match step.body {
        Some(expected) => assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), expected),
        None => assert!(bytes.is_empty()),
    }
    if let Some((status, body)) = step.response {
        let body = body.to_string();
        let reply = format!(
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        socket.get_mut().write_all(reply.as_bytes()).await.unwrap();
        socket.get_mut().shutdown().await.unwrap();
    }
}

/// Принимает прямой ответ, полный снимок и обрыв без повтора запроса.
#[tokio::test]
async fn folder_commands_send_once_and_accept_both_item_response_shapes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let owner = OwnerContext {
        account_id: AccountId("test".into()),
        server: format!("https://{}", listener.local_addr().unwrap()),
        organization_id: "org".into(),
        project_id: OWNER.project_id.to_string(),
        user_id: OWNER.user_id.to_string(),
    };
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
    let client =
        RestClient::new(Some(CertificateDer::from_pem_slice(CA).unwrap().as_ref())).unwrap();
    let create_body = json!({"uuid": CUSTOM, "title": "test", "background_color_value": null});
    let expected_body = create_body.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            exchange(
                &tls,
                &listener,
                Exchange {
                    start_line: format!("POST {FOLDERS_PATH}"),
                    body: Some(expected_body.clone()),
                    response: Some(("201 Created", folder(CUSTOM, "created", vec![]))),
                },
            )
            .await;
        }
        exchange(
            &tls,
            &listener,
            Exchange {
                start_line: format!("POST {ITEMS_PATH}"),
                body: Some(
                    json!({"folder_uuid": CUSTOM, "stream_uuid": STREAM, "chat_type": "group"}),
                ),
                response: Some(("201 Created", item(CUSTOM))),
            },
        )
        .await;
        let mut pinned = item(CUSTOM);
        pinned["pinned_at"] = json!(NOW);
        exchange(
            &tls,
            &listener,
            Exchange {
                start_line: format!("POST {ITEMS_PATH}{ITEM}/actions/pin/invoke"),
                body: None,
                response: Some(("200 OK", folder(CUSTOM, "created", vec![pinned]))),
            },
        )
        .await;
        exchange(
            &tls,
            &listener,
            Exchange {
                start_line: format!("POST {ITEMS_PATH}{ITEM}/actions/unpin/invoke"),
                body: None,
                response: None,
            },
        )
        .await;
        assert!(
            timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "неожиданный повтор"
        );
    });

    let token = "owner-token";
    for _ in 0..2 {
        assert_eq!(
            client
                .create_folder(&owner, token, CUSTOM, create_body.clone())
                .await
                .unwrap(),
            CUSTOM
        );
    }
    assert_eq!(
        client
            .add_folder_chat(&owner, token, CUSTOM, STREAM, ChatType::Group)
            .await
            .unwrap(),
        ITEM
    );
    let row: WireFolderItem = serde_json::from_value(item(CUSTOM)).unwrap();
    let item = row.into_item(OWNER, CUSTOM).unwrap();
    client
        .set_folder_item_pinned(&owner, token, CUSTOM, &item, true)
        .await
        .unwrap();
    assert!(matches!(
        client
            .set_folder_item_pinned(&owner, token, CUSTOM, &item, false)
            .await,
        Err(FolderError::Unknown)
    ));
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}
