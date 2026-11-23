//! Подключает защищённый звонок к шапке, панели сведений и строке истории.
//! Core проверяет приглашение, а экран открывает ссылку только для ещё живого адреса.

use std::{sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, Context, Div, FocusHandle, IntoElement, Stateful, Task, WeakEntity, div,
    prelude::*,
};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        calls::{CallError, JitsiSettings},
        encryption::ProtectedMessage,
    },
    runtime::SourceError,
};

use super::{Conversation, State, action, encryption::EncryptedPresentation};
use crate::gpui_client::{
    preferences::{Language, Preferences},
    runtime::RuntimeClient,
    ui::{Button, ButtonAppearance, Icon, Palette, TextRole, label},
};

const SETTINGS_RETRY_DELAY: Duration = Duration::from_secs(2);

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
    /// Читает настройки только когда у показанного окна есть защищённое содержимое или режим.
    pub(super) fn load_call_settings(&mut self, cx: &mut Context<Self>) {
        let Some(runtime) = self.calls.runtime.clone() else {
            return;
        };
        if self.calls.checked
            || self.calls.settings_task.is_some()
            || !self.has_access()
            || !(self.protected_call_route(cx) || self.has_encrypted_history())
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

    /// Строит общую кнопку действия для шапки и сведений.
    pub(super) fn call_trigger(
        &self,
        details: bool,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        if !self.has_access() || self.calls.settings.is_none() || !self.protected_call_route(cx) {
            return None;
        }
        let locale = Preferences::read_for(cx, &self.address.route.owner).language;
        let (id, focus) = if details {
            ("protected-call-details", &self.calls.details_focus)
        } else {
            ("protected-call-heading", &self.calls.header_focus)
        };
        let mut button = Button::new(
            id,
            if self.calls.pending.is_some() {
                locale.text("Отправляем приглашение…", "Sending invitation…")
            } else {
                locale.text("Позвонить", "Start call")
            },
        );
        button.icon = Some(Icon::Phone);
        button.appearance = ButtonAppearance::Ghost;
        button.disabled = self.calls.pending.is_some() || self.call_topic(cx).is_none();
        Some(
            button
                .render(palette, Some(focus), action(cx, Self::start_protected_call))
                .flex_none()
                .debug_selector(move || id.into()),
        )
    }

    /// Показывает границу защиты текста и вложений в сведениях о защищённом чате.
    pub(super) fn protected_call_details(
        &self,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Option<Div> {
        if !self.has_access() || !self.protected_call_route(cx) {
            return None;
        }
        let locale = Preferences::read_for(cx, &self.address.route.owner).language;
        Some(div().flex().flex_none().flex_col().gap_1().px_2().py_2()
            .children(self.call_trigger(true, palette, cx))
            .child(label(palette, TextRole::Caption, locale.text(
                "Текст сообщений и ссылки внутри защищены сквозным шифрованием.",
                "Message text and links are protected by end-to-end encryption.",
            )))
            .child(label(palette, TextRole::Caption, locale.text(
                "Изображения, видео, файлы и их метаданные хранятся без сквозного шифрования.",
                "Images, videos, files and their metadata are stored without end-to-end encryption.",
            )))
            .debug_selector(|| "protected-chat-boundary".into()))
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
                locale.text(
                    "Не удалось подтвердить приглашение в звонок",
                    "Could not confirm the call invitation",
                ),
            )
            .px_4()
            .py_1()
            .debug_selector(|| "protected-call-error".into()),
        )
    }

    /// Запускает одну подготовку из точной темы композера и ждёт подтверждения Core.
    fn start_protected_call(&mut self, cx: &mut Context<Self>) {
        if !self.has_access() || self.calls.pending.is_some() || self.calls.settings.is_none() {
            return;
        }
        let (Some(topic), Some(runtime), Some(encryption)) = (
            self.call_topic(cx),
            self.calls.runtime.clone(),
            self.encryption.as_ref(),
        ) else {
            return;
        };
        let activation = encryption.activation.clone();
        let address = self.address.clone();
        self.calls.failed = false;
        self.calls.pending = Some(cx.spawn(async move |target, cx| {
            let reply = runtime
                .conversation(address, move |services, address| async move {
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
                    .and_then(|call| call.with_url(|url| cx.open_url(url)))
                    .is_none();
                view.refresh_call_controls(cx);
            });
        }));
        self.refresh_call_controls(cx);
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
    fn call_topic(&self, cx: &App) -> Option<Uuid> {
        self.sending.as_ref()?.view.read(cx).protected_send_topic()
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
        locale.text("Войти в звонок", "Join call"),
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
