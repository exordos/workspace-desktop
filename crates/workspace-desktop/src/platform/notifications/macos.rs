//! Подключает центр уведомлений macOS, отправляет баннер и передаёт оболочке нажатие.
//! Цели баннеров хранятся в памяти процесса, а macOS получает только идентификатор запроса.

use std::collections::VecDeque;
use std::future::Future;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::{Block, RcBlock};
use futures::channel::{mpsc, oneshot};
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{ClassType, DeclaredClass, declare_class, mutability};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString, NSURL, NSUUID};
use objc2_user_notifications::{
    UNAlertStyle, UNAuthorizationOptions, UNAuthorizationStatus, UNErrorCode, UNErrorDomain,
    UNMutableNotificationContent, UNNotification, UNNotificationDefaultActionIdentifier,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSetting, UNNotificationSettings, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use super::{NotificationClick, NotificationError, NotificationPermission};

const RECENT_NOTIFICATIONS: usize = 128;
const CLICK_BUFFER: usize = 32;

/// Соединение с центром уведомлений и обработчик, который живёт вместе с ним.
pub(super) struct Center {
    center: Retained<UNUserNotificationCenter>, // Общий центр уведомлений приложения.
    _delegate: Retained<NotificationDelegate>,  // Центр держит слабую ссылку на обработчик.
    prefix: String, // Случайный префикс идентификаторов этой копии приложения.
    clicks: Arc<Mutex<ClickQueue>>, // Цели баннеров и канал к оболочке, общие с обработчиком.
    click_receiver: Mutex<Option<mpsc::Receiver<NotificationClick>>>, // Поток, который оболочка забирает один раз.
}

impl Center {
    /// Подключает обработчик активного окна, если процесс запущен из пакета .app.
    pub(super) fn new() -> Option<Self> {
        let bundle = NSBundle::mainBundle();
        // Центр уведомлений требует идентификатор приложения. Обычная программа cargo run его не имеет.
        if unsafe { bundle.bundleIdentifier() }.is_none()
            || !unsafe { bundle.bundlePath() }.to_string().ends_with(".app")
        {
            return None;
        }
        let center = unsafe { UNUserNotificationCenter::currentNotificationCenter() };
        let (sender, receiver) = mpsc::channel(CLICK_BUFFER);
        let clicks = Arc::new(Mutex::new(ClickQueue {
            targets: VecDeque::new(),
            sender,
        }));
        let delegate = NotificationDelegate::new(Arc::clone(&clicks));
        unsafe { center.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
        Some(Self {
            center,
            _delegate: delegate,
            prefix: NSUUID::UUID().UUIDString().to_string(),
            clicks,
            click_receiver: Mutex::new(Some(receiver)),
        })
    }

    /// Передаёт оболочке очередь, которая уже принимает нажатия от macOS.
    pub(super) fn take_clicks(&self) -> Option<mpsc::Receiver<NotificationClick>> {
        self.click_receiver.lock().ok()?.take()
    }

    /// Возвращает текущие настройки баннеров без запроса разрешения.
    pub(super) fn permission(
        &self,
    ) -> impl Future<Output = Result<NotificationPermission, NotificationError>> + Send + 'static
    {
        let (sender, receiver) = oneshot::channel();
        self.read_permission(move |permission| {
            let _ = sender.send(Ok(permission));
        });
        await_callback(receiver)
    }

    /// Просит macOS показать диалог после нажатия кнопки в рабочем окне.
    pub(super) fn request_permission(
        &self,
    ) -> impl Future<Output = Result<(), NotificationError>> + Send + 'static {
        let (sender, receiver) = oneshot::channel();
        request_permission(&self.center, move |result| {
            let _ = sender.send(result);
        });
        await_callback(receiver)
    }

    /// После проверки разрешения отправляет запрос и возвращает ответ центра уведомлений.
    pub(super) fn show(
        &self,
        id: &str,
        title: &str,
        body: &str,
        target: NotificationClick,
    ) -> impl Future<Output = Result<(), NotificationError>> + Send + 'static {
        let (sender, receiver) = oneshot::channel();
        let id = format!("{}/{id}", self.prefix);
        let (title, body) = (title.to_owned(), body.to_owned());
        let clicks = Arc::clone(&self.clicks);
        self.read_permission(move |permission| {
            if permission != NotificationPermission::Enabled {
                let _ = sender.send(Err(NotificationError::PermissionDenied));
                return;
            }
            // Центр доступен из любого потока. Создаём объекты уведомления в самом обработчике.
            let center = unsafe { UNUserNotificationCenter::currentNotificationCenter() };
            let content = unsafe { UNMutableNotificationContent::new() };
            let request = unsafe {
                content.setTitle(&NSString::from_str(&title));
                content.setBody(&NSString::from_str(&body));
                UNNotificationRequest::requestWithIdentifier_content_trigger(
                    &NSString::from_str(&id),
                    &content,
                    None,
                )
            };
            // Цель должна появиться до отправки запроса: macOS может показать баннер сразу.
            if let Ok(mut queue) = clicks.lock() {
                queue.remember(id.clone(), target);
            }
            let callback = RcBlock::new(once(move |error: *mut NSError| {
                let result = system_result(unsafe { error.as_ref() });
                if result.is_err()
                    && let Ok(mut queue) = clicks.lock()
                {
                    queue.forget(&id);
                }
                let _ = sender.send(result);
            }));
            unsafe {
                center.addNotificationRequest_withCompletionHandler(&request, Some(&callback))
            };
        });
        await_callback(receiver)
    }

    /// Считывает актуальное разрешение и стиль баннеров, заданные пользователем.
    fn read_permission(&self, complete: impl FnOnce(NotificationPermission) + Send + 'static) {
        let callback = RcBlock::new(once(move |settings: NonNull<UNNotificationSettings>| {
            let settings = unsafe { settings.as_ref() };
            let permission = match unsafe { settings.authorizationStatus() } {
                UNAuthorizationStatus::NotDetermined => NotificationPermission::NeedsPermission,
                UNAuthorizationStatus::Authorized
                    if unsafe { settings.alertSetting() } == UNNotificationSetting::Enabled
                        && unsafe { settings.alertStyle() } != UNAlertStyle::None =>
                {
                    NotificationPermission::Enabled
                }
                _ => NotificationPermission::NeedsSettings,
            };
            complete(permission);
        }));
        unsafe {
            self.center
                .getNotificationSettingsWithCompletionHandler(&callback)
        };
    }
}

/// Открывает раздел уведомлений в новых и старых версиях macOS.
pub(super) fn open_settings() -> bool {
    let pane = if std::path::Path::new("/System/Applications/System Settings.app").exists() {
        "x-apple.systempreferences:com.apple.Notifications-Settings.extension"
    } else {
        "x-apple.systempreferences:com.apple.preference.notifications"
    };
    let url = unsafe { NSURL::URLWithString(&NSString::from_str(pane)) };
    url.is_some_and(|url| unsafe { NSWorkspace::sharedWorkspace().openURL(&url) })
}

/// Хранит цели последних отправленных баннеров и передаёт известные клики приложению.
struct ClickQueue {
    targets: VecDeque<(String, NotificationClick)>, // Цели по идентификатору запроса, от старой к новой.
    sender: mpsc::Sender<NotificationClick>, // Канал к подписке оболочки, допускает отправку до запуска окна.
}

impl ClickQueue {
    /// Запоминает цель по идентификатору, ограничивая память для старых баннеров.
    fn remember(&mut self, id: String, click: NotificationClick) {
        self.forget(&id);
        if self.targets.len() == RECENT_NOTIFICATIONS {
            self.targets.pop_front();
        }
        self.targets.push_back((id, click));
    }

    /// Убирает цель баннера, отправку которого система отклонила.
    fn forget(&mut self, id: &str) {
        self.targets.retain(|(key, _)| key != id);
    }

    /// Передаёт цель один раз. Неизвестные идентификаторы игнорирует.
    fn dispatch(&mut self, id: &str) {
        let Some(position) = self.targets.iter().position(|(key, _)| key == id) else {
            return;
        };
        if self
            .sender
            .try_send(self.targets[position].1.clone())
            .is_ok()
        {
            self.targets.remove(position);
        }
    }
}

/// Просит разрешение только на показ уведомлений. Звук и счётчик на иконке не используются.
fn request_permission(
    center: &UNUserNotificationCenter,
    complete: impl FnOnce(Result<(), NotificationError>) + Send + 'static,
) {
    let callback = once(move |(granted, error): (Bool, *mut NSError)| {
        complete(system_result(unsafe { error.as_ref() }).and_then(|()| {
            if granted.as_bool() {
                Ok(())
            } else {
                Err(NotificationError::PermissionDenied)
            }
        }));
    });
    let wrapper =
        RcBlock::new(move |granted: Bool, error: *mut NSError| callback((granted, error)));
    unsafe {
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::UNAuthorizationOptionAlert,
            &wrapper,
        );
    }
}

/// Позволяет обработчику macOS передать результат ровно один раз и из любого потока.
fn once<T>(callback: impl FnOnce(T) + Send) -> impl Fn(T) + Send + Sync {
    let callback = Mutex::new(Some(callback));
    move |value| {
        let callback = callback.lock().ok().and_then(|mut slot| slot.take());
        if let Some(callback) = callback {
            callback(value);
        }
    }
}

/// Возвращает результат запроса или ошибку, если ОС закрыла обработчик без ответа.
async fn await_callback<T>(
    receiver: oneshot::Receiver<Result<T, NotificationError>>,
) -> Result<T, NotificationError> {
    receiver.await.unwrap_or_else(|_| {
        Err(NotificationError::System(
            "Центр уведомлений macOS не ответил".to_owned(),
        ))
    })
}

/// Преобразует ошибку macOS. Запрет может прийти после проверки, если местные настройки изменились.
fn system_result(error: Option<&NSError>) -> Result<(), NotificationError> {
    let Some(error) = error else {
        return Ok(());
    };
    let domain = error.domain();
    if unsafe { UNErrorDomain } == Some(&*domain)
        && error.code() == UNErrorCode::NotificationsNotAllowed.0
    {
        return Err(NotificationError::PermissionDenied);
    }
    Err(NotificationError::System(
        error.localizedDescription().to_string(),
    ))
}

declare_class!(
    /// Управляет показом баннера в активном окне и получает его нажатие от macOS.
    struct NotificationDelegate;

    // macOS вызывает обработчик из разных потоков. Общая очередь защищена Mutex.
    unsafe impl ClassType for NotificationDelegate {
        type Super = NSObject;
        type Mutability = mutability::Immutable;
        const NAME: &'static str = "WorkspaceDesktopNotificationDelegate";
    }

    impl DeclaredClass for NotificationDelegate {
        type Ivars = Arc<Mutex<ClickQueue>>;
    }
    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        /// Просит показать баннер и добавить его в историю центра уведомлений.
        #[allow(non_snake_case)]
        #[method(userNotificationCenter:willPresentNotification:withCompletionHandler:)]
        fn userNotificationCenter_willPresentNotification_withCompletionHandler(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion: &Block<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion.call((
                UNNotificationPresentationOptions::UNNotificationPresentationOptionBanner
                    | UNNotificationPresentationOptions::UNNotificationPresentationOptionList,
            ));
        }

        /// Передаёт клик по баннеру, игнорирует закрытие и всегда отвечает системе.
        #[allow(non_snake_case)]
        #[method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:)]
        fn userNotificationCenter_didReceiveNotificationResponse_withCompletionHandler(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &Block<dyn Fn()>,
        ) {
            let action = unsafe { response.actionIdentifier() };
            if *action == *unsafe { UNNotificationDefaultActionIdentifier } {
                let id = unsafe { response.notification().request().identifier() };
                if let Ok(mut queue) = self.ivars().lock() {
                    queue.dispatch(&id.to_string());
                }
            }
            completion.call(());
        }
    }
);

impl NotificationDelegate {
    /// Создаёт обработчик с общей очередью, которую может читать любой поток macOS.
    fn new(clicks: Arc<Mutex<ClickQueue>>) -> Retained<Self> {
        let delegate = Self::alloc().set_ivars(clicks);
        // NSObject не требует дополнительных аргументов для init.
        unsafe { objc2::msg_send_id![super(delegate), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{StreamExt, executor::block_on};
    use workspace_core::{
        context::{AccountId, OwnerContext, SessionGeneration},
        notification_target::NotificationTarget,
    };

    /// Проверяет, что системный делегат зарегистрировал обработчик нажатия, а очередь сохраняет цель.
    #[test]
    fn registers_activation_and_buffers_the_exact_target_once() {
        let (sender, mut receiver) = mpsc::channel(CLICK_BUFFER);
        let clicks = Arc::new(Mutex::new(ClickQueue {
            targets: VecDeque::new(),
            sender,
        }));
        let delegate = NotificationDelegate::new(Arc::clone(&clicks));
        assert!(delegate.respondsToSelector(objc2::sel!(
            userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:
        )));

        let click = NotificationClick {
            target: NotificationTarget {
                context: OwnerContext {
                    account_id: AccountId("recipient".into()),
                    server: "https://workspace.example.test".into(),
                    organization_id: "organization".into(),
                    project_id: "project".into(),
                    user_id: "user".into(),
                },
                conversation_id: "conversation".into(),
                message_id: "message".into(),
            },
            generation: SessionGeneration(42),
        };
        {
            let mut queue = delegate.ivars().lock().unwrap();
            queue.remember("current-process/banner".into(), click.clone());
            queue.dispatch("previous-process/banner");
            queue.dispatch("current-process/banner");
            queue.dispatch("current-process/banner");
        }
        drop(delegate);
        drop(clicks);

        assert_eq!(block_on(receiver.next()), Some(click));
        assert_eq!(block_on(receiver.next()), None);
    }
}
