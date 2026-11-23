//! Показывает сведения о локальных ключах и запускает управление точным поколением из панели чата.

use futures::{StreamExt, stream};
use gpui::{Context, Div, div, prelude::*};
use uuid::Uuid;
use workspace_core::messenger::encryption::{
    key_exchange::{KeyExchange, KeyRequestCompletion, KeyRequestOutcome, KeyRequestTarget},
    lifecycle::{KeyLifecycleError, KeyLifecycleIntent, LocalStreamKeys},
};

use super::{Conversation, State, action};
use crate::gpui_client::{
    preferences::Preferences,
    ui::{Button, Palette, TextRole, label},
};
use workspace_core::storage::KeyRequestHint;

/// Состояние ключей для текущего открытия панели. Хранит только UUID и разрешённые действия.
#[derive(Default)]
pub(super) enum KeyManagement {
    #[default]
    Hidden, // Обычный разговор или сервер ещё не подтвердил режим.
    Loading(Uuid),      // Чтение списка для этого открытия.
    Ready(KeyControls), // Есть сведения о ключах для показанного пользователя.
}

/// Видимые локальные ключи и действие, которое уже запустили или ожидаем подтвердить.
pub(super) struct KeyControls {
    data: LocalStreamKeys, // Подтверждённое поколение, право ротации и локальные UUID.
    expanded: bool,        // Развёрнут ли список поколений этого чата.
    remove: Option<Uuid>,  // Поколение, удаления которого пользователь ещё не подтвердил.
    rotate: bool,          // Пользователь должен явно подтвердить смену поколения.
    operation: Option<Uuid>, // Активный вызов, к которому относится поздний ответ.
    error: bool,           // Последнее действие не получило подтверждения.
    request: Option<Uuid>, // Публичный ID ожидающего запроса текущего поколения.
    rejected: bool,        // Ранее подтверждённый отказ, после которого можно запросить ключ снова.
    bulk: Vec<PendingKeyGrant>, // Известные запросы всех тем и отдельные результаты выдачи.
}

/// Строка массовой выдачи адресована точному запросу из журнала сообщений.
#[derive(Clone)]
struct PendingKeyGrant {
    hint: KeyRequestHint, // Подтверждённое размещение и имя устройства из истории.
    state: GrantState,    // Решение или состояние доставки для одного запроса.
}

/// Что происходит с одной строкой доступного набора.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GrantState {
    Available, // Core подтвердил возможность отправки.
    Sending,   // Запущена явная операция выдачи.
    Granted,   // Сервер подтвердил размещение ответа.
    Failed,    // Выдача не завершилась и может быть повторно проверена.
}

impl PendingKeyGrant {
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
    Rotate,       // Выпустить поколение штатным процессом Core.
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
        let current = match &panel.keys {
            KeyManagement::Ready(keys) if keys.operation.is_none() => Some((
                keys.expanded,
                keys.data.topic_id,
                keys.data.current_key_id,
                keys.request,
                keys.rejected,
            )),
            KeyManagement::Ready(_) => return,
            KeyManagement::Hidden => None,
            KeyManagement::Loading(_) => return,
        };
        let request = Uuid::new_v4();
        let previous_results = match &panel.keys {
            KeyManagement::Ready(keys) => keys.bulk.clone(),
            _ => Vec::new(),
        };
        panel.keys = KeyManagement::Loading(request);
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
                            let row = PendingKeyGrant {
                                hint,
                                state: GrantState::Available,
                            };
                            let permission = exchange
                                .key_reply_capabilities(
                                    &activation,
                                    row.target(),
                                    row.hint.request_id,
                                )
                                .await
                                .ok()?;
                            permission.can_grant.then_some(row)
                        })
                        .buffered(8)
                        .filter_map(async |row| row)
                        .collect::<Vec<_>>()
                        .await;
                    for previous in previous_results {
                        if !matches!(previous.state, GrantState::Granted | GrantState::Failed) {
                            continue;
                        }
                        if let Some(row) = bulk.iter_mut().find(|row| row.same_request(&previous)) {
                            row.state = previous.state;
                        } else if previous.state == GrantState::Granted {
                            bulk.push(previous);
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
                        let waiting = current
                            .filter(|(_, topic, key, _, _)| {
                                *topic == data.topic_id && *key == data.current_key_id
                            })
                            .filter(|_| {
                                data.keys
                                    .iter()
                                    .all(|key| key.key_id != data.current_key_id)
                            });
                        KeyManagement::Ready(KeyControls {
                            data,
                            expanded: current.is_some_and(|(expanded, ..)| expanded),
                            remove: None,
                            rotate: false,
                            operation: None,
                            error: false,
                            request: waiting.and_then(|(_, _, _, request, _)| request),
                            rejected: waiting.is_some_and(|(_, _, _, _, rejected)| rejected),
                            bulk,
                        })
                    }
                    _ => KeyManagement::Hidden,
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Выдаёт только прошедшие последнюю проверку запросы, независимо от открытой темы.
    fn bulk_grant_keys(&mut self, cx: &mut Context<Self>) {
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
            .filter_map(|row| {
                matches!(row.state, GrantState::Available | GrantState::Failed).then(|| {
                    row.state = GrantState::Sending;
                    row.clone()
                })
            })
            .collect();
        if requests.is_empty() {
            return;
        }
        let op_id = Uuid::new_v4();
        keys.operation = Some(op_id);
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
                            let shared = exchange
                                .respond_to_key_request(
                                    &activation,
                                    row.target(),
                                    row.hint.request_id,
                                    KeyRequestOutcome::Granted,
                                )
                                .await
                                .is_ok();
                            row.state = if shared {
                                GrantState::Granted
                            } else {
                                GrantState::Failed
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
                        .map_or(GrantState::Failed, |resolved| resolved.state);
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
            || keys.data.current_key_id != target.key_id
            || keys.request != Some(receipt.request_id)
        {
            return;
        }
        keys.request = None;
        if receipt.outcome == KeyRequestOutcome::Rejected {
            keys.rejected = true;
        } else {
            self.refresh_key_management(cx);
        }
        cx.notify();
    }

    /// Просит недостающий текущий ключ для подтверждённой темы даже при пустой истории.
    fn request_current_key(&mut self, cx: &mut Context<Self>) {
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
            || keys.request.is_some()
            || keys
                .data
                .keys
                .iter()
                .any(|key| key.key_id == keys.data.current_key_id)
        {
            return;
        }
        let target = KeyRequestTarget {
            stream_id: self.address.route.scope.stream_id,
            topic_id: keys.data.topic_id,
            key_id: keys.data.current_key_id,
        };
        let op_id = Uuid::new_v4();
        keys.operation = Some(op_id);
        keys.error = false;
        keys.rejected = false;
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
                        keys.request = Some(receipt.request_id);
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

    /// Проверяет, что команда адресована списку из того же открытия панели.
    fn apply_key_management(&mut self, change: Change, cx: &mut Context<Self>) {
        let (Some(panel), Some(encryption)) = (&mut self.details, &self.encryption) else {
            return;
        };
        if !panel.is_present(cx) || !matches!(panel.state, Some(State::Ready)) {
            return;
        }
        let KeyManagement::Ready(keys) = &mut panel.keys else {
            return;
        };
        if keys.operation.is_some() {
            return;
        }
        match change {
            Change::Remove(id)
                if keys.remove == Some(id) && keys.data.keys.iter().any(|key| key.key_id == id) => {
            }
            Change::Rotate if keys.rotate && keys.data.can_rotate => {}
            _ => return,
        }
        let op_id = Uuid::new_v4();
        keys.operation = Some(op_id);
        keys.error = false;
        let runtime = panel.runtime.clone();
        let address = self.address.clone();
        let engine = encryption.engine.clone();
        let activation = encryption.activation.clone();
        cx.spawn(async move |view, cx| {
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
                        Change::Rotate => {
                            let pending = services.messenger.pending_key_lifecycle(&services.sync, &activation)?
                                .into_iter().find(|operation| {
                                    operation.stream_id == scope.stream_id && matches!(
                                        operation.intent, KeyLifecycleIntent::Rotate { topic_id, .. }
                                            if scope.topic_id.is_none_or(|current| current == topic_id)
                                    )
                                });
                            let operation = match pending {
                                Some(operation) => operation.operation_id,
                                None => services.messenger.stage_key_rotation(&services.sync, &activation, scope).await?,
                            };
                            services
                                .messenger
                                .rotate_encrypted_key(&services.sync, &activation, operation)
                                .await?;
                            Ok(())
                        }
                    }
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
                keys.error = !matches!(result, Ok(Ok(())));
                log::debug!(
                    "event=gpui_chat_keys_changed op_id={op_id} ok={}",
                    !keys.error
                );
                if !keys.error {
                    view.load_key_management(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl KeyManagement {
    /// Рисует список и подтверждения только после проверки нативного защищённого чата.
    pub(super) fn render(&self, palette: Palette, cx: &Context<Conversation>) -> Option<Div> {
        let Self::Ready(keys) = self else {
            return None;
        };
        let locale = Preferences::read(cx).language;
        let text = |ru, en| locale.text(ru, en);
        let mut group = div()
            .flex()
            .flex_none()
            .flex_col()
            .items_start()
            .gap_1()
            .px_2()
            .py_2()
            .child(
                Button::new("conversation-chat-keys", text("Ключи чата", "Chat keys"))
                    .render(
                        palette,
                        None,
                        action(cx, |view, _, cx| {
                            let Some(KeyManagement::Ready(keys)) =
                                view.details.as_mut().map(|panel| &mut panel.keys)
                            else {
                                return;
                            };
                            keys.expanded = !keys.expanded;
                            cx.notify();
                        }),
                    )
                    .debug_selector(|| "conversation-chat-keys".into()),
            );
        if !keys.expanded {
            return Some(group);
        }
        group = group.child(label(
            palette,
            TextRole::Caption,
            format!(
                "{} · {}",
                text("Текущий ключ", "Current key"),
                keys.data.current_key_id
            ),
        ));
        if keys.data.keys.is_empty() {
            group = group.child(label(
                palette,
                TextRole::Body,
                text(
                    "На этом устройстве пока нет ключей",
                    "This device has no keys yet",
                ),
            ));
        }
        let busy = keys.operation.is_some();
        if !keys
            .data
            .keys
            .iter()
            .any(|key| key.key_id == keys.data.current_key_id)
        {
            let mut request = Button::new(
                "request-current-chat-key",
                text("Запросить текущий ключ", "Request current key"),
            );
            request.disabled = busy || keys.request.is_some();
            group = group.child(
                request
                    .render(
                        palette,
                        None,
                        action(cx, |view, _, cx| view.request_current_key(cx)),
                    )
                    .debug_selector(|| "request-current-chat-key".into()),
            );
            if keys.request.is_some() {
                group = group.child(label(
                    palette,
                    TextRole::Caption,
                    text("Ожидаем ответа на запрос", "Awaiting a key reply"),
                ));
            } else if keys.rejected {
                group = group.child(label(
                    palette,
                    TextRole::Caption,
                    text("В выдаче ключа отказано", "Key request declined"),
                ));
            }
        }
        let mut held = div()
            .id("conversation-held-keys")
            .flex()
            .flex_col()
            .w_full()
            .gap_1()
            .max_h(gpui::px(160.))
            .overflow_y_scroll();
        for key in &keys.data.keys {
            let id = key.key_id;
            let confirm = keys.remove == Some(id);
            let name = if confirm {
                text("Подтвердить удаление", "Confirm removal")
            } else {
                text("Удалить с устройства", "Remove from device")
            };
            let mut button = Button::new((gpui::ElementId::from(id), "remove-chat-key"), name);
            button.disabled = busy;
            held = held.child(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap_1()
                    .child(label(palette, TextRole::Caption, id.to_string()))
                    .children(key.added_at_unix_ms.map(|added_at| {
                        label(
                            palette,
                            TextRole::Caption,
                            crate::platform::chat_keys::key_date(added_at),
                        )
                    }))
                    .child(
                        button
                            .render(
                                palette,
                                None,
                                action(cx, move |view, _, cx| {
                                    if confirm {
                                        view.apply_key_management(Change::Remove(id), cx);
                                    } else if let Some(KeyManagement::Ready(keys)) =
                                        view.details.as_mut().map(|panel| &mut panel.keys)
                                    {
                                        keys.remove = Some(id);
                                        keys.rotate = false;
                                        cx.notify();
                                    }
                                }),
                            )
                            .debug_selector(move || format!("conversation-remove-key-{id}")),
                    ),
            );
        }
        group = group.child(held);
        if keys.data.can_rotate {
            let name = if keys.rotate {
                text("Подтвердить смену ключа", "Confirm key rotation")
            } else {
                text("Сменить ключ", "Rotate key")
            };
            let confirm = keys.rotate;
            let mut button = Button::new("conversation-rotate-key", name);
            button.disabled = busy;
            group = group.child(
                button
                    .render(
                        palette,
                        None,
                        action(cx, move |view, _, cx| {
                            if confirm {
                                view.apply_key_management(Change::Rotate, cx);
                            } else if let Some(KeyManagement::Ready(keys)) =
                                view.details.as_mut().map(|panel| &mut panel.keys)
                            {
                                keys.rotate = true;
                                keys.remove = None;
                                cx.notify();
                            }
                        }),
                    )
                    .debug_selector(|| "conversation-rotate-key".into()),
            );
        }
        if keys.error {
            group = group.child(label(
                palette,
                TextRole::Caption,
                text(
                    "Не удалось подтвердить действие. Повторите попытку",
                    "Action could not be confirmed. Try again",
                ),
            ));
        }
        Some(group.children(bulk_controls(&keys.bulk, busy, palette, cx)))
    }
}

/// Показывает доступную массовую выдачу и отдельный результат для каждого устройства.
fn bulk_controls(
    rows: &[PendingKeyGrant],
    busy: bool,
    palette: Palette,
    cx: &Context<Conversation>,
) -> Option<Div> {
    if rows.is_empty() {
        return None;
    }
    let locale = Preferences::read(cx).language;
    let t = |ru, en| locale.text(ru, en);
    let count = rows
        .iter()
        .filter(|row| matches!(row.state, GrantState::Available | GrantState::Failed))
        .count();
    let mut list = div()
        .id("conversation-bulk-key-requests")
        .max_h(gpui::px(160.))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap_1()
        .child(label(
            palette,
            TextRole::Label,
            t("Запросы ключей", "Key requests"),
        ));
    if count > 0 {
        let mut button = Button::new(
            "bulk-grant-chat-keys",
            format!(
                "{} · {count}",
                t("Выдать доступные ключи", "Share available keys")
            ),
        );
        button.disabled = busy;
        list = list.child(
            button
                .render(
                    palette,
                    None,
                    action(cx, |view, _, cx| view.bulk_grant_keys(cx)),
                )
                .debug_selector(|| "bulk-grant-chat-keys".into()),
        );
    }
    for row in rows {
        let id = row.hint.request_id;
        let state = row.state;
        let status = match row.state {
            GrantState::Sending => t("Отправляем…", "Sending…"),
            GrantState::Granted => t("Выдан", "Granted"),
            GrantState::Failed => t("Не удалось выдать", "Could not share"),
            GrantState::Available => t("Ожидает ответа", "Awaiting a reply"),
        };
        list = list.child(
            div()
                .flex()
                .justify_between()
                .gap_1()
                .debug_selector(move || format!("bulk-key-result-{id}"))
                .child(label(palette, TextRole::Body, row.hint.device_name.clone()))
                .child(
                    label(palette, TextRole::Caption, status).debug_selector(move || {
                        if state == GrantState::Granted {
                            format!("bulk-key-granted-{id}")
                        } else {
                            format!("bulk-key-pending-{id}")
                        }
                    }),
                ),
        );
    }
    Some(div().min_w_0().child(list))
}
