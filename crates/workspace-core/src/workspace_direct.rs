//! Экспериментальный прямой маршрут к серверу Workspace на macOS.
//! Настраивает HTTPS и открывает TCP для WSS через выбранный сетевой интерфейс.

use tokio::net::TcpStream;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{
        Error, handshake::client::Request, handshake::client::Response, protocol::WebSocketConfig,
    },
};

#[cfg(target_os = "macos")]
use std::{ffi::CString, io, net::SocketAddr, num::NonZeroU32};

/// Выбирает системный маршрут или явно заданный интерфейс для соединений Workspace.
#[derive(Clone, Default)]
pub enum WorkspaceRoute {
    #[default]
    System, // Ядро ОС выбирает маршрут по своим настройкам.
    #[cfg(target_os = "macos")]
    Direct(DirectInterface), // Сокеты закреплены за выбранным интерфейсом.
}

/// Хранит проверенное имя интерфейса для HTTPS и WSS.
#[cfg(target_os = "macos")]
#[derive(Clone)]
pub struct DirectInterface(String); // Имя интерфейса macOS, например `en0`.

impl WorkspaceRoute {
    /// Включает прямой режим, если указанный сетевой интерфейс существует.
    #[cfg(target_os = "macos")]
    pub fn direct(interface: &str) -> io::Result<Self> {
        interface_index(interface)?;
        Ok(Self::Direct(DirectInterface(interface.to_owned())))
    }

    /// Привязывает HTTPS-клиент мессенджера к интерфейсу в прямом режиме.
    pub(crate) fn messenger_https(
        &self,
        builder: reqwest::ClientBuilder,
    ) -> reqwest::ClientBuilder {
        #[cfg(target_os = "macos")]
        if let Self::Direct(interface) = self {
            return builder.no_proxy().interface(&interface.0);
        }
        builder
    }

    /// Привязывает блокирующий HTTPS-клиент авторизации к тому же интерфейсу.
    pub(crate) fn iam_https(
        &self,
        builder: reqwest::blocking::ClientBuilder,
    ) -> reqwest::blocking::ClientBuilder {
        #[cfg(target_os = "macos")]
        if let Self::Direct(interface) = self {
            return builder.no_proxy().interface(&interface.0);
        }
        builder
    }

    /// Открывает WSS выбранным маршрутом, сохраняя исходный адрес для проверки TLS.
    pub(crate) async fn websocket(
        &self,
        request: Request,
        config: WebSocketConfig,
        connector: Option<Connector>,
    ) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), Error> {
        #[cfg(target_os = "macos")]
        if let Self::Direct(interface) = self {
            let stream = interface.connect(&request).await?;
            return tokio_tungstenite::client_async_tls_with_config(
                request,
                stream,
                Some(config),
                connector,
            )
            .await;
        }
        connect_async_tls_with_config(request, Some(config), false, connector).await
    }
}

#[cfg(target_os = "macos")]
impl DirectInterface {
    /// Пробует адреса сервера через выбранный интерфейс и возвращает первое успешное соединение.
    async fn connect(&self, request: &Request) -> io::Result<TcpStream> {
        use futures::{StreamExt, stream::FuturesUnordered};

        let host = request
            .uri()
            .host()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Не указан сервер WSS"))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let port = request.uri().port_u16().unwrap_or(443);
        let interface = interface_index(&self.0)?;
        let addresses = tokio::net::lookup_host((host, port)).await?;
        // Недоступный IPv6 не должен задерживать подключение по рабочему IPv4.
        let mut attempts = addresses
            .map(|address| connect_bound(address, interface))
            .collect::<FuturesUnordered<_>>();
        let mut last_error = io::Error::new(io::ErrorKind::NotFound, "DNS не вернул адрес сервера");
        while let Some(result) = attempts.next().await {
            match result {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }
}

/// Получает индекс интерфейса из текущей конфигурации macOS.
#[cfg(target_os = "macos")]
fn interface_index(interface: &str) -> io::Result<NonZeroU32> {
    if interface.is_empty()
        || interface.len() >= libc::IFNAMSIZ
        || !interface.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Неверное имя интерфейса",
        ));
    }
    let name = CString::new(interface)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Неверное имя интерфейса"))?;
    // CString держит корректную строку C в памяти на время системного вызова.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    NonZeroU32::new(index)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Сетевой интерфейс не найден"))
}

/// Закрепляет сокет за интерфейсом до соединения с сервером.
#[cfg(target_os = "macos")]
async fn connect_bound(address: SocketAddr, interface: NonZeroU32) -> io::Result<TcpStream> {
    use socket2::SockRef;
    use tokio::net::TcpSocket;

    let socket = match address {
        SocketAddr::V4(_) => {
            let socket = TcpSocket::new_v4()?;
            SockRef::from(&socket).bind_device_by_index_v4(Some(interface))?;
            socket
        }
        SocketAddr::V6(_) => {
            let socket = TcpSocket::new_v6()?;
            SockRef::from(&socket).bind_device_by_index_v6(Some(interface))?;
            socket
        }
    };
    socket.set_nodelay(true)?;
    socket.connect(address).await
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// Ошибочный интерфейс отклоняется при включении режима.
    #[test]
    fn rejects_invalid_interface() {
        assert!(WorkspaceRoute::direct("").is_err());
        assert!(WorkspaceRoute::direct("wpNoDevice99").is_err());
    }

    /// Фиксирует интерфейс исходящего TCP для обоих семейств адресов.
    #[tokio::test]
    async fn connects_through_selected_interface() {
        use socket2::SockRef;
        use std::time::Duration;
        use tokio::net::TcpListener;
        use tokio::time::timeout;

        let WorkspaceRoute::Direct(interface) = WorkspaceRoute::direct("lo0").unwrap() else {
            panic!("ожидался прямой маршрут");
        };
        let expected = interface_index("lo0").unwrap();
        for address in ["127.0.0.1:0", "[::1]:0"] {
            let listener = TcpListener::bind(address).await.unwrap();
            let address = listener.local_addr().unwrap();
            let request = Request::builder()
                .uri(format!("wss://{address}/events"))
                .body(())
                .unwrap();
            let client = timeout(Duration::from_secs(3), interface.connect(&request))
                .await
                .unwrap()
                .unwrap();
            let socket = SockRef::from(&client);
            let bound = if address.is_ipv4() {
                socket.device_index_v4()
            } else {
                socket.device_index_v6()
            };
            assert_eq!(bound.unwrap(), Some(expected));
        }
    }
}
