//! Передаёт пробуждение macOS фоновой работе и снимает подписку при закрытии окна.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_app_kit::{NSWorkspace, NSWorkspaceDidWakeNotification};
use objc2_foundation::{MainThreadMarker, NSNotification, NSNotificationCenter, NSObject};
use tokio::sync::mpsc;

/// Держит подписку на пробуждение в главном потоке в течение жизни окна.
pub struct WakeObserver {
    center: Retained<NSNotificationCenter>, // Центр уведомлений NSWorkspace.
    observer: Retained<NSObject>,           // Метка, по которой снимается подписка.
    _main_thread: MainThreadMarker,         // Оставляет снятие подписки в главном потоке.
}

impl WakeObserver {
    /// Подписывается на сигнал пробуждения. Одного ожидающего сигнала достаточно для Runtime.
    pub fn subscribe(main_thread: MainThreadMarker, wake: mpsc::Sender<()>) -> Self {
        let callback = RcBlock::new(move |_: NonNull<NSNotification>| {
            if wake.try_send(()).is_ok() {
                log::info!("macOS wake signal queued for Workspace synchronization");
            }
        });
        // Регистрируемся на главном потоке. Блок может вызываться из другого потока:
        // он использует только потокобезопасный отправитель и не трогает окно.
        let (center, observer) = unsafe {
            let workspace = NSWorkspace::sharedWorkspace();
            let center = workspace.notificationCenter();
            let observer = center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceDidWakeNotification),
                Some(&workspace),
                None,
                &callback,
            );
            (center, observer)
        };
        Self {
            center,
            observer,
            _main_thread: main_thread,
        }
    }
}

impl Drop for WakeObserver {
    /// Снимает подписку с того же центра, где она зарегистрирована.
    fn drop(&mut self) {
        // Метка получена от этого центра и остаётся действительной до снятия подписки.
        unsafe { self.center.removeObserver(&self.observer) };
    }
}
