//! Готовит представления защищённых сообщений вне потока GPUI и меняет только строки текущего окна.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use gpui::{BackgroundExecutor, ClipboardItem, Context, Task, Window};
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;
use workspace_core::{
    messenger::encryption::{
        DecryptionError, EncryptionActivation, MessageEncryption, ProtectedMessage,
    },
    storage::{
        HistoryEdge, HistoryMessage, HistoryWindow, MessageBody, OutboxEntry, PreparedSendBody,
    },
};

use super::{
    Conversation, State,
    key_exchange::{CheckedKeyRequest, KeyActions},
    protected_body::ProtectedLayout,
    protected_quotes::{ProtectedQuotes, QuoteSlot},
};

/// Видимое состояние тела, которое может меняться под тем же UUID.
#[derive(Clone)]
pub(super) enum EncryptedPresentation {
    Locked,                       // Точного ключа пока нет.
    Failed,                       // Хранилище или расшифровка вернули ошибку.
    Ready(Arc<ProtectedMessage>), // Отзывный материал для текущей отрисовки истории.
}

impl PartialEq for EncryptedPresentation {
    /// Сравнивает отображение без чтения секретного текста.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ready(left), Self::Ready(right)) => Arc::ptr_eq(left, right),
            (Self::Locked, Self::Locked) | (Self::Failed, Self::Failed) => true,
            _ => false,
        }
    }
}

impl Eq for EncryptedPresentation {}

/// Версия тела, для которой экран подготовил видимое состояние.
struct Presented {
    key_id: Uuid,                         // Поколение ключа из тела сообщения.
    ciphertext: String,                   // Точное тело для отбрасывания устаревшей подготовки.
    state: EncryptedPresentation, // Материал для окна, который удаляется при отзыве доступа.
    layout: Option<Arc<ProtectedLayout>>, // Диапазоны и ID без копии Markdown.
}

/// Результат подготовки одного сообщения перед возвратом в GPUI.
pub(super) struct PreparedBody {
    id: Uuid,                                        // Сообщение, где можно показать результат.
    key_id: Uuid,                                    // UUID точного ключа.
    ciphertext: String,                              // Тело на момент запуска.
    revision: u64,                                   // Ревизия наблюдения ключа при запуске.
    message_revision: u64, // Изменение тела в Sync может опередить новый снимок.
    result: Result<PreparedMarkup, DecryptionError>, // Разметка рассчитана после принятия Core.
}

/// Текст остаётся в отзываемом держателе, а UI получает только структуру вокруг него.
struct PreparedMarkup {
    access: Arc<ProtectedMessage>, // Результат с проверкой активации и поколения.
    layout: Arc<ProtectedLayout>,  // Только диапазоны и ID.
}

/// Отдельная задача принимает тело только для текущего открытого разговора.
#[derive(Clone)]
pub(super) struct PrepareEncrypted {
    engine: Arc<MessageEncryption>, // Читает группу ключей и проверяет криптографическое тело.
    activation: EncryptionActivation, // Выбранный вход и локальное устройство.
    stream_id: Uuid,                // Исходный разговор для целого сообщения.
    known: HashMap<Uuid, (Uuid, String)>, // Подготовленные версии, которые можно переиспользовать.
    key_revisions: HashMap<Uuid, u64>, // Ревизии известных изменений точных ключей.
    message_revisions: HashMap<Uuid, u64>, // Правки и удаления, о которых уже сообщил Sync.
    default_revision: u64,          // Ревизия неизвестного ключа после перечитывания подписки.
}

impl PrepareEncrypted {
    /// Готовит новое окно до создания разговора. Результат ещё проверит экран при показе.
    pub(super) fn for_opening(
        engine: Arc<MessageEncryption>,
        activation: EncryptionActivation,
        stream_id: Uuid,
    ) -> Self {
        Self {
            engine,
            activation,
            stream_id,
            known: HashMap::new(),
            key_revisions: HashMap::new(),
            message_revisions: HashMap::new(),
            default_revision: 0,
        }
    }

    /// Шифротекст уходит на исполнитель, а GPUI получает готовые к сверке результаты.
    pub(super) async fn window(
        self,
        window: &HistoryWindow,
        outbox: &[OutboxEntry],
        background: &BackgroundExecutor,
    ) -> Vec<PreparedBody> {
        let requests = ciphertexts(window, outbox)
            .filter_map(|(id, key_id, ciphertext)| {
                if self
                    .known
                    .get(&id)
                    .is_some_and(|(key, text)| *key == key_id && text == ciphertext)
                {
                    return None;
                }
                Some((id, key_id, ciphertext.to_owned()))
            })
            .collect::<Vec<_>>();
        self.messages(requests, background).await
    }

    /// Подготавливает только переданные UUID из текущей версии окна.
    async fn messages(
        self,
        requests: Vec<(Uuid, Uuid, String)>,
        background: &BackgroundExecutor,
    ) -> Vec<PreparedBody> {
        if requests.is_empty() {
            return Vec::new();
        }
        background
            .spawn(async move {
                requests
                    .into_iter()
                    .map(|(id, key_id, ciphertext)| {
                        let revision = self
                            .key_revisions
                            .get(&key_id)
                            .copied()
                            .unwrap_or(self.default_revision);
                        let result = self
                            .engine
                            .prepare(&self.activation, self.stream_id, key_id)
                            .and_then(|prepared| self.engine.accept(prepared.decrypt(&ciphertext)))
                            .and_then(|access| {
                                let layout = access
                                    .with_body(ProtectedLayout::parse)
                                    .ok_or(DecryptionError::Stale)?;
                                Ok(PreparedMarkup {
                                    access: Arc::new(access),
                                    layout: Arc::new(layout),
                                })
                            });
                        PreparedBody {
                            id,
                            key_id,
                            ciphertext,
                            revision,
                            message_revision: self
                                .message_revisions
                                .get(&id)
                                .copied()
                                .unwrap_or_default(),
                            result,
                        }
                    })
                    .collect()
            })
            .await
    }
}

/// Живые представления и подписка одного открытого экрана.
pub(super) struct HistoryEncryption {
    pub(super) engine: Arc<MessageEncryption>, // Общий владелец секретов рабочего профиля.
    pub(super) activation: EncryptionActivation, // Точная сессия экрана.
    pub(super) key_actions: KeyActions,        // Видимый ход действия возле исходной строки.
    pub(super) key_permissions: HashMap<Uuid, CheckedKeyRequest>, // Последние проверенные права выдачи и отказа.
    pub(super) quotes: ProtectedQuotes, // Проверенные исходники защищённых цитат.
    stream_id: Uuid,                    // Стрим открытой истории.
    by_id: HashMap<Uuid, Presented>,    // Текущие представления по стабильному UUID сообщения.
    key_revisions: HashMap<Uuid, u64>,  // Ревизии изменений ключей, уже обработанных экраном.
    message_revisions: HashMap<Uuid, u64>, // Ревизии тела сообщений по событиям Sync.
    pending_messages: HashSet<Uuid>, // Пока следующее чтение истории не подтвердило тело, оно закрыто.
    default_revision: u64,           // Сменится при пропуске события подписки.
    revision: u64,                   // Счётчик наблюдённых изменений.
    _watch: Task<()>,                // Закрытие экрана отменяет ожидание ключей.
}

impl HistoryEncryption {
    /// Подписывается до запроса истории, чтобы учесть ключи, изменённые при открытии.
    pub(super) fn new(
        engine: Arc<MessageEncryption>,
        activation: EncryptionActivation,
        stream_id: Uuid,
        cx: &mut Context<Conversation>,
    ) -> Self {
        let mut changes = engine.subscribe();
        let subscribed = activation.clone();
        let watch = cx.spawn(async move |target, cx| {
            loop {
                let event = match changes.recv().await {
                    Ok(change) if change.activation == subscribed => Some(change),
                    Ok(_) => continue,
                    Err(RecvError::Lagged(_)) => None,
                    Err(RecvError::Closed) => break,
                };
                if target
                    .update(cx, |view, cx| match event {
                        Some(change) => {
                            view.quoted_key_changed(change.stream_id, change.key_id, cx);
                            if change.stream_id == stream_id {
                                view.encrypted_key_changed(
                                    Some((change.key_id, change.available)),
                                    cx,
                                );
                            }
                        }
                        None => view.encrypted_key_changed(None, cx),
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            engine,
            activation,
            stream_id,
            key_actions: KeyActions::new(),
            key_permissions: HashMap::new(),
            quotes: ProtectedQuotes::default(),
            by_id: HashMap::new(),
            key_revisions: HashMap::new(),
            message_revisions: HashMap::new(),
            pending_messages: HashSet::new(),
            default_revision: 0,
            revision: 0,
            _watch: watch,
        }
    }

    /// Снимает только идентификаторы и шифротекст готовых строк перед загрузкой новой страницы.
    pub(super) fn prepare(&self) -> PrepareEncrypted {
        let mut prepared = self.prepare_for_navigation();
        prepared.known = self
            .by_id
            .iter()
            .map(|(id, body)| (*id, (body.key_id, body.ciphertext.clone())))
            .collect();
        prepared
    }

    /// Готовит новое окно без материалов, которые могли исчезнуть из прежней ленты.
    pub(super) fn prepare_for_navigation(&self) -> PrepareEncrypted {
        PrepareEncrypted {
            engine: Arc::clone(&self.engine),
            activation: self.activation.clone(),
            stream_id: self.stream_id,
            known: HashMap::new(),
            key_revisions: self.key_revisions.clone(),
            message_revisions: self.message_revisions.clone(),
            default_revision: self.default_revision,
        }
    }

    /// Возвращает версию защиты окна для проверки отложенного перехода.
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    /// Возвращает последнее состояние для строки снимка. Ожидание ключа отображается как блокировка.
    pub(super) fn presentation(&self, id: Uuid) -> Option<&EncryptedPresentation> {
        self.by_id.get(&id).map(|body| &body.state)
    }

    /// Возвращает точную разметку представлений действующего окна.
    pub(super) fn layouts(&self) -> HashMap<Uuid, Arc<ProtectedLayout>> {
        self.by_id
            .iter()
            .filter_map(|(id, body)| Some((*id, body.layout.clone()?)))
            .collect()
    }

    /// Выбирает ссылки из ещё раскрытых баблов, удерживая точную версию каждого.
    pub(super) fn quote_requests(&self) -> HashMap<QuoteSlot, (Uuid, Arc<ProtectedMessage>)> {
        self.by_id
            .iter()
            .filter_map(|(&message, row)| {
                let EncryptedPresentation::Ready(parent) = &row.state else {
                    return None;
                };
                parent.with_body(|_| ())?;
                Some(
                    row.layout
                        .as_ref()?
                        .quotes()
                        .enumerate()
                        .map(move |(position, source)| {
                            (QuoteSlot { message, position }, (source, parent.clone()))
                        }),
                )
            })
            .flatten()
            .collect()
    }

    /// Проверяет точный держатель и карточку перед открытием или повторным показом.
    pub(super) fn holds_file(
        &self,
        key: super::attachments::AttachmentKey,
        access: &Arc<ProtectedMessage>,
    ) -> bool {
        self.by_id.get(&key.message).is_some_and(|body| {
            matches!(&body.state, EncryptedPresentation::Ready(current) if Arc::ptr_eq(current, access))
                && body.layout.as_ref().is_some_and(|layout| layout.has_file(key))
                && access.with_body(|_| ()).is_some()
        })
    }

    /// Определяет адрес существующей защищённой карточки для сохранения фокуса.
    pub(super) fn has_file(&self, key: super::attachments::AttachmentKey) -> bool {
        self.by_id
            .get(&key.message)
            .and_then(|body| body.layout.as_ref())
            .is_some_and(|layout| layout.has_file(key))
    }

    /// Предупреждает запуск действий над телом, которое Sync уже заменил.
    pub(super) fn message_change_pending(&self, id: Uuid) -> bool {
        self.pending_messages.contains(&id)
    }

    /// Освобождает текст и отменяет результаты старых запросов при уходе с маршрута или отзыве доступа.
    pub(super) fn clear(&mut self) {
        self.quotes.clear();
        self.key_actions.clear();
        self.key_permissions.clear();
        self.message_revisions.clear();
        self.pending_messages.clear();
        self.invalidate(None);
    }

    /// Оставляет состояния только тех шифротекстов, которые продолжают принадлежать окну.
    pub(super) fn retain(&mut self, window: &HistoryWindow, outbox: &[OutboxEntry]) {
        self.key_actions
            .retain(|id, record| record.still_visible(*id, window));
        let current: HashMap<_, _> = ciphertexts(window, outbox)
            .map(|(id, key, text)| (id, (key, text)))
            .collect();
        self.by_id.retain(|id, body| {
            current.get(id).is_some_and(|(key, ciphertext)| {
                *key == body.key_id && *ciphertext == body.ciphertext
            })
        });
    }

    /// Подтверждает результат только для совпадающего ключа, строки и поколения сессии.
    pub(super) fn accept(
        &mut self,
        window: &HistoryWindow,
        outbox: &[OutboxEntry],
        bodies: Vec<PreparedBody>,
    ) -> Vec<(Uuid, EncryptedPresentation)> {
        if bodies.is_empty() {
            return Vec::new();
        }
        let current: HashMap<_, _> = ciphertexts(window, outbox)
            .map(|(id, key, text)| (id, (key, text)))
            .collect();
        let mut changed = Vec::new();
        for body in bodies {
            let revision = self
                .key_revisions
                .get(&body.key_id)
                .copied()
                .unwrap_or(self.default_revision);
            if body.revision != revision
                || body.message_revision
                    != self
                        .message_revisions
                        .get(&body.id)
                        .copied()
                        .unwrap_or_default()
                || current.get(&body.id) != Some(&(body.key_id, body.ciphertext.as_str()))
            {
                continue;
            }
            let (state, layout) = match body.result {
                Ok(text) if text.access.with_body(|_| ()).is_some() => {
                    (EncryptedPresentation::Ready(text.access), Some(text.layout))
                }
                Ok(_) => continue,
                Err(DecryptionError::MissingKey) => (EncryptedPresentation::Locked, None),
                Err(DecryptionError::InvalidCiphertext | DecryptionError::KeyStore(_)) => {
                    (EncryptedPresentation::Failed, None)
                }
                Err(DecryptionError::Stale) => continue,
            };
            if self.presentation(body.id) != Some(&state) {
                changed.push((body.id, state.clone()));
            }
            self.by_id.insert(
                body.id,
                Presented {
                    key_id: body.key_id,
                    ciphertext: body.ciphertext,
                    state,
                    layout,
                },
            );
            self.pending_messages.remove(&body.id);
        }
        changed
    }

    /// Отзывает видимые результаты и попытки того ключа, который изменился.
    fn invalidate(&mut self, key: Option<Uuid>) {
        self.revision += 1;
        if let Some(key) = key {
            self.key_revisions.insert(key, self.revision);
            self.by_id.retain(|_, body| body.key_id != key);
        } else {
            self.default_revision = self.revision;
            self.key_revisions.clear();
            self.by_id.clear();
        }
    }

    /// Закрывает строку сразу по событию Sync и отсеивает подготовку прежнего снимка.
    fn invalidate_message(&mut self, id: Uuid) {
        self.revision += 1;
        self.message_revisions.insert(id, self.revision);
        self.by_id.remove(&id);
        self.pending_messages.insert(id);
    }
}

impl Conversation {
    /// Копирует явно выбранный пользователем текст только у текущего раскрытого сообщения.
    pub(super) fn copy_protected_body(
        &self,
        id: Uuid,
        access: &Arc<ProtectedMessage>,
        cx: &mut Context<Self>,
    ) {
        if self.protected_action_source(id, access).is_some() {
            let _ = access.with_body(|markdown| {
                cx.write_to_clipboard(ClipboardItem::new_string(markdown.to_owned()));
            });
        }
    }

    /// Передаёт в обычный редактор точный исходный конверт для целого защищённого ответа.
    pub(super) fn reply_to_protected_body(
        &self,
        id: Uuid,
        access: Arc<ProtectedMessage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(original) = self.protected_action_source(id, &access) else {
            return;
        };
        let Some(sending) = &self.sending else { return };
        sending.view.update(cx, |composer, cx| {
            composer.reply_protected(original.clone(), access, window, cx)
        });
    }

    /// Не позволяет отложенному событию кнопки использовать holder от прежней версии строки.
    fn protected_action_source(
        &self,
        id: Uuid,
        access: &Arc<ProtectedMessage>,
    ) -> Option<&HistoryMessage> {
        let (State::Ready(history), Some(encryption)) = (&self.state, &self.encryption) else {
            return None;
        };
        if !matches!(
            encryption.presentation(id),
            Some(EncryptedPresentation::Ready(current)) if Arc::ptr_eq(current, access)
        ) {
            return None;
        }
        history.messages.iter().find(|row| row.id == id)
    }

    /// Открывает правку своего защищённого сообщения из действующего окна истории.
    pub(super) fn open_protected_edit(
        &self,
        id: Uuid,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let (State::Ready(history), Some(encryption), Some(sending)) =
            (&self.state, &self.encryption, &self.sending)
        else {
            return;
        };
        let Some(expected) = history.messages.iter().find(|row| row.id == id) else {
            return;
        };
        if expected.metadata.author_id != self.address.route.owner.user_id.parse().ok()
            || expected.metadata.author_id.is_none()
            || encrypted_body(expected).is_none()
        {
            return;
        }
        let Some(EncryptedPresentation::Ready(access)) = encryption.presentation(id) else {
            return;
        };
        sending.view.update(cx, |composer, cx| {
            composer.open_protected_edit(
                expected.clone(),
                access.clone(),
                self.attachments.access.clone(),
                cx,
            );
            if let Some(window) = window {
                composer.focus_editor(window, cx);
            }
        });
    }

    /// Выбирает последнюю свою подтверждённую строку в точной теме готового редактора.
    pub(super) fn edit_previous_protected(&self, cx: &mut Context<Self>) {
        let (State::Ready(history), Some(sending)) = (&self.state, &self.sending) else {
            return;
        };
        let Some(topic_id) = sending.view.read(cx).protected_send_topic() else {
            return;
        };
        let Ok(owner) = self.address.route.owner.user_id.parse::<Uuid>() else {
            return;
        };
        if history.newer != HistoryEdge::End {
            return;
        }
        if let Some(row) = history.messages.iter().rev().find(|row| {
            row.topic_id == topic_id
                && row.metadata.author_id == Some(owner)
                && encrypted_body(row).is_some()
        }) {
            self.open_protected_edit(row.id, None, cx);
        }
    }

    /// Известная правка или удаление скрывает старый текст до нового окна из SQLite.
    pub(super) fn encrypted_message_changed(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(encryption) = &mut self.encryption else {
            return;
        };
        let displayed = matches!(&self.state, State::Ready(window) if window.messages.iter().find(|row| row.id == id).and_then(encrypted_body).is_some());
        let action_removed = encryption.key_actions.remove(&id).is_some();
        let permission_removed = encryption.key_permissions.remove(&id).is_some();
        let quote_rows = encryption.quotes.invalidate_message(id);
        if action_removed || permission_removed {
            self.viewport.invalidate_row_height(id);
        }
        if displayed || self.active.is_some() {
            encryption.invalidate_message(id);
        }
        if displayed && let Some(sending) = &self.sending {
            sending.view.update(cx, |composer, cx| {
                composer.cancel_protected_edit(Some(id), cx);
                composer.revoke_protected_replies(Some(id), cx);
            });
        }
        let body_changed = displayed
            && self
                .viewport
                .apply_encryption([(id, EncryptedPresentation::Locked)]);
        if action_removed || permission_removed || body_changed {
            cx.notify();
        }
        self.refresh_quote_rows(quote_rows, cx);
    }

    /// Обрабатывает дельту ключа отдельно от дерева истории и очереди отправки.
    fn encrypted_key_changed(&mut self, event: Option<(Uuid, bool)>, cx: &mut Context<Self>) {
        // Изменение одного ключа не обязательно отзывает остальные; пропуск событий требует закрыть поле.
        self.refresh_local_key(event.is_some(), cx);
        let Some(encryption) = &mut self.encryption else {
            return;
        };
        if let Some(sending) = &self.sending {
            sending.view.update(cx, |composer, cx| {
                composer.revoke_protected_replies(None, cx);
                composer.refresh_encrypted_destination(&sending.source, cx);
            });
        }
        let key = event.map(|(key, _)| key);
        encryption.invalidate(key);
        if event.is_none() {
            encryption.quotes.clear();
        }
        self.attachments.invalidate_revoked(cx);
        let actions_before = encryption.key_actions.len();
        encryption
            .key_actions
            .retain(|_, record| !record.uses_key(key));
        let actions_changed = encryption.key_actions.len() != actions_before;
        let State::Ready(window) = &self.state else {
            return;
        };
        let requests: Vec<_> = ciphertexts(window, &self.outbox)
            .filter(|(id, key_id, _)| {
                key.is_none_or(|key| *key_id == key) && !encryption.pending_messages.contains(id)
            })
            .map(|(id, key_id, ciphertext)| (id, key_id, ciphertext.to_owned()))
            .collect();
        let permissions_changed = self.reset_key_reply_permissions(key);
        let changed = self.viewport.apply_encryption(
            requests
                .iter()
                .map(|(id, ..)| (*id, EncryptedPresentation::Locked)),
        );
        if event.is_none_or(|(_, available)| available) {
            self.prepare_encrypted_rows(requests, cx);
        }
        if changed || actions_changed || permissions_changed {
            cx.notify();
        }
        self.check_key_reply_permissions(cx);
        self.refresh_key_management(cx);
        self.resolve_protected_quotes(cx);
    }

    /// Снимает устаревшие возможности с тех строк, которых коснулось изменение ключа или роли.
    pub(super) fn reset_key_reply_permissions(&mut self, key: Option<Uuid>) -> bool {
        let Some(encryption) = &mut self.encryption else {
            return false;
        };
        let mut removed = false;
        encryption.key_permissions.retain(|id, entry| {
            let stale = key.is_none_or(|key| key == entry.target.key_id);
            if stale && entry.permission.is_some_and(|p| p.grant || p.reject) {
                self.viewport.invalidate_row_height(*id);
                removed = true;
            }
            !stale
        });
        removed
    }

    /// Передаёт Core повтор той же защищённой попытки только для строки текущего хвоста.
    pub(super) fn retry_protected_send(&mut self, canonical: Uuid, cx: &mut Context<Self>) {
        let Some(runtime) = self.avatar_runtime.clone() else {
            return;
        };
        if !self.has_access()
            || !self.outbox.iter().any(|entry| {
                entry.snapshot.canonical_id == canonical && entry.snapshot.body.markdown().is_none()
            })
        {
            return;
        }
        let address = self.address.clone();
        cx.spawn(async move |target, cx| {
            let reply = runtime
                .conversation(address, move |services, address| async move {
                    services
                        .sync
                        .retry_encrypted_send(&address.route.owner, address.generation, canonical)
                        .unwrap_or(false)
                })
                .await;
            let _ = target.update(cx, |view, cx| {
                if reply
                    .for_current(Some(&view.address))
                    .is_some_and(|result| result.unwrap_or(false))
                {
                    view.refresh_outbox(cx);
                }
            });
        })
        .detach();
    }

    /// Перечитывает конкретный шифротекст после нажатия «Повторить».
    pub(super) fn retry_encrypted(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self
            .encryption
            .as_ref()
            .is_some_and(|encryption| encryption.pending_messages.contains(&id))
        {
            return;
        }
        let State::Ready(window) = &self.state else {
            return;
        };
        if let Some((_, key, ciphertext)) =
            ciphertexts(window, &self.outbox).find(|(current, _, _)| *current == id)
        {
            self.prepare_encrypted_rows(vec![(id, key, ciphertext.to_owned())], cx);
        }
    }

    /// Ставит на подготовку только строки без актуального состояния после принятия страницы.
    pub(super) fn prepare_unresolved_rows(&self, cx: &mut Context<Self>) {
        let (State::Ready(window), Some(encryption)) = (&self.state, &self.encryption) else {
            return;
        };
        let requests = ciphertexts(window, &self.outbox)
            .filter(|(id, _, _)| {
                encryption.presentation(*id).is_none() && !encryption.pending_messages.contains(id)
            })
            .map(|(id, key, ciphertext)| (id, key, ciphertext.to_owned()))
            .collect();
        self.prepare_encrypted_rows(requests, cx);
    }

    /// Принимает фоновый ответ только для открытого окна, без обновления композера и панели.
    fn prepare_encrypted_rows(&self, requests: Vec<(Uuid, Uuid, String)>, cx: &mut Context<Self>) {
        if requests.is_empty() {
            return;
        }
        let Some(encryption) = &self.encryption else {
            return;
        };
        let prepare = encryption.prepare();
        let background = cx.background_executor().clone();
        cx.spawn(async move |target, cx| {
            let bodies = prepare.messages(requests, &background).await;
            let _ = target.update(cx, |view, cx| {
                let (State::Ready(window), Some(encryption)) = (&view.state, &mut view.encryption)
                else {
                    return;
                };
                let changes = encryption.accept(window, &view.outbox, bodies);
                if view.viewport.apply_encryption(changes) {
                    cx.notify();
                }
                view.resolve_protected_quotes(cx);
            });
        })
        .detach();
    }
}

/// Даёт подготовке и проверке те же UUID, которые может показать история и её локальный хвост.
pub(super) fn ciphertexts<'a>(
    window: &'a HistoryWindow,
    outbox: &'a [OutboxEntry],
) -> impl Iterator<Item = (Uuid, Uuid, &'a str)> {
    let pending = outbox.iter().filter_map(|entry| {
        let id = entry.snapshot.public_id;
        if window.newer != HistoryEdge::End
            || window.messages.iter().any(|message| message.id == id)
        {
            return None;
        }
        let PreparedSendBody::Encrypted {
            key_id, ciphertext, ..
        } = &entry.snapshot.body
        else {
            return None;
        };
        Some((id, *key_id, ciphertext.as_str()))
    });
    window
        .messages
        .iter()
        .filter_map(|message| {
            let (key, text) = encrypted_body(message)?;
            Some((message.id, key, text))
        })
        .chain(pending)
}

/// Извлекает точный UUID поколения и непрозрачный шифротекст.
fn encrypted_body(message: &HistoryMessage) -> Option<(Uuid, &str)> {
    match &message.body {
        MessageBody::Encrypted { key_id, ciphertext } => Some((*key_id, ciphertext)),
        MessageBody::Markdown(_) | MessageBody::KeyService { .. } => None,
    }
}
