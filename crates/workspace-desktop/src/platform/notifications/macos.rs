//! Подключает центр уведомлений macOS, проверяет разрешение и запрашивает показ баннера.
//! Обработчик в конце файла нужен для показа уведомлений, пока окно приложения активно.

use std::future::Future;
use std::ptr::NonNull;
use std::sync::Mutex;

use block2::{Block, RcBlock};
use futures::channel::oneshot;
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{ClassType, DeclaredClass, declare_class, mutability};
use objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNErrorCode, UNErrorDomain,
    UNMutableNotificationContent, UNNotification, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationSetting, UNNotificationSettings, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use super::NotificationError;

/// Соединение с центром уведомлений и обработчик, который живёт вместе с ним.
pub(super) struct Center {
    center: Retained<UNUserNotificationCenter>, // Общий центр уведомлений приложения.
    _delegate: Retained<ForegroundDelegate>,    // Центр держит слабую ссылку на обработчик.
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
        let delegate: Retained<ForegroundDelegate> =
            unsafe { objc2::msg_send_id![ForegroundDelegate::class(), new] };
        unsafe { center.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
        Some(Self {
            center,
            _delegate: delegate,
        })
    }

    /// Проверяет разрешение и просит его, если пользователь ещё не принимал решение.
    pub(super) fn prepare(
        &self,
    ) -> impl Future<Output = Result<(), NotificationError>> + Send + 'static {
        let (sender, receiver) = oneshot::channel();
        self.with_alert_permission(move |result| {
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
    ) -> impl Future<Output = Result<(), NotificationError>> + Send + 'static {
        let (sender, receiver) = oneshot::channel();
        let (id, title, body) = (id.to_owned(), title.to_owned(), body.to_owned());
        self.with_alert_permission(move |permission| {
            if let Err(error) = permission {
                let _ = sender.send(Err(error));
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
            let callback = RcBlock::new(once(move |error: *mut NSError| {
                let _ = sender.send(system_result(unsafe { error.as_ref() }));
            }));
            unsafe {
                center.addNotificationRequest_withCompletionHandler(&request, Some(&callback))
            };
        });
        await_callback(receiver)
    }

    /// Читает актуальные настройки ОС и при необходимости запускает системный запрос.
    fn with_alert_permission(
        &self,
        complete: impl FnOnce(Result<(), NotificationError>) + Send + 'static,
    ) {
        let callback = RcBlock::new(once(move |settings: NonNull<UNNotificationSettings>| {
            let settings = unsafe { settings.as_ref() };
            match unsafe { settings.authorizationStatus() } {
                UNAuthorizationStatus::NotDetermined => request_permission(complete),
                UNAuthorizationStatus::Authorized
                    if unsafe { settings.alertSetting() } == UNNotificationSetting::Enabled =>
                {
                    complete(Ok(()));
                }
                _ => complete(Err(NotificationError::PermissionDenied)),
            }
        }));
        unsafe {
            self.center
                .getNotificationSettingsWithCompletionHandler(&callback)
        };
    }
}

/// Просит разрешение только на показ уведомлений. Звук и счётчик на иконке не используются.
fn request_permission(complete: impl FnOnce(Result<(), NotificationError>) + Send + 'static) {
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
    let center = unsafe { UNUserNotificationCenter::currentNotificationCenter() };
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
async fn await_callback(
    receiver: oneshot::Receiver<Result<(), NotificationError>>,
) -> Result<(), NotificationError> {
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
    /// Передаёт macOS решение о показе уведомления при активном приложении.
    struct ForegroundDelegate;

    // У обработчика нет изменяемого состояния. macOS может вызвать его в любом потоке.
    unsafe impl ClassType for ForegroundDelegate {
        type Super = NSObject;
        type Mutability = mutability::Immutable;
        const NAME: &'static str = "WorkspaceDesktopNotificationDelegate";
    }

    impl DeclaredClass for ForegroundDelegate {}
    unsafe impl NSObjectProtocol for ForegroundDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for ForegroundDelegate {
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
    }
);
