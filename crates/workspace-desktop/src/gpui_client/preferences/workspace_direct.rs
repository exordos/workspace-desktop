//! Хранит настройку прямого подключения и выбирает сетевой интерфейс macOS при запуске.

use std::{io, path::Path, ptr};

use gpui::App;
use system_configuration::{
    core_foundation::{
        array::CFArray,
        base::{CFType, TCFType},
        dictionary::CFDictionary,
        string::CFString,
    },
    dynamic_store::SCDynamicStore,
    sys::dynamic_store::SCDynamicStoreCreate,
};
use workspace_core::workspace_direct::WorkspaceRoute;

use super::{FILE, Preferences, read_file};

/// Читает общий переключатель независимо от активного аккаунта.
pub(in crate::gpui_client) fn enabled(app: &App) -> bool {
    app.try_global::<Preferences>()
        .and_then(|store| store.values.get(&None))
        .and_then(|values| values.workspace_direct)
        .unwrap_or_default()
}

/// Атомарно сохраняет обратное значение в общую запись профиля.
pub(in crate::gpui_client) fn toggle(app: &mut App) -> io::Result<()> {
    let enabled = !enabled(app);
    let mut values = app
        .try_global::<Preferences>()
        .map(|store| store.values.clone())
        .unwrap_or_default();
    values.entry(None).or_default().workspace_direct = enabled.then_some(true);
    Preferences::save(app, values)
}

/// Восстанавливает маршрут для HTTPS и WebSocket из того же файла, что читает окно настроек.
pub(in crate::gpui_client) fn load_route(profile: &Path) -> WorkspaceRoute {
    if !saved_enabled(profile) {
        return WorkspaceRoute::default();
    }
    if let Some(interface) = preferred_interface()
        && let Ok(route) = WorkspaceRoute::direct(&interface)
    {
        log::info!("event=workspace_direct_route result=enabled interface={interface}");
        return route;
    }
    log::warn!("event=workspace_direct_route result=interface_unavailable route=system");
    WorkspaceRoute::default()
}

/// Читает сохранённый переключатель без доступа к открытому окну.
fn saved_enabled(profile: &Path) -> bool {
    read_file(&profile.join(FILE))
        .get(&None)
        .and_then(|values| values.workspace_direct)
        .unwrap_or_default()
}

/// Читает текущую конфигурацию macOS для выбора интерфейса.
fn preferred_interface() -> Option<String> {
    let store = open_store()?;
    select_interface(|path| network_state(&store, path))
}

/// Выбирает основной физический интерфейс или активный сервис в порядке macOS.
fn select_interface(
    state_at: impl Fn(&str) -> Option<CFDictionary<CFString, CFType>>,
) -> Option<String> {
    for family in ["IPv4", "IPv6"] {
        if let Some(interface) = state_at(&format!("State:/Network/Global/{family}"))
            .and_then(|state| physical_interface(&state, "PrimaryInterface"))
        {
            return Some(interface);
        }
    }

    // Основным может быть VPN. Тогда ищем Wi-Fi или Ethernet с маршрутизатором.
    let setup = state_at("Setup:/Network/Global/IPv4")?;
    let order = setup
        .find(CFString::new("ServiceOrder"))?
        .downcast::<CFArray>()?;
    // Элементы массива SystemConfiguration являются объектами CF. Тип каждого ID проверяем ниже.
    let order = unsafe { CFArray::<CFType>::wrap_under_get_rule(order.as_concrete_TypeRef()) };
    for service in order
        .iter()
        .filter_map(|value| value.downcast::<CFString>())
    {
        for family in ["IPv4", "IPv6"] {
            let key = format!("State:/Network/Service/{service}/{family}");
            if let Some(state) = state_at(&key)
                && state.find(CFString::new("Router")).is_some()
                && let Some(interface) = physical_interface(&state, "InterfaceName")
            {
                return Some(interface);
            }
        }
    }
    None
}

/// Открывает только чтение сетевого состояния и допускает отказ SystemConfiguration.
fn open_store() -> Option<SCDynamicStore> {
    let name = CFString::new("Workspace Desktop");
    // Имя живёт до конца вызова. Системное хранилище не использует callback или контекст.
    let store = unsafe {
        SCDynamicStoreCreate(
            ptr::null(),
            name.as_concrete_TypeRef(),
            None,
            ptr::null_mut(),
        )
    };
    // Правило Create передаёт приложению ссылку, которую освободит обёртка.
    (!store.is_null()).then(|| unsafe { SCDynamicStore::wrap_under_create_rule(store) })
}

/// Читает целиком запись сетевого сервиса или состояния системы.
fn network_state(store: &SCDynamicStore, path: &str) -> Option<CFDictionary<CFString, CFType>> {
    let dictionary = store.get(path)?.downcast_into::<CFDictionary>()?;
    // У словаря из CFPropertyList строковые ключи и значения CF произвольного типа.
    Some(unsafe {
        CFDictionary::<CFString, CFType>::wrap_under_get_rule(dictionary.as_concrete_TypeRef())
    })
}

/// Принимает имена сетевых интерфейсов Wi-Fi и Ethernet, которые macOS задаёт как enN.
fn physical_interface(state: &CFDictionary<CFString, CFType>, key: &str) -> Option<String> {
    let name = state
        .find(CFString::new(key))?
        .downcast::<CFString>()?
        .to_string();
    let number = name.strip_prefix("en")?;
    (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// При основном маршруте через VPN выбирает первый физический сервис с маршрутизатором.
    #[test]
    fn skips_vpn_and_offline_interfaces() {
        let services = ["vpn", "offline", "wifi", "ethernet"].map(CFString::new);
        let order = CFArray::from_CFTypes(&services);
        let interface = select_interface(|key| {
            let values = match key {
                "State:/Network/Global/IPv4" => vec![("PrimaryInterface", "utun4")],
                "Setup:/Network/Global/IPv4" => {
                    return Some(CFDictionary::from_CFType_pairs(&[(
                        CFString::new("ServiceOrder"),
                        order.as_CFType(),
                    )]));
                }
                "State:/Network/Service/vpn/IPv4" => {
                    vec![("InterfaceName", "utun4"), ("Router", "10.0.0.1")]
                }
                "State:/Network/Service/offline/IPv4" => vec![("InterfaceName", "en2")],
                "State:/Network/Service/wifi/IPv6" => {
                    vec![("InterfaceName", "en0"), ("Router", "fe80::1")]
                }
                "State:/Network/Service/ethernet/IPv4" => {
                    vec![("InterfaceName", "en1"), ("Router", "192.0.2.1")]
                }
                _ => return None,
            };
            let values = values
                .into_iter()
                .map(|(key, value)| (CFString::new(key), CFString::new(value).as_CFType()))
                .collect::<Vec<_>>();
            Some(CFDictionary::from_CFType_pairs(&values))
        });
        assert_eq!(interface.as_deref(), Some("en0"));
    }

    /// Читает сохранённый выбор после открытия нового профиля в окне и воркере.
    #[cfg(feature = "gpui-test")]
    #[gpui::test]
    fn persists_direct_switch_across_restarts(cx: &mut gpui::TestAppContext) {
        let profile = tempfile::tempdir().unwrap();
        let directory = profile.path();
        assert!(!saved_enabled(directory));
        cx.update(|app| {
            Preferences::install(directory, app);
            assert!(!enabled(app));
            toggle(app).unwrap();
            assert!(enabled(app));
        });
        assert!(saved_enabled(directory));
        cx.update(|app| {
            Preferences::install(directory, app);
            assert!(enabled(app));
            toggle(app).unwrap();
            assert!(!enabled(app));
        });
        assert!(!saved_enabled(directory));
    }
}
