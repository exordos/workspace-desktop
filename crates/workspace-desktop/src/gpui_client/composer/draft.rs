//! Связывает поле ввода и вкладки с локальным снимком. Буфер рабочего потока
//! переживает смену экрана, пока запись или передача снимка очереди не завершилась.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use gpui::Context;
use tokio::sync::Notify;
use uuid::Uuid;
use workspace_core::{
    context::{OwnerContext, SessionGeneration},
    storage::{DraftAccess, DraftScope, DraftVersion, PlaintextDraft},
    sync::{Sync, SyncError},
};

use super::{Composer, Editor};
use crate::gpui_client::contracts::ConversationAddress;

/// Локальная область снимка и поколение, в котором она подтверждена.
#[derive(Clone, Eq, Hash, PartialEq)]
struct Key {
    owner: OwnerContext,           // Полный владелец.
    generation: SessionGeneration, // Поколение входа, подтвердившего доступ.
    stream: Uuid,                  // Открытый стрим.
    topic: Uuid,                   // Явная тема, проверенная сервером.
}

impl Key {
    /// Связывает адрес окна с подтверждённой темой.
    fn new(address: &ConversationAddress, scope: DraftScope) -> Self {
        Self {
            owner: address.route.owner.clone(),
            generation: address.generation,
            stream: scope.stream_id,
            topic: scope.topic_id,
        }
    }
}

/// Снимок, который можно записывать после закрытия породившего его окна.
#[derive(Clone)]
struct Pending {
    access: DraftAccess,   // Версия доступа, с которой набран ввод.
    value: PlaintextDraft, // Самый свежий ввод, ещё не подтверждённый на диске.
}

/// Операции одной темы, которые переживают экран.
#[derive(Default)]
struct Slot {
    pending: Option<Pending>, // Последний снимок, включая явную очистку.
    writer: bool,             // Уже поставлен запрос, который сможет записать этот слот.
    send: Option<Uuid>,       // Идентификатор операции передачи редактора очереди.
}

/// Состояние таймера и очереди записи, общее для окон одного профиля.
#[derive(Default)]
pub(in crate::gpui_client) struct DraftBuffer {
    slots: Mutex<HashMap<Key, Slot>>, // Сохраняем по подтверждённым адресам и поколениям.
    settled: Notify, // Пробуждает повторное открытие после завершения записи очереди.
}

impl DraftBuffer {
    /// Сразу принимает новый ввод. Фоновый запрос для этой темы может быть только один.
    fn stage(&self, key: Key, pending: Pending) -> bool {
        let mut slots = self.slots.lock().unwrap();
        let slot = slots.entry(key).or_default();
        slot.pending = Some(pending);
        !std::mem::replace(&mut slot.writer, true)
    }

    /// Даёт черновику закончить передачу в очередь перед повторным открытием.
    pub(super) async fn wait_for_send(&self, address: &ConversationAddress, scope: DraftScope) {
        let key = Key::new(address, scope);
        loop {
            let changed = self.settled.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let sending = self
                .slots
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|slot| slot.send.is_some());
            if !sending {
                return;
            }
            changed.await;
        }
    }

    /// Снимает последний ввод до чтения SQLite. Если запись успеет завершиться
    /// между этими шагами, вызывающий всё равно увидит самую свежую из двух версий.
    pub(super) fn current(
        &self,
        address: &ConversationAddress,
        scope: DraftScope,
    ) -> Option<(DraftAccess, PlaintextDraft)> {
        let key = Key::new(address, scope);
        self.slots
            .lock()
            .unwrap()
            .get(&key)
            .and_then(|slot| slot.pending.as_ref())
            .map(|pending| (pending.access.clone(), pending.value.clone()))
    }

    /// Записывает последний ввод темы. Буфер удерживает его при временной ошибке.
    fn write(&self, key: &Key, sync: &Sync) -> bool {
        loop {
            let pending = self
                .slots
                .lock()
                .unwrap()
                .get(key)
                .and_then(|slot| slot.pending.clone());
            if let Some(pending) = &pending {
                let result =
                    sync.save_draft(&key.owner, key.generation, &pending.access, &pending.value);
                if matches!(result, Err(SyncError::Storage(error)) if !matches!(error, workspace_core::storage::StorageError::StaleDraftAccess))
                {
                    return false;
                }
            }
            let mut slots = self.slots.lock().unwrap();
            let Some(slot) = slots.get_mut(key) else {
                return true;
            };
            let same = match (&slot.pending, &pending) {
                (Some(current), Some(previous)) => {
                    current.access == previous.access
                        && current.value.id == previous.value.id
                        && current.value.version == previous.value.version
                }
                (None, None) => true,
                _ => false,
            };
            if !same {
                continue;
            }
            slot.pending = None;
            slot.writer = false;
            if slot.send.is_none() {
                slots.remove(key);
            }
            return true;
        }
    }

    /// Разрешает следующему вводу или штатному выходу повторить локальную запись.
    fn release_writer(&self, key: &Key) {
        if let Some(slot) = self.slots.lock().unwrap().get_mut(key) {
            slot.writer = false;
        }
    }

    /// Завершает оставшиеся локальные записи перед остановкой рабочего потока.
    pub(in crate::gpui_client) fn flush(&self, sync: &Sync) {
        let keys: Vec<_> = self.slots.lock().unwrap().keys().cloned().collect();
        for key in keys {
            if !self.write(&key, sync) {
                log::warn!("event=workspace_composer_draft_flush result=unavailable");
            }
        }
    }

    /// Скрывает снимок от нового окна до результата записи очереди.
    fn start_send(
        self: &Arc<Self>,
        key: Key,
        access: DraftAccess,
        version: DraftVersion,
        op_id: Uuid,
    ) -> Arc<DraftSend> {
        self.slots
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .send = Some(op_id);
        Arc::new(DraftSend {
            buffer: self.clone(),
            key,
            access,
            version,
            op_id,
        })
    }

    /// Убирает недоступные буферы после подтверждённого отзыва.
    pub(in crate::gpui_client) fn revoke(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream: Uuid,
        topic: Option<Uuid>,
    ) {
        self.slots.lock().unwrap().retain(|key, _| {
            key.owner != *owner
                || key.generation != generation
                || key.stream != stream
                || topic.is_some_and(|topic| key.topic != topic)
        });
        self.settled.notify_waiters();
    }

    /// Удаляет снимки завершённых поколений, сохраняя данные действующих аккаунтов.
    pub(in crate::gpui_client) fn keep_sessions(
        &self,
        keep: impl Fn(&OwnerContext, SessionGeneration) -> bool,
    ) {
        self.slots
            .lock()
            .unwrap()
            .retain(|key, _| keep(&key.owner, key.generation));
        self.settled.notify_waiters();
    }
}

/// Неизменная локальная версия, которую передаём очереди отправки.
pub(super) struct DraftSend {
    buffer: Arc<DraftBuffer>,         // Рабочий буфер исходной темы.
    key: Key,                         // Тема и поколение исходного поля.
    pub(super) access: DraftAccess,   // Подтверждённая область для атомарной записи.
    pub(super) version: DraftVersion, // Точный снимок, скрываемый по завершении передачи.
    op_id: Uuid,                      // Идентификатор операции, которая держит слот.
}

impl DraftSend {
    /// Перед завершением снимает отправленную версию из буфера. Новая версия остаётся доступной.
    pub(super) fn finish(&self, saved: bool) {
        let mut slots = self.buffer.slots.lock().unwrap();
        if let Some(slot) = slots.get_mut(&self.key)
            && slot.send == Some(self.op_id)
        {
            if saved
                && slot.pending.as_ref().is_some_and(|pending| {
                    pending.value.id == self.version.id
                        && pending.value.version <= self.version.version
                })
            {
                slot.pending = None;
            }
            slot.send = None;
            if slot.pending.is_none() && !slot.writer {
                slots.remove(&self.key);
            }
        }
        drop(slots);
        self.buffer.settled.notify_waiters();
    }
}

impl Drop for DraftSend {
    /// Освобождает восстановление, если запись так и не дошла до рабочего потока.
    fn drop(&mut self) {
        self.finish(false);
    }
}

/// Подтверждённый черновик одного окна, связанный с версиями его редактора.
pub(super) struct DraftSession {
    access: DraftAccess,       // Подтверждение владельца и темы.
    id: Uuid,                  // UUID записи, переживающий закрытие окна.
    version: u64,              // Последняя версия SQLite или ещё ожидающего снимка.
    stamp: Option<(u64, u64)>, // Версии поля и вкладок, для которых уже снят снимок.
}

impl Composer {
    /// Восстанавливает ввод только если с начала чтения его не редактировали.
    pub(super) fn hydrate_draft(
        &mut self,
        access: DraftAccess,
        saved: Option<PlaintextDraft>,
        stamp_at_start: (u64, u64),
        cx: &mut Context<Self>,
    ) {
        let mut session = DraftSession {
            access,
            id: saved.as_ref().map_or_else(Uuid::new_v4, |value| value.id),
            version: saved.as_ref().map_or(0, |value| value.version),
            stamp: None,
        };
        let editor = self.editor.read(cx);
        if (editor.version(), self.replies.revision) == stamp_at_start
            && !editor.composing()
            && editor.text().is_empty()
            && self.replies.sources().is_empty()
        {
            if let Some(saved) = saved {
                let text = self.replies.restore_draft(saved, cx);
                self.editor
                    .update(cx, |editor, cx| editor.show_reply(&text, cx));
                cx.emit(super::ComposerEvent::DraftRestored);
            }
            session.stamp = Some((self.editor.read(cx).version(), self.replies.revision));
        }
        self.draft_session = Some(session);
        self.persist_draft(cx);
    }

    /// Ставит подтверждённый ввод в буфер до того, как окно может сменить маршрут.
    pub(super) fn persist_draft(&mut self, cx: &mut Context<Self>) {
        let Some(runtime) = &self.draft_runtime else {
            return;
        };
        let Some(session) = &mut self.draft_session else {
            return;
        };
        let editor = self.editor.read(cx);
        let stamp = (editor.version(), self.replies.revision);
        if editor.composing() || session.stamp == Some(stamp) {
            return;
        }
        session.version += 1;
        session.stamp = Some(stamp);
        let key = Key::new(&self.address, session.access.scope());
        let (markdown, replies) = self.replies.draft_snapshot(editor.text());
        let value = PlaintextDraft {
            id: session.id,
            version: session.version,
            markdown,
            replies,
        };
        log::debug!(
            "event=workspace_composer_draft_staged stream_id={} topic_id={} version={}",
            key.stream,
            key.topic,
            value.version
        );
        let buffer = runtime.drafts.clone();
        if !buffer.stage(
            key.clone(),
            Pending {
                access: session.access.clone(),
                value,
            },
        ) {
            return;
        }
        let runtime = runtime.clone();
        cx.spawn(async move |_, cx| {
            for delay in [0, 60, 180] {
                if delay != 0 {
                    cx.background_executor()
                        .timer(Duration::from_millis(delay))
                        .await;
                }
                let buffer = buffer.clone();
                let worker_key = key.clone();
                let done = runtime
                    .request(
                        move |services| async move { buffer.write(&worker_key, &services.sync) },
                    )
                    .await;
                if matches!(done, Ok(true)) {
                    return;
                }
            }
            buffer.release_writer(&key);
            log::warn!("event=workspace_composer_draft_write result=unavailable");
        })
        .detach();
    }

    /// Фиксирует снимок отправки до передачи фоновой операции экрану.
    pub(super) fn prepare_draft_send(
        &mut self,
        op_id: Uuid,
        cx: &mut Context<Self>,
    ) -> Option<Arc<DraftSend>> {
        self.persist_draft(cx);
        let session = self.draft_session.as_ref()?;
        let runtime = self.draft_runtime.as_ref()?;
        let version = DraftVersion {
            id: session.id,
            version: session.version,
        };
        (version.version != 0).then(|| {
            runtime.drafts.start_send(
                Key::new(&self.address, session.access.scope()),
                session.access.clone(),
                version,
                op_id,
            )
        })
    }

    /// Очищает поле лички только при отзыве темы, к которой оно было привязано.
    pub(in crate::gpui_client) fn revoke_draft_topic(
        &mut self,
        topic: Uuid,
        cx: &mut Context<Self>,
    ) {
        if self.draft_topic == Some(topic) {
            self.revoke_draft(cx);
        }
    }

    /// Убирает ввод и подтверждение сохранения при явном отзыве доступа.
    pub(in crate::gpui_client) fn revoke_draft(&mut self, cx: &mut Context<Self>) {
        self.draft_session = None;
        self.draft_topic = None;
        self.replies.clear();
        self.editor.update(cx, Editor::clear);
        cx.notify();
    }
}
