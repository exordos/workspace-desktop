//! Проверяет публичный запрос ссылки регистрации через локальный HTTPS-сервер.

use std::sync::Arc;

use rustls::{
    ServerConfig,
    crypto::ring,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    time::{Duration, timeout},
};
use tokio_rustls::TlsAcceptor;
use workspace_core::{messenger::MessengerSource, runtime::SourceError};

const CA: &[u8] = include_bytes!("../../../tests/fixtures/tls/ca.pem");
const CERT: &[u8] = include_bytes!("../../../tests/fixtures/tls/localhost.pem");
const KEY: &[u8] = include_bytes!("../../../tests/fixtures/tls/localhost-key.pem");
const TIMEOUT: Duration = Duration::from_secs(5);

/// Публичный GET не передаёт токены, принимает безопасную ссылку и останавливается на редиректе.
#[tokio::test]
async fn reads_advertised_registration_without_authentication_or_redirects() {
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
    let fixture = tokio::spawn(async move {
        for body in [
            r#"{"registration_url":" https://accounts.example.test/join?source=desktop#form "}"#,
            r#"{"registration_url":42}"#,
            r#"{"registration_url":"https://user:secret@accounts.example.test/join"}"#,
            r#"{"registration_url":"https://@accounts.example.test/join"}"#,
            r#"{"registration_url":"http://accounts.example.test/join"}"#,
            r#"{"registration_url":"//accounts.example.test/join"}"#,
            r#"{"branding":"Test Workspace"}"#,
            "",
        ] {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(tls.accept(socket).await.unwrap());
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert_eq!(
                line,
                "GET /api/workspace/v1/messenger/server_settings HTTP/1.1\r\n"
            );
            loop {
                line.clear();
                assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                assert!(line.split_once(':').is_none_or(|(name, _)| {
                    !name.eq_ignore_ascii_case("authorization")
                        && !name.eq_ignore_ascii_case("cookie")
                }));
            }
            let status = if body.is_empty() {
                "302 Found\r\nLocation: /private"
            } else {
                "200 OK"
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            );
            socket
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        }
        assert!(
            timeout(Duration::from_millis(300), listener.accept())
                .await
                .is_err()
        );
    });
    let client =
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap();
    let registration = || async {
        timeout(TIMEOUT, client.registration_url(&server))
            .await
            .expect("дождаться публичных настроек сервера")
    };
    assert_eq!(
        registration().await,
        Ok(Some(
            "https://accounts.example.test/join?source=desktop#form".into()
        )),
    );
    for _ in 0..6 {
        assert_eq!(registration().await, Ok(None));
    }
    assert_eq!(registration().await, Err(SourceError::InvalidResponse));
    timeout(TIMEOUT, fixture).await.unwrap().unwrap();
}
