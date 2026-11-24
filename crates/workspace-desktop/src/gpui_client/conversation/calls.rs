//! Подключает звонок к шапке и панели сведений, а защищённое приглашение — к истории.
//! Core проверяет приглашение, а экран открывает ссылку только для ещё живого адреса.

use std::{sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, Context, Div, FocusHandle, IntoElement, Stateful, Task, WeakEntity, prelude::*,
};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        calls::{CallError, DeliveredJitsiCall, DeliveredPlainJitsiCall, JitsiSettings},
        encryption::ProtectedMessage,
    },
    runtime::SourceError,
};

use super::{Conversation, State, action, encryption::EncryptedPresentation, heading::icon_button};
use crate::gpui_client::{
    preferences::{Language, Preferences},
    runtime::RuntimeClient,
    ui::{Button, Icon, Palette, TextRole, label, popup::tooltip},
};

const SETTINGS_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Приглашение, которое ядро разрешило открыть после проверки квитанции.
enum ConfirmedInvitation {
    Plain(DeliveredPlainJitsiCall), // Отправленная ссылка в обычном сообщении.
    Protected(DeliveredJitsiCall),  // Отправленная ссылка в защищённом сообщении.
}

impl ConfirmedInvitation {
    /// Передаёт адрес системному адаптеру под проверкой действующего разрешения.
    fn open(&self, cx: &mut App) -> Option<()> {
        let open = |url: &str| cx.open_url(url);
        match self {
            Self::Plain(call) => call.with_url(open),
            Self::Protected(call) => call.with_url(open),
        }
    }
}

/// Состояние звонка одного открытого разговора.
pub(super) struct Calls {
    runtime: Option<RuntimeClient>, // Запускает команды через рабочий Core.
    settings: Option<Arc<JitsiSettings>>, // Проверенный сервер Jitsi текущего владельца.
    settings_task: Option<Task<()>>, // Одно чтение конфигурации для этого экрана.
    checked: bool,                  // Настройки прочитаны, в том числе если сервис отключён.
    pending: Option<Task<()>>,      // Ожидает подтверждения отправки приглашения.
    join_task: Option<Task<()>>,    // Проверяет оригинал ссылки перед входом в комнату.
    failed: bool,                   // Видимая ошибка последней попытки.
    header_focus: FocusHandle,      // Кнопка в шапке.
    details_focus: FocusHandle,     // Кнопка в сведениях.
}

impl Calls {
    /// Готовит состояние и клавиатурный фокус общих кнопок.
    pub(super) fn new(cx: &App) -> Self {
        Self {
            runtime: None,
            settings: None,
            settings_task: None,
            checked: false,
            pending: None,
            join_task: None,
            failed: false,
            header_focus: cx.focus_handle().tab_stop(true),
            details_focus: cx.focus_handle().tab_stop(true),
        }
    }

    /// Подключает рабочий поток обычного экрана разговора.
    pub(super) fn set_runtime(&mut self, runtime: RuntimeClient) {
        self.runtime = Some(runtime);
    }

    /// Отдаёт проверенную конфигурацию только текущему представлению.
    pub(super) fn settings(&self) -> Option<&Arc<JitsiSettings>> {
        self.settings.as_ref()
    }

    /// Закрывает ожидание и не позволяет прийти к системному открытию после отзыва.
    pub(super) fn cancel(&mut self) {
        self.pending.take();
        self.join_task.take();
        self.settings_task.take();
        self.failed = false;
    }
}

impl Conversation {
    /// Читает настройки для доступного чата с приглашениями Jitsi.
    pub(super) fn load_call_settings(&mut self, cx: &mut Context<Self>) {
        let Some(runtime) = self.calls.runtime.clone() else {
            return;
        };
        if self.calls.checked
            || self.calls.settings_task.is_some()
            || !self.has_access()
            || (self.favorites_presentation && !self.has_encrypted_history())
        {
            return;
        }
        let address = self.address.clone();
        self.calls.settings_task = Some(cx.spawn(async move |target, cx| {
            let reply = runtime
                .conversation(address, |services, address| async move {
                    services
                        .messenger
                        .jitsi_settings(&services.sync, &address.route.owner, address.generation)
                        .await
                })
                .await;
            let _ = target.update(cx, |view, cx| {
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                view.calls.settings_task.take();
                if !view.has_access() {
                    return;
                }
                match result {
                    Ok(Ok(settings)) => {
                        view.calls.checked = true;
                        view.calls.settings = Some(Arc::new(settings));
                        view.refresh_call_controls(cx);
                    }
                    Ok(Err(CallError::Unavailable)) => view.calls.checked = true,
                    Ok(Err(CallError::Source(SourceError::Temporary))) | Err(_) => {
                        view.retry_call_settings(cx);
                    }
                    _ => {}
                }
            });
        }));
    }

    /// Повторяет временный отказ на том же экране с паузой между чтениями.
    fn retry_call_settings(&mut self, cx: &mut Context<Self>) {
        let delay = cx.background_executor().timer(SETTINGS_RETRY_DELAY);
        self.calls.settings_task = Some(cx.spawn(async move |target, cx| {
            delay.await;
            let _ = target.update(cx, |view, cx| {
                view.calls.settings_task.take();
                view.load_call_settings(cx);
            });
        }));
    }

    /// Строит кнопку звонка в шапке открытого разговора.
    pub(super) fn call_trigger(
        &self,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        let protected = self.protected_call_route(cx);
        if !self.has_access() || self.calls.settings.is_none() || self.favorites_presentation {
            return None;
        }
        let locale = Preferences::read_for(cx, &self.address.route.owner).language;
        let hint = if self.calls.pending.is_some() {
            locale.tr("desktop.conversation.calls.sendingInvitation")
        } else {
            locale.tr("call.call")
        };
        let disabled = self.calls.pending.is_some() || self.send_topic_for_call(cx).is_none();
        let id = if protected {
            "protected-call-heading"
        } else {
            "conversation-call-heading"
        };
        Some(icon_button(
            id,
            Icon::Phone,
            hint.into(),
            &self.calls.header_focus,
            disabled,
            palette,
            action(cx, Self::start_call),
        ))
    }

    /// Показывает действие звонка в карточке защищённого чата, включая недоступное состояние.
    pub(super) fn protected_call_button(
        &self,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let locale = Preferences::read_for(cx, &self.address.route.owner).language;
        let hint = if self.calls.pending.is_some() {
            locale.tr("desktop.conversation.calls.sendingInvitation")
        } else {
            locale.tr("call.call")
        };
        let id = "protected-call-details";
        let mut button = Button::new(id, "");
        button.disabled = !self.has_access()
            || !self.protected_call_route(cx)
            || self.favorites_presentation
            || self.calls.settings.is_none()
            || self.calls.pending.is_some()
            || self.send_topic_for_call(cx).is_none();
        let color = if button.disabled {
            palette.disabled
        } else {
            palette.text
        };
        button
            .render(
                palette,
                Some(&self.calls.details_focus),
                action(cx, Self::start_call),
            )
            .h(gpui::px(48.))
            .flex_1()
            .child(Icon::Phone.element(gpui::px(32.), color))
            .tooltip(move |_, app| tooltip(palette, hint.into(), app))
            .debug_selector(move || id.into())
    }

    /// Возвращает доступность звонка в карточке текущего собеседника.
    pub(super) fn peer_call_availability(&self, cx: &App) -> Option<bool> {
        (self.has_access()
            && !self.favorites_presentation
            && self.calls.settings.is_some()
            && !self.protected_call_route(cx))
        .then(|| self.calls.pending.is_none() && self.send_topic_for_call(cx).is_some())
    }

    /// Запускает общий сценарий звонка из шапки или правой панели.
    pub(super) fn start_call(&mut self, cx: &mut Context<Self>) {
        if !self.has_access()
            || self.favorites_presentation
            || self.calls.pending.is_some()
            || self.calls.settings.is_none()
        {
            return;
        }
        let (Some(topic), Some(runtime)) =
            (self.send_topic_for_call(cx), self.calls.runtime.clone())
        else {
            return;
        };
        let activation = if self.protected_call_route(cx) {
            let Some(encryption) = self.encryption.as_ref() else {
                return;
            };
            Some(encryption.activation.clone())
        } else {
            None
        };
        let address = self.address.clone();
        self.calls.failed = false;
        self.calls.pending = Some(cx.spawn(async move |target, cx| {
            let reply = runtime
                .conversation(address, move |services, address| async move {
                    if let Some(activation) = activation {
                        let pending = services
                            .messenger
                            .prepare_encrypted_call(
                                &services.sync,
                                &activation,
                                address.route.scope,
                                topic,
                            )
                            .await?;
                        services
                            .messenger
                            .wait_for_call_invitation(&services.sync, pending)
                            .await
                            .map(ConfirmedInvitation::Protected)
                    } else {
                        services
                            .messenger
                            .send_jitsi_invitation(
                                &services.sync,
                                &address.route.owner,
                                address.generation,
                                address.route.scope,
                                topic,
                            )
                            .await
                            .map(ConfirmedInvitation::Plain)
                    }
                })
                .await;
            let _ = target.update(cx, |view, cx| {
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                view.calls.pending.take();
                if !view.has_access() {
                    return;
                }
                view.calls.failed = result
                    .ok()
                    .and_then(Result::ok)
                    .and_then(|call| call.open(cx))
                    .is_none();
                view.refresh_call_controls(cx);
            });
        }));
        self.refresh_call_controls(cx);
    }

    /// Оставляет отказ в чате и возвращает возможность нажать кнопку ещё раз.
    pub(super) fn call_error(&self, palette: Palette, cx: &Context<Self>) -> Option<Div> {
        if !self.calls.failed || !self.has_access() {
            return None;
        }
        let locale = Preferences::read_for(cx, &self.address.route.owner).language;
        Some(
            label(
                palette,
                TextRole::Caption,
                locale.tr("desktop.conversation.calls.couldNotConfirmTheCallInvitation"),
            )
            .px_4()
            .py_1()
            .debug_selector(|| "conversation-call-error".into()),
        )
    }

    /// Повторно проверяет ту же видимую версию сообщения непосредственно перед открытием.
    fn join_protected_call(
        &mut self,
        id: Uuid,
        expected: &Arc<ProtectedMessage>,
        cx: &mut Context<Self>,
    ) {
        if self.calls.join_task.is_some() || !self.has_access() {
            return;
        }
        let (Some(runtime), Some(encryption), State::Ready(window)) =
            (self.calls.runtime.clone(), &self.encryption, &self.state)
        else {
            return;
        };
        let Some(row) = window
            .messages
            .iter()
            .find(|message| message.id == id)
            .cloned()
        else {
            return;
        };
        let Some(EncryptedPresentation::Ready(current)) = encryption.presentation(id) else {
            return;
        };
        if !Arc::ptr_eq(current, expected) || encryption.message_change_pending(id) {
            return;
        }
        let activation = encryption.activation.clone();
        let address = self.address.clone();
        self.calls.failed = false;
        self.calls.join_task = Some(cx.spawn(async move |target, cx| {
            let reply = runtime
                .conversation(address, move |services, address| async move {
                    services
                        .messenger
                        .confirm_jitsi_join(&services.sync, &activation, address.route.scope, &row)
                        .await
                })
                .await;
            let _ = target.update(cx, |view, cx| {
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                view.calls.join_task.take();
                if view.has_access() {
                    view.calls.failed = result
                        .ok()
                        .and_then(Result::ok)
                        .and_then(|call| call.with_url(|url| cx.open_url(url)))
                        .is_none();
                    view.refresh_call_controls(cx);
                }
            });
        }));
    }

    /// Берёт уже подтверждённое назначение из того же редактора.
    fn send_topic_for_call(&self, cx: &App) -> Option<Uuid> {
        let composer = self.sending.as_ref()?.view.read(cx);
        composer
            .protected_send_topic()
            .or_else(|| composer.plain_send_topic())
    }

    /// Определяет защищённый режим цели даже при временном отсутствии ключа.
    fn protected_call_route(&self, cx: &App) -> bool {
        self.sending
            .as_ref()
            .is_some_and(|sending| sending.view.read(cx).known_protected_destination())
    }

    /// Обновляет общий экран и рамку сведений, когда меняются кнопки звонка.
    fn refresh_call_controls(&self, cx: &mut Context<Self>) {
        if let Some(panel) = &self.details {
            panel.refresh_frame(cx);
        }
        cx.notify();
    }
}

/// Добавляет действие входа, сохраняя только ссылку на отзывное представление сообщения.
pub(super) fn join_card(
    id: Uuid,
    text: &Arc<ProtectedMessage>,
    settings: Option<&JitsiSettings>,
    conversation: Option<&WeakEntity<Conversation>>,
    palette: Palette,
    locale: Language,
) -> Option<AnyElement> {
    let settings = settings?;
    let target = conversation?.clone();
    if text.with_body(|markdown| settings.recognize_invitation(markdown).is_some()) != Some(true) {
        return None;
    }
    let expected = Arc::clone(text);
    let mut button = Button::new(
        (gpui::ElementId::Uuid(id), "join-jitsi"),
        locale.tr("message.joinCall"),
    );
    button.icon = Some(Icon::Phone);
    Some(
        button
            .render(palette, None, move |_, app| {
                let _ = target.update(app, |view, cx| view.join_protected_call(id, &expected, cx));
            })
            .debug_selector(move || format!("protected-call-join-{id}"))
            .into_any_element(),
    )
}
