//! Восстанавливает входящие ответы на ключи для выбранного устройства независимо от открытого маршрута.

use gpui::Context;
use workspace_core::messenger::encryption::EncryptionActivation;

use super::Client;
use crate::gpui_client::runtime::IncomingKey;

impl Client {
    /// Запускает повтор запроса ответа и передаёт квитанцию его текущему видимому состоянию.
    pub(super) fn queue_key_response(
        &self,
        activation: EncryptionActivation,
        incoming: IncomingKey,
        cx: &mut Context<Self>,
    ) {
        let runtime = self.runtime.clone();
        let background = cx.background_executor().clone();
        cx.spawn(async move |client, cx| {
            let Some((target, receipt)) = runtime
                .apply_incoming_key(activation.clone(), incoming, background)
                .await
            else {
                return;
            };
            let _ = client.update(cx, |client, cx| {
                if let Some(view) = &client.conversation {
                    view.update(cx, |view, cx| {
                        view.key_request_completed(&activation, target, receipt, cx)
                    });
                }
            });
        })
        .detach();
    }

    /// Проверяет SQLite после старта, выбора входа и пропуска подписки, без чтения окна чата.
    pub(super) fn reconcile_key_inbox(
        &mut self,
        activation: EncryptionActivation,
        cx: &mut Context<Self>,
    ) {
        let runtime = self.runtime.clone();
        let background = cx.background_executor().clone();
        self.key_response_scan = Some(cx.spawn(async move |client, cx| {
            let mut cursor = None;
            while let Some(page) = runtime
                .saved_key_response_page(&activation, cursor, &background)
                .await
            {
                let updates = client.update(cx, |client, cx| {
                    for hint in page.responses {
                        let incoming = IncomingKey::persisted(&activation, hint);
                        client.queue_key_response(activation.clone(), incoming, cx);
                    }
                });
                if updates.is_err() || page.next.is_none() {
                    break;
                }
                cursor = page.next;
            }
        }));
    }
}
