//! Показывает каждый защищённый фрагмент отдельным отзывным спойлером.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, IntoElement, Render, Task, WeakEntity, Window,
    div, prelude::*, px,
};
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;
use workspace_core::messenger::{
    encryption::{
        AcceptedDecryption, DecryptionError, EncryptionActivation, MessageEncryption,
        fragments::{FragmentError, FragmentMessageError, FragmentReference},
        key_exchange::{
            KeyExchange, KeyRequestCompletion, KeyRequestOutcome, KeyRequestReceipt,
            KeyRequestTarget,
        },
    },
    message_actions::MessageTarget,
};

use crate::gpui_client::{
    contracts::ConversationAddress,
    preferences::{Language, Preferences},
    runtime::RuntimeClient,
    ui::{Button, Icon, Palette, TextRole, label, text_role},
};

use super::{Conversation, State, viewport::GuardedText};

/// Сессия и маршрут, в которых строка показала ссылку.
#[derive(Clone)]
pub(super) struct FragmentRuntime {
    runtime: RuntimeClient,           // Ставит REST и Keychain в рабочую очередь.
    address: ConversationAddress,     // Текущий экран, в том числе при показе пересылки.
    engine: Arc<MessageEncryption>,   // Принимает результат под проверкой версии ключа.
    activation: EncryptionActivation, // Точный выбранный вход.
    view: WeakEntity<Conversation>,   // Сбрасывает высоту только зависимой строки.
}

impl Conversation {
    /// Привязывает фрагменты к текущему окну, используя тот же выбранный вход и исполнитель.
    pub(super) fn fragment_runtime(&self, cx: &Context<Self>) -> Option<FragmentRuntime> {
        let encryption = self.encryption.as_ref()?;
        Some(FragmentRuntime {
            runtime: self.avatar_runtime.clone()?,
            address: self.address.clone(),
            engine: encryption.engine.clone(),
            activation: encryption.activation.clone(),
            view: cx.entity().downgrade(),
        })
    }

    /// Возвращает открытые forwarding slots для проверки отзыва в GPUI-тестах.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_fragment_key_state(
        &self,
        stream: Uuid,
        key: Uuid,
        app: &App,
    ) -> Vec<(Uuid, bool, bool)> {
        self.viewport
            .fragment_views
            .tested_key_state(stream, key, app)
    }

    /// Проверяет исходную строку в Core и переносит правку только в оставшийся открытым редактор.
    pub(super) fn edit_fragments(
        &self,
        id: Uuid,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let (State::Ready(history), Some(sending), Some(runtime)) =
            (&self.state, &self.sending, self.fragment_runtime(cx))
        else {
            return;
        };
        let Some(expected) = history.messages.iter().find(|row| row.id == id) else {
            return;
        };
        if expected.body.markdown().is_none()
            || expected.metadata.author_id != self.address.route.owner.user_id.parse().ok()
            || expected.metadata.author_id.is_none()
            || sending.view.read(cx).fragment_edit_topic() != Some(expected.topic_id)
        {
            return;
        }
        let expected = expected.clone();
        let source = expected.clone();
        let activation = runtime.activation.clone();
        let target = MessageTarget {
            scope: runtime.address.route.scope,
            topic_id: expected.topic_id,
            message_id: expected.id,
        };
        let window = window.map(|window| window.window_handle());
        cx.spawn(async move |view, cx| {
            let reply = runtime.runtime.conversation(runtime.address, move |services, _| async move {
                services.messenger.open_fragment_edit(&services.sync, &activation, target, &source).await
            }).await;
            let apply = move |view: &mut Conversation, window: Option<&mut Window>, cx: &mut Context<Conversation>| {
                if view.address != reply.address
                    || !matches!(&view.state, State::Ready(history) if history.messages.contains(&expected))
                {
                    return;
                }
                let Some(sending) = &view.sending else { return; };
                sending.view.update(cx, |composer, cx| {
                    match reply.value {
                        Ok(Ok(edit)) => {
                            composer.open_fragment_edit(expected, edit, runtime.engine, runtime.activation, view.attachments.access.clone(), cx);
                            if let Some(window) = window {
                                composer.focus_editor(window, cx);
                            }
                        }
                        _ => composer.fragment_edit_unavailable(cx),
                    }
                });
            };
            if let Some(window) = window {
                let _ = window.update(cx, |_, window, app| {
                    let _ = view.update(app, |view, cx| apply(view, Some(window), cx));
                });
            } else {
                let _ = view.update(cx, |view, cx| apply(view, None, cx));
            }
        }).detach();
    }

    /// Выбирает последнюю свою обычную строку в той же теме подтверждённого хвоста.
    pub(super) fn edit_previous_fragment(&self, cx: &mut Context<Self>) {
        let (State::Ready(history), Some(sending)) = (&self.state, &self.sending) else {
            return;
        };
        let Some(topic_id) = sending.view.read(cx).fragment_edit_topic() else {
            return;
        };
        let Ok(owner) = self.address.route.owner.user_id.parse::<Uuid>() else {
            return;
        };
        if history.newer != workspace_core::storage::HistoryEdge::End {
            return;
        }
        if let Some(row) = history.messages.iter().rev().find(|row| {
            row.topic_id == topic_id
                && row.metadata.author_id == Some(owner)
                && row
                    .body
                    .markdown()
                    .is_some_and(|text| !text.trim().is_empty())
        }) {
            self.edit_fragments(row.id, None, cx);
        }
    }
}

/// Создаёт отдельный вид для видимого спойлера и переиспользует его при неизменном теле.
#[derive(Clone, Default)]
pub(super) struct FragmentViews(Rc<RefCell<FragmentEntries>>); // Стабильные слоты одного окна.

/// Сохраняет открытые элементы и адреса строк до следующего канонического снимка.
#[derive(Default)]
struct FragmentEntries {
    slots: HashMap<(Uuid, usize), FragmentEntry>, // Место слота в конкретной строке.
    awaiting_snapshot: HashSet<Uuid>, // Строки, чья каноническая версия ещё перечитывается.
}

/// Удерживает представление лишь пока в строке сохранён тот же конверт.
struct FragmentEntry {
    reference: FragmentReference, // Каноническая ссылка, без открытого текста.
    spoiler: Entity<FragmentSpoiler>, // Отдельный рендер с собственным фокусом и версиями.
}

impl FragmentViews {
    /// Оставляет состояния только у тех слотов, чей исходный конверт ещё на месте.
    pub(super) fn retain(&self, mut current: impl FnMut(Uuid, usize, &FragmentReference) -> bool) {
        let mut entries = self.0.borrow_mut();
        entries.awaiting_snapshot.clear();
        entries
            .slots
            .retain(|&(id, position), entry| current(id, position, &entry.reference));
    }

    /// Закрывает строку сразу по событию и отменяет работу с ушедшей версией.
    pub(super) fn invalidate_row(&self, id: Uuid, cx: &mut Context<Conversation>) {
        let mut entries = self.0.borrow_mut();
        entries.awaiting_snapshot.insert(id);
        entries.slots.retain(|&(row, _), entry| {
            if row != id {
                return true;
            }
            entry.spoiler.update(cx, FragmentSpoiler::revoke);
            false
        });
    }

    /// Отзывает открытые фрагменты пересылок, когда потерян доступ к исходному стриму.
    pub(super) fn invalidate_stream(
        &self,
        stream: Uuid,
        cx: &mut Context<Conversation>,
    ) -> Vec<Uuid> {
        let entries = self.0.borrow();
        let mut affected = HashSet::new();
        for (&(row, _), entry) in &entries.slots {
            if entry.reference.stream_id() == stream {
                entry.spoiler.update(cx, FragmentSpoiler::revoke);
                affected.insert(row);
            }
        }
        affected.into_iter().collect()
    }

    /// Закрывает фрагменты точного ключа и создаёт их заново после следующего кадра.
    pub(super) fn invalidate_key(
        &self,
        stream: Uuid,
        key: Uuid,
        cx: &mut Context<Conversation>,
    ) -> Vec<Uuid> {
        let mut entries = self.0.borrow_mut();
        let mut affected = HashSet::new();
        entries.slots.retain(|&(row, _), entry| {
            if (entry.reference.stream_id(), entry.reference.key_id()) != (stream, key) {
                return true;
            }
            entry.spoiler.update(cx, FragmentSpoiler::revoke);
            affected.insert(row);
            false
        });
        affected.into_iter().collect()
    }

    /// Возвращает слоты ключа и их раскрытие для GPUI-тестов отзыва.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn tested_key_state(
        &self,
        stream: Uuid,
        key: Uuid,
        app: &App,
    ) -> Vec<(Uuid, bool, bool)> {
        self.0
            .borrow()
            .slots
            .iter()
            .filter(|(_, entry)| {
                (entry.reference.stream_id(), entry.reference.key_id()) == (stream, key)
            })
            .map(|(&(row, _), entry)| {
                let spoiler = entry.spoiler.read(app);
                let has_secret_access = matches!(
                    &spoiler.state,
                    FragmentState::Ready(access) if access.with_text(|_| ()).is_some()
                );
                (row, spoiler.expanded, has_secret_access)
            })
            .collect()
    }

    /// Показывает отказ ровно тем слотам, где ждали подтверждённый ответ исходной темы.
    pub(super) fn receive(
        &self,
        target: KeyRequestTarget,
        completion: KeyRequestCompletion,
        cx: &mut Context<Conversation>,
    ) -> Vec<Uuid> {
        let entries = self.0.borrow();
        let mut affected = HashSet::new();
        for (&(row, _), entry) in &entries.slots {
            if (entry.reference.stream_id(), entry.reference.key_id())
                == (target.stream_id, target.key_id)
                && entry.spoiler.update(cx, |spoiler, cx| {
                    spoiler.apply_receipt(target, completion, cx)
                })
            {
                affected.insert(row);
            }
        }
        affected.into_iter().collect()
    }

    /// Возвращает стабильный элемент слота. Локальная ошибка не меняет соседние спойлеры.
    pub(super) fn element(
        &self,
        slot: (Uuid, usize),
        reference: Option<&FragmentReference>,
        runtime: Option<&FragmentRuntime>,
        palette: Palette,
        locale: Language,
        app: Option<&mut App>,
    ) -> AnyElement {
        let (Some(reference), Some(runtime), Some(app)) = (reference, runtime, app) else {
            return label(palette, TextRole::Caption, locale.tr("e2ee.secretFragment"))
                .into_any_element();
        };
        let mut entries = self.0.borrow_mut();
        if entries.awaiting_snapshot.contains(&slot.0) {
            return label(palette, TextRole::Caption, locale.tr("e2ee.secretFragment"))
                .into_any_element();
        }
        if let Some(entry) = entries.slots.get(&slot)
            && entry.reference == *reference
        {
            return entry.spoiler.clone().into_any_element();
        }
        let spoiler = app
            .new(|cx| FragmentSpoiler::new(runtime.clone(), slot.0, slot.1, reference.clone(), cx));
        entries.slots.insert(
            slot,
            FragmentEntry {
                reference: reference.clone(),
                spoiler: spoiler.clone(),
            },
        );
        spoiler.into_any_element()
    }
}

/// Публичное состояние спойлера и отзывный доступ к раскрытому тексту.
enum FragmentState {
    Loading,                        // Рабочий поток проверяет исходный чат и конверт.
    Locked,                         // Для поколения можно запросить ключ.
    Failed,                         // Хранилище или конверт не прочитались.
    Unavailable,                    // Нет доступа к исходному чату.
    Ready(Arc<AcceptedDecryption>), // Текст каждый кадр проходит проверку ключа.
}

/// Вид одного слота. Его жизненный цикл отделён от остальной строки истории.
struct FragmentSpoiler {
    runtime: FragmentRuntime,           // Выбранный вход и исходный экран.
    row: Uuid,                          // UUID бабла для перерасчёта его высоты.
    position: usize,                    // Позиция спойлера среди частей этой строки.
    reference: FragmentReference,       // Чат и ключ до возможной пересылки.
    state: FragmentState,               // Результат последней подтверждённой работы.
    expanded: bool,                     // Раскрытие сбрасывается при отзыве ключа и смене тела.
    requesting: bool,                   // Ожидается ответ на операцию запроса ключа.
    waiting: Option<KeyRequestReceipt>, // Публичный адрес принятого запроса.
    refused: bool,      // Получен проверенный отказ, запрос можно повторить вручную.
    revision: u64,      // Отбрасывает устаревшие сетевые ответы.
    focus: FocusHandle, // Одна клавиатурная остановка в этом спойлере.
    _watch: Task<()>,   // Слушает только точный чат и ключ.
}

impl FragmentSpoiler {
    /// Подписывается до первого чтения ключа и начинает с закрытого состояния.
    fn new(
        runtime: FragmentRuntime,
        row: Uuid,
        position: usize,
        reference: FragmentReference,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut changes = runtime.engine.subscribe();
        let activation = runtime.activation.clone();
        let (stream_id, key_id) = (reference.stream_id(), reference.key_id());
        let watch = cx.spawn(async move |entity, cx| {
            loop {
                let available = match changes.recv().await {
                    Ok(change)
                        if change.activation == activation
                            && (change.stream_id, change.key_id) == (stream_id, key_id) =>
                    {
                        change.available
                    }
                    Ok(_) => continue,
                    Err(RecvError::Lagged(_)) => true,
                    Err(RecvError::Closed) => return,
                };
                if entity
                    .update(cx, |this, cx| {
                        if available {
                            this.load(cx);
                        } else {
                            this.revision += 1;
                            this.expanded = false;
                            this.requesting = false;
                            this.waiting = None;
                            this.refused = false;
                            this.state = FragmentState::Locked;
                            this.invalidate(cx);
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        let mut spoiler = Self {
            runtime,
            row,
            position,
            reference,
            state: FragmentState::Loading,
            expanded: false,
            requesting: false,
            waiting: None,
            refused: false,
            revision: 0,
            focus: cx.focus_handle().tab_stop(true),
            _watch: watch,
        };
        spoiler.load(cx);
        spoiler
    }

    /// Закрывает спойлер и готовит новое представление только для исходного чата ссылки.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.revision += 1;
        let revision = self.revision;
        let runtime = self.runtime.clone();
        let activation = runtime.activation.clone();
        let reference = self.reference.clone();
        self.expanded = false;
        self.requesting = false;
        self.waiting = None;
        self.refused = false;
        self.state = FragmentState::Loading;
        if revision > 1 {
            self.invalidate(cx);
        }
        cx.spawn(async move |entity, cx| {
            let reply = runtime
                .runtime
                .conversation(runtime.address, move |services, _| async move {
                    services
                        .messenger
                        .prepare_linked_fragment(&services.sync, &activation, &reference)
                        .await
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                if this.revision != revision || this.runtime.address != reply.address {
                    return;
                }
                this.state = match reply.value {
                    Ok(Ok(result)) => match runtime.engine.accept_fragment(result) {
                        Ok(access) => FragmentState::Ready(Arc::new(access)),
                        Err(DecryptionError::Stale | DecryptionError::MissingKey) => {
                            FragmentState::Locked
                        }
                        Err(_) => FragmentState::Failed,
                    },
                    Ok(Err(FragmentMessageError::Fragment(FragmentError::Decryption(
                        DecryptionError::MissingKey,
                    )))) => FragmentState::Locked,
                    Ok(Err(FragmentMessageError::Source(_) | FragmentMessageError::Auth(_))) => {
                        FragmentState::Unavailable
                    }
                    _ => FragmentState::Failed,
                };
                this.invalidate(cx);
            });
        })
        .detach();
    }

    /// Запрашивает поколение в исходном чате, где оно было выпущено.
    fn request_key(&mut self, cx: &mut Context<Self>) {
        self.requesting = true;
        self.refused = false;
        let revision = self.revision;
        let runtime = self.runtime.clone();
        let activation = runtime.activation.clone();
        let reference = self.reference.clone();
        let engine = runtime.engine.clone();
        cx.notify();
        cx.spawn(async move |entity, cx| {
            let reply = runtime
                .runtime
                .conversation(runtime.address.clone(), move |services, _| async move {
                    let target = services
                        .messenger
                        .fragment_key_request_target(&services.sync, &activation, &reference)
                        .await
                        .map_err(|_| ())?;
                    KeyExchange::new(services.messenger, services.sync, engine)
                        .request_key(&activation, target, "Workspace Desktop")
                        .await
                        .map_err(|_| ())
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                if this.revision == revision && this.runtime.address == reply.address {
                    this.requesting = false;
                    match reply.value {
                        Ok(Ok(receipt)) => {
                            this.waiting = Some(receipt);
                            if let Some(completion) = runtime.runtime.key_request_completion(
                                &runtime.activation,
                                receipt.target,
                                receipt.request_id,
                            ) {
                                this.apply_receipt(receipt.target, completion, cx);
                            }
                        }
                        _ => this.state = FragmentState::Failed,
                    }
                    this.invalidate(cx);
                }
            });
        })
        .detach();
    }

    /// Закрывает содержимое без повторного сетевого запроса по отозванному маршруту.
    fn revoke(&mut self, cx: &mut Context<Self>) {
        self.revision += 1;
        self.expanded = false;
        self.requesting = false;
        self.waiting = None;
        self.refused = false;
        self.state = FragmentState::Unavailable;
        cx.notify();
    }

    /// Принимает подтверждённый отказ на ожидаемый запрос. Выдачу подхватывает событие ключа.
    fn apply_receipt(
        &mut self,
        target: KeyRequestTarget,
        completion: KeyRequestCompletion,
        cx: &mut Context<Self>,
    ) -> bool {
        if completion.outcome != KeyRequestOutcome::Rejected
            || self.waiting
                != Some(KeyRequestReceipt {
                    request_id: completion.request_id,
                    target,
                })
        {
            return false;
        }
        self.waiting = None;
        self.refused = true;
        cx.notify();
        true
    }

    /// Переизмеряет только собственный бабл и будит отдельный рендер спойлера.
    fn invalidate(&self, cx: &mut Context<Self>) {
        let _ = self
            .runtime
            .view
            .update(cx, |view, _| view.viewport.invalidate_row_height(self.row));
        cx.notify();
    }
}

impl Render for FragmentSpoiler {
    /// Показывает скрытый фрагмент или отзывный текст без передачи его в события элементов.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let locale = Preferences::read_for(cx, &self.runtime.address.route.owner).language;
        let (caption, action) = match &self.state {
            FragmentState::Ready(access) if access.with_text(|_| ()).is_some() => (
                if self.expanded {
                    locale.tr("e2ee.hideSecretFragment")
                } else {
                    locale.tr("e2ee.showSecretFragment")
                },
                1,
            ),
            FragmentState::Locked if self.waiting.is_some() => {
                (locale.tr("desktop.messenger.history.waitingForKey"), 0)
            }
            FragmentState::Locked if self.refused => (
                locale.tr("desktop.messenger.history.retryDeclinedFragmentRequest"),
                2,
            ),
            FragmentState::Locked | FragmentState::Ready(_) => (locale.tr("e2ee.requestAccess"), 2),
            FragmentState::Failed => (locale.tr("e2ee.retry"), 3),
            FragmentState::Loading | FragmentState::Unavailable => {
                (locale.tr("e2ee.secretFragment"), 0)
            }
        };
        let position = self.position;
        let row = self.row;
        let target = cx.entity().downgrade();
        let mut button = Button::new(("encrypted-fragment", cx.entity().entity_id()), caption);
        button.icon = Some(Icon::EncryptedSpoiler);
        button.disabled = action == 0 || self.requesting;
        let control = button
            .render(palette, Some(&self.focus), move |_, cx| {
                let _ = target.update(cx, |this, cx| match action {
                    1 => {
                        this.expanded = !this.expanded;
                        this.invalidate(cx);
                    }
                    2 => this.request_key(cx),
                    3 => this.load(cx),
                    _ => {}
                });
            })
            .debug_selector(move || format!("encrypted-fragment-{row}-{position}"))
            .when(action == 1, |button| {
                button.debug_selector(move || format!("encrypted-fragment-{row}-{position}-ready"))
            });
        let text = match &self.state {
            FragmentState::Ready(access) if self.expanded && action == 1 => Some(
                text_role(palette, TextRole::Body)
                    .max_w_full()
                    .min_w_0()
                    .child(GuardedText::fragment(access.clone()))
                    .debug_selector(move || {
                        format!("encrypted-fragment-{row}-{position}-revealed")
                    }),
            ),
            _ => None,
        };
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .max_w_full()
            .gap(px(2.))
            .child(control)
            .children(text)
    }
}
