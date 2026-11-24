//! Подтверждает перевыпуск и удаление выбранного ключа в модальном окне разговора.

use gpui::{
    App, Context, FocusHandle, Render, Subscription, WeakEntity, Window, div, prelude::*, px,
};

use super::super::{Conversation, EncryptedScreen};
use super::{Change, KeyControls, KeyManagement};
use crate::gpui_client::{
    contracts::ConversationAddress,
    preferences::Preferences,
    right_panel::RightPanel,
    ui::{Button, ButtonAppearance, Palette, SURFACE_RADIUS, TextRole, label},
};

/// Подтверждение сохраняет конкретный ключ и экран, из которого его открыли.
pub(super) struct KeyConfirmation {
    host: WeakEntity<RightPanel>, // Стек, возвращающий фокус после закрытия.
    conversation: WeakEntity<Conversation>, // Экран, которому принадлежит действие.
    _changes: Subscription,       // Перерисовывает отправку и результат.
    address: ConversationAddress, // Вход, стрим и тема на момент открытия.
    screen: Option<EncryptedScreen>, // Открытые сведения или конкретный вложенный экран.
    change: Change,               // На какой ключ направлено действие.
    region: FocusHandle,          // Фокус диалога на время серверной операции.
}

impl Conversation {
    /// Открывает отменяемое подтверждение для действия и ключа из видимого экрана.
    pub(super) fn confirm_key_change(
        &mut self,
        change: Change,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &self.details else {
            return;
        };
        let screen = panel.encrypted_screen();
        if self
            .key_change_state(change, screen, cx)
            .is_none_or(|keys| keys.operation.is_some())
        {
            return;
        }
        let address = self.address.clone();
        let host = panel.host.clone();
        let conversation = cx.entity();
        let target = conversation.downgrade();
        let opened = host.update(cx, |host, cx| {
            let weak_host = cx.entity().downgrade();
            let opened =
                host.open_conversation_modal(&address, opener, window, cx, |region, cx| {
                    cx.new(|cx| KeyConfirmation {
                        host: weak_host,
                        conversation: target.clone(),
                        _changes: cx.observe(&conversation, |_, _, cx| cx.notify()),
                        address: address.clone(),
                        screen,
                        change,
                        region,
                    })
                });
            if opened {
                host.set_modal_dismiss(move |app| {
                    let Some(conversation) = target.upgrade() else {
                        return true;
                    };
                    let Some(KeyManagement::Ready(keys)) = conversation
                        .read(app)
                        .details
                        .as_ref()
                        .map(|panel| &panel.keys)
                    else {
                        return true;
                    };
                    keys.operation.is_none()
                });
            }
            opened
        });
        if opened
            && let Some(KeyManagement::Ready(keys)) =
                self.details.as_mut().map(|panel| &mut panel.keys)
        {
            keys.error = false;
            cx.notify();
        }
    }
}

impl KeyConfirmation {
    /// Проверяет, что команда относится к тому же входу и текущему ключу.
    fn state<'a>(&self, app: &'a App) -> Option<&'a KeyControls> {
        let conversation = self.conversation.upgrade()?;
        let conversation = conversation.read(app);
        (conversation.address == self.address)
            .then(|| conversation.key_change_state(self.change, self.screen, app))
            .flatten()
    }

    /// Отменяет только подтверждение ключа и возвращает фокус его кнопке.
    fn close(&self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.host.update(cx, |host, cx| {
            if host.modal_is::<Self>() {
                host.close_modal(window, cx);
            }
        });
    }

    /// Передаёт операции точный ключ и экран, указанные при открытии подтверждения.
    fn submit(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state(cx).is_none_or(|keys| keys.operation.is_some()) {
            return;
        }
        window.focus(&self.region);
        let _ = self.conversation.update(cx, |conversation, cx| {
            conversation.apply_key_management(self.change, self.screen, window, cx);
        });
    }
}

impl Render for KeyConfirmation {
    /// Показывает последствия операции, отмену и повтор при неподтверждённом результате.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(keys) = self.state(cx) else {
            self.close(window, cx);
            return div();
        };
        let busy = keys.operation.is_some();
        let failed = keys.error;
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read_for(cx, &self.address.route.owner).language;
        let (heading, description, submit_title) = match self.change {
            Change::Rotate(_) => (
                "e2ee.rotateConfirmTitle",
                "e2ee.rotateConfirmDescription",
                "e2ee.rotateKey",
            ),
            Change::Remove(_) => (
                "e2ee.deleteLocalKeyConfirmTitle",
                "e2ee.deleteLocalKeyConfirmDescription",
                "e2ee.deleteLocalKey",
            ),
        };
        let mut cancel = Button::new("conversation-key-cancel", language.tr("common.cancel"));
        cancel.disabled = busy;
        let mut confirm = Button::new("conversation-key-confirm", language.tr(submit_title));
        confirm.appearance = ButtonAppearance::Primary;
        confirm.disabled = busy;
        let edit = cx.entity().downgrade();
        let dismiss = edit.clone();
        div()
            .debug_selector(|| "conversation-key-dialog".into())
            .w((window.viewport_size().width - px(32.)).clamp(px(0.), px(480.)))
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .rounded(SURFACE_RADIUS)
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .child(label(palette, TextRole::Heading, language.tr(heading)))
            .child(label(palette, TextRole::Body, language.tr(description)))
            .when(failed, |body| {
                body.child(
                    label(
                        palette,
                        TextRole::Caption,
                        language.tr(
                            "desktop.conversation.encryption.actionCouldNotBeConfirmedTryAgain",
                        ),
                    )
                    .text_color(palette.danger),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        cancel
                            .render(palette, None, move |window, app| {
                                let _ = dismiss.update(app, |dialog, cx| dialog.close(window, cx));
                            })
                            .debug_selector(|| "conversation-key-cancel".into()),
                    )
                    .child(
                        confirm
                            .render(palette, None, move |window, app| {
                                let _ = edit.update(app, |dialog, cx| dialog.submit(window, cx));
                            })
                            .debug_selector(|| "conversation-key-confirm".into()),
                    ),
            )
    }
}
