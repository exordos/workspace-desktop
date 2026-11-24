//! Превращает внешние URN аватаров в URL и скачивает фото без данных входа Workspace.
//! Подключение разрешено только к публичному адресу, который выбрал проверенный DNS-запрос.

use std::{
    io,
    net::{IpAddr, ToSocketAddrs},
    sync::{Arc, OnceLock},
    time::Duration,
};

use reqwest::{
    Client, StatusCode, Url,
    dns::{Addrs, Name, Resolve, Resolving},
    header::ACCEPT,
    redirect::Policy,
};
use uuid::Uuid;

use crate::{messenger::details::DetailsError, runtime::SourceError};

use super::AvatarImage;

const GRAVATAR: &str = "https://secure.gravatar.com/avatar/";
static CLIENT: OnceLock<Result<Client, SourceError>> = OnceLock::new();

/// Принимает адрес из профиля или собирает адрес Gravatar из готового хеша или UUID.
pub(super) fn profile_url(reference: &str) -> Option<Url> {
    let (kind, target) = reference.strip_prefix("urn:")?.split_once(':')?;
    match kind {
        "url" => {
            if target
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '\\')
                || !target.split_once("://").is_some_and(|(scheme, _)| {
                    scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http")
                })
            {
                return None;
            }
            let url = Url::parse(target).ok()?;
            (url.host().is_some() && url.username().is_empty() && url.password().is_none())
                .then_some(url)
        }
        "gravatar" => {
            let hash = target.trim();
            (matches!(hash.len(), 32 | 64) && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then(|| gravatar(&hash.to_ascii_lowercase()))
                .flatten()
        }
        "gavatar" => {
            let user = Uuid::parse_str(target).ok().filter(|id| !id.is_nil())?;
            gravatar(&user.simple().to_string())
        }
        _ => None,
    }
}

/// Собирает точный адрес стандартного изображения Gravatar.
fn gravatar(hash: &str) -> Option<Url> {
    Url::parse(&format!("{GRAVATAR}{hash}?d=identicon&s=500")).ok()
}

/// Получает изображение отдельным HTTP-клиентом без прокси, cookies, токенов и редиректов.
pub(super) async fn download(mut url: Url) -> Result<AvatarImage, DetailsError> {
    let host = url.host_str().ok_or(SourceError::InvalidResponse)?;
    if !public_host(host) {
        return Err(DetailsError::ProfileUnavailable);
    }
    url.set_fragment(None);
    let client = CLIENT
        .get_or_init(client)
        .as_ref()
        .map_err(|error| *error)?;
    let response = client
        .get(url)
        .header(ACCEPT, "image/png, image/jpeg, image/gif, image/webp")
        .send()
        .await
        .map_err(|_| SourceError::Temporary)?;
    match response.status() {
        StatusCode::OK => Ok(AvatarImage::read_response(response).await?),
        StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => Err(DetailsError::ProfileUnavailable),
        _ => Err(SourceError::Temporary.into()),
    }
}

/// Создаёт соединения, для которых адрес DNS проверяется непосредственно перед подключением.
fn client() -> Result<Client, SourceError> {
    Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .referer(false)
        .dns_resolver(Arc::new(PublicDns))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| SourceError::Temporary)
}

/// Отбрасывает локальное имя и проверяет IP-литерал, для которого клиент пропускает DNS.
fn public_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).trim_end_matches('.');
    host.parse::<IpAddr>().map_or_else(
        |_| host.contains('.') && !host.ends_with(".localhost") && !host.ends_with(".local"),
        public_ip,
    )
}

/// DNS для внешних картинок. Передаёт клиенту только проверенные адреса одного ответа.
struct PublicDns;

impl Resolve for PublicDns {
    /// Исключает адреса локальной сети и служебные адреса до начала подключения.
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses = tokio::task::spawn_blocking(move || {
                (host.as_str(), 0).to_socket_addrs().map(|addresses| {
                    addresses
                        .filter(|address| public_ip(address.ip()))
                        .collect::<Vec<_>>()
                })
            })
            .await??;
            if addresses.is_empty() {
                return Err(io::Error::other("no public address for avatar host").into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

/// Принимает адрес внешней сети и исключает приватные, локальные и служебные диапазоны.
fn public_ip(address: IpAddr) -> bool {
    match address.to_canonical() {
        IpAddr::V4(address) => {
            let [first, second, third, _] = address.octets();
            !address.is_private()
                && !address.is_loopback()
                && !address.is_link_local()
                && !address.is_documentation()
                && !matches!(
                    (first, second, third),
                    (0 | 224..=255, _, _)
                        | (100, 64..=127, _)
                        | (192, 0, 0)
                        | (192, 88, 99)
                        | (198, 18 | 19, _)
                )
        }
        IpAddr::V6(address) => {
            let [first, second, ..] = address.segments();
            (0x2000..=0x3fff).contains(&first)
                && !matches!(
                    (first, second),
                    (0x2001, 0x0000..=0x01ff | 0x0db8) | (0x2002 | 0x3fff, _)
                )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Воспроизводит адреса Electron и принимает только известные внешние ссылки.
    #[test]
    fn resolves_legacy_and_current_avatar_urns() {
        let hash = "ABCDEF1234567890ABCDEF1234567890";
        assert_eq!(
            profile_url(&format!("urn:gravatar:{hash}"))
                .unwrap()
                .as_str(),
            format!("{GRAVATAR}{}?d=identicon&s=500", hash.to_ascii_lowercase())
        );
        assert_eq!(
            profile_url("urn:gavatar:abcdef12-3456-7890-abcd-ef1234567890")
                .unwrap()
                .as_str(),
            format!("{GRAVATAR}{}?d=identicon&s=500", hash.to_ascii_lowercase())
        );
        let url = "https://images.example.test/photo.png?size=500";
        assert_eq!(
            profile_url(&format!("urn:url:{url}")).unwrap().as_str(),
            url
        );
        for invalid in [
            "urn:url:http://user@host.test/avatar",
            "urn:url:data:image/png",
            "urn:gravatar:arbitrary",
            "urn:gavatar:00000000-0000-0000-0000-000000000000",
        ] {
            assert!(profile_url(invalid).is_none(), "{invalid}");
        }
    }

    /// Блокирует IP-литералы и локальные имена до сети, включая IPv4, записанный как IPv6.
    #[test]
    fn blocks_non_public_avatar_destinations() {
        for address in [
            "localhost",
            "local.localhost",
            "printer.local",
            "127.0.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.100.100.200",
            "[::1]",
            "[::ffff:127.0.0.1]",
            "[fe80::1]",
            "[fd00::1]",
            "[2002:7f00:1::1]",
        ] {
            assert!(!public_host(address), "{address}");
        }
        for address in ["8.8.8.8", "[2606:4700:4700::1111]", "secure.gravatar.com"] {
            assert!(public_host(address), "{address}");
        }
    }
}
