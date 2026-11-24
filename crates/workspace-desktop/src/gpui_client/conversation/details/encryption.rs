//! Показывает сведения о локальных ключах и запускает управление точным поколением из панели чата.

mod confirmation;
mod view;

use std::collections::HashMap;

use futures::{StreamExt, stream};
use gpui::{App, Context, FocusHandle, Window};
use uuid::Uuid;
use workspace_core::messenger::encryption::{
    key_exchange::{
        KeyExchange, KeyReplyCapabilities, KeyRequestCompletion, KeyRequestOutcome,
        KeyRequestStatus, KeyRequestTarget,
    },
    lifecycle::{KeyLifecycleError, LocalStreamKeys},
};

use super::{Conversation, EncryptedScreen, State};
use workspace_core::storage::KeyRequestHint;

use confirmation::KeyConfirmation;

/// Состояние ключей для текущего открытия панели. Хранит только UUID и разрешённые действия.
#[derive(Default)]
pub(super) enum KeyManagement {
    #[default]
    Hidden, // Обычный разговор или сервер ещё не подтвердил режим.
    Loading(Uuid),           // Чтение списка для этого открытия.
    Failed,                  // Не удалось прочитать список, можно повторить.
    Ready(Box<KeyControls>), // Есть сведения о ключах для показанного пользователя.
}

/// Видимые локальные ключи и действие, которое уже запустили или ожидаем подтвердить.
pub(super) struct KeyControls {
    data: LocalStreamKeys, // Подтверждённое поколение, право ротации и локальные UUID.
    expanded: bool,        // Развёрнут ли список поколений этого чата.
    request_sections: [bool; 3], // Раскрытие входящих, выданных и отклонённых запросов.
    key_focus: HashMap<Uuid, FocusHandle>, // Фокус каждой строки для возврата после просмотра.
    rotate_focus: FocusHandle, // Кнопка перевыпуска на текущем экране.
    remove_focus: FocusHandle, // Кнопка удаления открытого ключа.
    operation: Option<Uuid>, // Активный вызов, к которому относится поздний ответ.
    error: bool,           // Последнее действие не получило подтверждения.
    requests: HashMap<Uuid, RequestedKey>, // Состояния своих запросов по UUID поколения.
    bulk: Vec<PendingKeyGrant>, // Известные запросы всех тем и отдельные результаты выдачи.
}

/// Состояние запроса ключа с этого устройства.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestedKey {
    Waiting(Uuid), // Публичный UUID запроса до подтверждённого ответа.
    Rejected,      // После подтверждённого отказа можно отправить новый запрос.
}

/// Строка списка адресована точному запросу из журнала сообщений.
#[derive(Clone)]
struct PendingKeyGrant {
    hint: KeyRequestHint, // Подтверждённое размещение и имя устройства из истории.
    author_id: Uuid,      // UUID автора, подтверждённый серверным чтением запроса.
    capabilities: KeyReplyCapabilities, // Какие ответы разрешил Core.
    state: GrantState,    // Решение или состояние доставки для одного запроса.
}

/// Что происходит с одной строкой доступного набора.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GrantState {
    Pending,                      // Можно принять решение по запросу.
    Sending,                      // Запущена явная операция ответа.
    Completed(KeyRequestOutcome), // Сервер подтвердил выдачу или отказ.
    Failed(KeyRequestOutcome),    // Ответ не завершился, можно повторить то же решение.
}

impl PendingKeyGrant {
    /// Учитывает проверенные права и позволяет повторить только уже выбранный ответ.
    fn can_respond(&self, outcome: KeyRequestOutcome) -> bool {
        if self.state != GrantState::Pending && self.state != GrantState::Failed(outcome) {
            return false;
        }
        match outcome {
            KeyRequestOutcome::Granted => self.capabilities.can_grant,
            KeyRequestOutcome::Rejected => self.capabilities.can_reject,
        }
    }

    /// Использует UUID места запроса, темы и поколения без зависимости от видимого окна.
    fn target(&self) -> KeyRequestTarget {
        KeyRequestTarget {
            stream_id: self.hint.stream_id,
            topic_id: self.hint.topic_id,
            key_id: self.hint.key_id,
        }
    }

    /// Сопоставляет полную цель выдачи и устройство из текущего журнала.
    fn same_request(&self, other: &Self) -> bool {
        self.target() == other.target()
            && self.hint.request_id == other.hint.request_id
            && self.hint.device_id == other.hint.device_id
    }
}

/// Команда над уже показанным ключом или текущим поколением.
#[derive(Clone, Copy)]
enum Change {
    Remove(Uuid), // Удалить один локальный ключ, сохранив серверное поколение.
    Rotate(Uuid), // Выпустить новое поколение после показанного пользователю.
}

impl Conversation {
    /// Запрашивает метаданные только для настоящего открытия сведений.
    pub(super) fn load_key_management(&mut self, cx: &mut Context<Self>) {
        let (Some(panel), Some(encryption)) = (&mut self.details, &self.encryption) else {
            return;
        };
        if !matches!(panel.state, Some(State::Ready)) || !panel.is_present(cx) {
            return;
        }
        let request = Uuid::new_v4();
        match &panel.keys {
            KeyManagement::Ready(keys) if keys.operation.is_some() => return,
            KeyManagement::Loading(_) => return,
            _ => {}
        }
        let previous = match std::mem::replace(&mut panel.keys, KeyManagement::Loading(request)) {
            KeyManagement::Ready(keys) => Some(keys),
            _ => None,
        };
        let previous_results = previous
            .as_ref()
            .map_or_else(Vec::new, |keys| keys.bulk.clone());
        let activation = encryption.activation.clone();
        let engine = encryption.engine.clone();
        let runtime = panel.runtime.clone();
        let address = self.address.clone();
        cx.spawn(async move |view, cx| {
            let reply = runtime
                .conversation(address, move |services, address| async move {
                    let data = services
                        .messenger
                        .local_stream_keys(&services.sync, &activation, address.route.scope)
                        .await?;
                    let mut cursor = None;
                    let mut hints = Vec::new();
                    let device = activation.session().device_id.to_string();
                    loop {
                        let page = services
                            .sync
                            .key_service_requests(
                                &address.route.owner,
                                address.generation,
                                address.route.scope.stream_id,
                                cursor,
                            )
                            .map_err(|_| KeyLifecycleError::Changed)?;
                        hints.extend(
                            page.requests
                                .into_iter()
                                .filter(|hint| hint.device_id != device),
                        );
                        cursor = page.next;
                        if cursor.is_none() {
                            break;
                        }
                    }
                    let exchange = KeyExchange::new(services.messenger, services.sync, engine);
                    let mut bulk = stream::iter(hints)
                        .map(|hint| async {
                            let target = KeyRequestTarget {
                                stream_id: hint.stream_id,
                                topic_id: hint.topic_id,
                                key_id: hint.key_id,
                            };
                            let (author_id, capabilities, state) = match exchange
                                .key_request_status(&activation, target, hint.request_id)
                                .await
                                .ok()??
                            {
                                KeyRequestStatus::Pending {
                                    author_id,
                                    capabilities,
                                } => (author_id, capabilities, GrantState::Pending),
                                KeyRequestStatus::Completed { author_id, outcome } => (
                                    author_id,
                                    KeyReplyCapabilities::default(),
                                    GrantState::Completed(outcome),
                                ),
                            };
                            Some(PendingKeyGrant {
                                hint,
                                author_id,
                                capabilities,
                                state,
                            })
                        })
                        .buffered(8)
                        .filter_map(async |row| row)
                        .collect::<Vec<_>>()
                        .await;
                    for previous in previous_results {
                        if let Some(row) = bulk.iter_mut().find(|row| row.same_request(&previous))
                            && row.state == GrantState::Pending
                            && matches!(
                                previous.state,
                                GrantState::Completed(_) | GrantState::Failed(_)
                            )
                        {
                            // POST может получить подтверждение раньше, чем ответ появится в локальном индексе.
                            row.state = previous.state;
                        }
                    }
                    Ok::<_, KeyLifecycleError>((data, bulk))
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                let Some(panel) = view.details.as_mut().filter(|panel| panel.is_present(cx)) else {
                    return;
                };
                if !matches!(panel.keys, KeyManagement::Loading(id) if id == request) {
                    return;
                }
                panel.keys = match result {
                    Ok(Ok((data, bulk))) => {
                        let (
                            expanded,
                            request_sections,
                            mut key_focus,
                            requests,
                            rotate_focus,
                            remove_focus,
                        ) = previous
                            .filter(|keys| keys.data.topic_id == data.topic_id)
                            .map(|keys| {
                                (
                                    keys.expanded,
                                    keys.request_sections,
                                    keys.key_focus,
                                    keys.requests,
                                    keys.rotate_focus,
                                    keys.remove_focus,
                                )
                            })
                            .unwrap_or_else(|| {
                                (
                                    false,
                                    [false; 3],
                                    HashMap::new(),
                                    HashMap::new(),
                                    cx.focus_handle(),
                                    cx.focus_handle(),
                                )
                            });
                        key_focus.retain(|id, _| data.keys.iter().any(|key| key.key_id == *id));
                        for key in &data.keys {
                            key_focus
                                .entry(key.key_id)
                                .or_insert_with(|| cx.focus_handle());
                        }
                        KeyManagement::Ready(Box::new(KeyControls {
                            data,
                            expanded,
                            request_sections,
                            key_focus,
                            rotate_focus,
                            remove_focus,
                            operation: None,
                            error: false,
                            requests,
                            bulk,
                        }))
                    }
                    _ => KeyManagement::Failed,
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Отвечает на один запрос или выдаёт ключи по всем доступным запросам в канале.
    fn respond_to_sidebar_key_requests(
        &mut self,
        request_id: Option<Uuid>,
        outcome: KeyRequestOutcome,
        cx: &mut Context<Self>,
    ) {
        let (Some(panel), Some(encryption)) = (&mut self.details, &self.encryption) else {
            return;
        };
        if !panel.is_present(cx) {
            return;
        }
        let KeyManagement::Ready(keys) = &mut panel.keys else {
            return;
        };
        if keys.operation.is_some() {
            return;
        }
        let requests: Vec<_> = keys
            .bulk
            .iter_mut()
            .filter(|row| {
                request_id.is_none_or(|id| id == row.hint.request_id) && row.can_respond(outcome)
            })
            .map(|row| {
                row.state = GrantState::Sending;
                row.clone()
            })
            .collect();
        if requests.is_empty() {
            return;
        }
        let op_id = Uuid::new_v4();
        keys.operation = Some(op_id);
        keys.error = false;
        let runtime = panel.runtime.clone();
        let address = self.address.clone();
        let activation = encryption.activation.clone();
        let engine = encryption.engine.clone();
        cx.spawn(async move |entity, cx| {
            let reply = runtime
                .conversation(address, move |services, _| async move {
                    let exchange = KeyExchange::new(services.messenger, services.sync, engine);
                    stream::iter(requests)
                        .map(|mut row| async {
                            let sent = exchange
                                .respond_to_key_request(
                                    &activation,
                                    row.target(),
                                    row.hint.request_id,
                                    outcome,
                                )
                                .await
                                .is_ok();
                            row.state = if sent {
                                GrantState::Completed(outcome)
                            } else {
                                GrantState::Failed(outcome)
                            };
                            row
                        })
                        .buffered(8)
                        .collect::<Vec<_>>()
                        .await
                })
                .await;
            let _ = entity.update(cx, |view, cx| {
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                let Some(panel) = view.details.as_mut().filter(|panel| panel.is_present(cx)) else {
                    return;
                };
                let KeyManagement::Ready(keys) = &mut panel.keys else {
                    return;
                };
                if keys.operation != Some(op_id) {
                    return;
                }
                keys.operation = None;
                for row in keys
                    .bulk
                    .iter_mut()
                    .filter(|row| row.state == GrantState::Sending)
                {
                    row.state = result
                        .as_ref()
                        .ok()
                        .and_then(|resolved| {
                            resolved
                                .iter()
                                .find(|candidate| row.same_request(candidate))
                        })
                        .map_or(GrantState::Failed(outcome), |resolved| resolved.state);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Перечитывает метаданные видимого списка после локального изменения ключа.
    pub(in crate::gpui_client) fn refresh_key_management(&mut self, cx: &mut Context<Self>) {
        if self
            .details
            .as_ref()
            .is_some_and(|panel| matches!(panel.keys, KeyManagement::Ready(_)))
        {
            self.load_key_management(cx);
        }
    }

    /// Применяет только проверенный ответ для ожидающего запроса из этой панели.
    pub(in crate::gpui_client) fn key_management_receipt(
        &mut self,
        target: KeyRequestTarget,
        receipt: KeyRequestCompletion,
        cx: &mut Context<Self>,
    ) {
        if target.stream_id != self.address.route.scope.stream_id {
            return;
        }
        let Some(KeyManagement::Ready(keys)) = self.details.as_mut().map(|panel| &mut panel.keys)
        else {
            return;
        };
        if keys.data.topic_id != target.topic_id
            || keys.requests.get(&target.key_id) != Some(&RequestedKey::Waiting(receipt.request_id))
        {
            return;
        }
        if receipt.outcome == KeyRequestOutcome::Rejected {
            keys.requests.insert(target.key_id, RequestedKey::Rejected);
        } else {
            keys.requests.remove(&target.key_id);
            self.refresh_key_management(cx);
        }
        cx.notify();
    }

    /// Запрашивает указанное поколение в подтверждённой теме открытой панели.
    fn request_sidebar_key(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let (Some(panel), Some(encryption)) = (&mut self.details, &self.encryption) else {
            return;
        };
        if !panel.is_present(cx) || !matches!(panel.state, Some(State::Ready)) {
            return;
        }
        let KeyManagement::Ready(keys) = &mut panel.keys else {
            return;
        };
        if keys.operation.is_some()
            || matches!(keys.requests.get(&id), Some(RequestedKey::Waiting(_)))
            || (keys.data.current_key_id != id
                && !keys.data.keys.iter().any(|key| key.key_id == id))
        {
            return;
        }
        let target = KeyRequestTarget {
            stream_id: self.address.route.scope.stream_id,
            topic_id: keys.data.topic_id,
            key_id: id,
        };
        let op_id = Uuid::new_v4();
        keys.operation = Some(op_id);
        keys.error = false;
        keys.requests.remove(&id);
        let activation = encryption.activation.clone();
        let captured = activation.clone();
        let engine = encryption.engine.clone();
        let address = self.address.clone();
        let runtime = panel.runtime.clone();
        cx.spawn(async move |view, cx| {
            let reply = runtime
                .conversation(address, move |services, _| async move {
                    KeyExchange::new(services.messenger, services.sync, engine)
                        .request_key(&captured, target, "Workspace Desktop (Mac)")
                        .await
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                let Some(panel) = view.details.as_mut().filter(|panel| panel.is_present(cx)) else {
                    return;
                };
                let KeyManagement::Ready(keys) = &mut panel.keys else {
                    return;
                };
                if keys.operation != Some(op_id) {
                    return;
                }
                keys.operation = None;
                let completion = match result {
                    Ok(Ok(receipt)) => {
                        keys.requests
                            .insert(id, RequestedKey::Waiting(receipt.request_id));
                        runtime.key_request_completion(&activation, target, receipt.request_id)
                    }
                    _ => {
                        keys.error = true;
                        None
                    }
                };
                if let Some(completion) = completion {
                    view.key_management_receipt(target, completion, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Проверяет, что диалог всё ещё относится к открытому экрану и точному ключу.
    fn key_change_state(
        &self,
        change: Change,
        screen: Option<EncryptedScreen>,
        app: &App,
    ) -> Option<&KeyControls> {
        let panel = self.details.as_ref()?;
        if self.encryption.is_none()
            || !self.has_access()
            || !panel.is_present(app)
            || panel.gallery.is_some()
            || panel.encrypted_screen() != screen
            || !matches!(panel.state, Some(State::Ready))
        {
            return None;
        }
        let KeyManagement::Ready(keys) = &panel.keys else {
            return None;
        };
        let available = match change {
            Change::Remove(id) => {
                screen == Some(EncryptedScreen::LocalKey(id))
                    && keys.data.keys.iter().any(|key| key.key_id == id)
            }
            Change::Rotate(id) => {
                matches!(screen, None | Some(EncryptedScreen::Keys))
                    && keys.data.can_rotate
                    && keys.data.current_key_id == id
            }
        };
        available.then_some(keys.as_ref())
    }

    /// Проверяет ещё раз состояние перед отправкой подтверждённого действия в Core.
    fn apply_key_management(
        &mut self,
        change: Change,
        screen: Option<EncryptedScreen>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .key_change_state(change, screen, cx)
            .is_none_or(|keys| keys.operation.is_some())
        {
            return;
        }
        let (Some(panel), Some(encryption)) = (&mut self.details, &self.encryption) else {
            return;
        };
        let KeyManagement::Ready(keys) = &mut panel.keys else {
            return;
        };
        let op_id = Uuid::new_v4();
        keys.operation = Some(op_id);
        keys.error = false;
        let runtime = panel.runtime.clone();
        let address = self.address.clone();
        let engine = encryption.engine.clone();
        let activation = encryption.activation.clone();
        cx.spawn_in(window, async move |view, cx| {
            let reply = runtime
                .conversation(address, move |services, address| async move {
                    let scope = address.route.scope;
                    match change {
                        Change::Remove(id) => {
                            let current = services
                                .messenger
                                .local_stream_keys(&services.sync, &activation, scope)
                                .await?;
                            if !current.keys.iter().any(|key| key.key_id == id) {
                                return Ok(());
                            }
                            tokio::task::spawn_blocking(move || {
                                engine.remove_key(&activation, scope.stream_id, id)
                            })
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .ok_or(KeyLifecycleError::Changed)
                        }
                        Change::Rotate(id) => {
                            let operation = services
                                .messenger
                                .stage_confirmed_key_rotation(
                                    &services.sync,
                                    &activation,
                                    scope,
                                    id,
                                )
                                .await?;
                            services
                                .messenger
                                .rotate_encrypted_key(&services.sync, &activation, operation)
                                .await?;
                            Ok(())
                        }
                    }
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    let Some(result) = reply.for_current(Some(&view.address)) else {
                        return;
                    };
                    let Some(panel) = view.details.as_mut().filter(|panel| panel.is_present(cx))
                    else {
                        return;
                    };
                    let KeyManagement::Ready(keys) = &mut panel.keys else {
                        return;
                    };
                    if keys.operation != Some(op_id) {
                        return;
                    }
                    keys.operation = None;
                    keys.error = !matches!(result, Ok(Ok(())));
                    log::debug!(
                        "event=gpui_chat_keys_changed op_id={op_id} ok={}",
                        !keys.error
                    );
                    if !keys.error {
                        panel.host.update(cx, |host, cx| {
                            if host.modal_is::<KeyConfirmation>() {
                                host.close_modal(window, cx);
                            }
                        });
                        if let Change::Remove(id) = change {
                            view.return_from_encrypted_local_key(id, cx);
                        }
                        view.load_key_management(cx);
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }
}
