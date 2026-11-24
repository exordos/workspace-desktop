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
use workspace_core::messenger::send::PlainSendTarget;
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
    access: DraftAccess,                     // Версия доступа, с которой набран ввод.
    value: PlaintextDraft,                   // Самый свежий ввод, ещё не подтверждённый на диске.
    literal_target: Option<PlainSendTarget>, // Проверка внешнего формата, если есть маркеры.
}

/// Адрес прежнего обычного черновика для записи метки удаления.
#[derive(Clone)]
struct PendingPurge {
    access: DraftAccess,     // Подтверждённая область.
    version: DraftVersion,   // UUID и версия, до которой старый обычный текст нужно удалить.
    target: PlainSendTarget, // Подтверждённая нативная тема.
}

/// Операции одной темы, которые переживают экран.
#[derive(Default)]
struct Slot {
    pending: Option<Pending>,   // Последний снимок, включая явную очистку.
    protected: Option<Pending>, // Закрытый снимок только для восстановления поля в памяти.
    purges: std::collections::VecDeque<PendingPurge>, // Адреса до фиксации удаления в Core.
    writer: bool,               // Уже поставлен запрос, который сможет записать этот слот.
    send: Option<Uuid>,         // Идентификатор операции передачи редактора очереди.
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
        slot.protected = None;
        slot.pending = Some(pending);
        !std::mem::replace(&mut slot.writer, true)
    }

    /// Сразу убирает текст из обычной очереди и оставляет память редактора и адрес удаления.
    fn stage_protected(&self, key: Key, pending: Pending, native: Option<PlainSendTarget>) -> bool {
        let mut slots = self.slots.lock().unwrap();
        let slot = slots.entry(key).or_default();
        if let Some(target) = native
            && slot
                .protected
                .as_ref()
                .is_none_or(|old| old.value.id != pending.value.id)
        {
            slot.purges.push_back(PendingPurge {
                target,
                access: pending.access.clone(),
                version: DraftVersion {
                    id: pending.value.id,
                    version: pending.value.version,
                },
            });
        }
        slot.pending = None;
        slot.protected = Some(pending);
        !slot.purges.is_empty() && !std::mem::replace(&mut slot.writer, true)
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
    ) -> Option<(DraftAccess, PlaintextDraft, bool)> {
        let key = Key::new(address, scope);
        self.slots.lock().unwrap().get(&key).and_then(|slot| {
            let pending = slot.protected.as_ref().or(slot.pending.as_ref())?;
            Some((
                pending.access.clone(),
                pending.value.clone(),
                slot.protected.is_some(),
            ))
        })
    }

    /// Записывает последний ввод темы. Буфер удерживает его при временной ошибке.
    fn write(&self, key: &Key, sync: &Sync) -> bool {
        loop {
            let (purge, pending) = self
                .slots
                .lock()
                .unwrap()
                .get(key)
                .map(|slot| (slot.purges.front().cloned(), slot.pending.clone()))
                .unwrap_or_default();
            if let Some(purge) = purge {
                let result = sync.protect_draft(&purge.target, &purge.access, purge.version);
                if matches!(result, Err(SyncError::Storage(error)) if !matches!(error, workspace_core::storage::StorageError::StaleDraftAccess))
                {
                    return false;
                }
                let mut slots = self.slots.lock().unwrap();
                if let Some(slot) = slots.get_mut(key)
                    && slot.purges.front().is_some_and(|current| {
                        current.access == purge.access
                            && current.version.id == purge.version.id
                            && current.version.version == purge.version.version
                    })
                {
                    slot.purges.pop_front();
                }
                continue;
            }
            if let Some(pending) = &pending {
                let result = if let Some(target) = &pending.literal_target {
                    sync.save_external_draft(target, &pending.access, &pending.value)
                } else {
                    sync.save_draft(&key.owner, key.generation, &pending.access, &pending.value)
                };
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
            if !same || !slot.purges.is_empty() {
                continue;
            }
            slot.pending = None;
            slot.writer = false;
            if slot.send.is_none() && slot.protected.is_none() {
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
        if !self.flush_matching(sync, |_| true) {
            log::warn!("event=workspace_composer_draft_flush result=unavailable");
        }
    }

    /// Сохраняет последние правки выбранного входа перед чтением общей страницы черновиков.
    pub(in crate::gpui_client) fn flush_session(
        &self,
        sync: &Sync,
        session: &crate::gpui_client::contracts::AccountSession,
    ) -> bool {
        self.flush_matching(sync, |key| {
            key.owner == session.owner && key.generation == session.generation
        })
    }

    /// Завершает выбранные записи и сообщает об ошибке даже при частичном успехе.
    fn flush_matching(&self, sync: &Sync, matches: impl Fn(&Key) -> bool) -> bool {
        let keys: Vec<_> = self
            .slots
            .lock()
            .unwrap()
            .keys()
            .filter(|key| matches(key))
            .cloned()
            .collect();
        let mut complete = true;
        for key in keys {
            complete &= self.write(&key, sync);
        }
        complete
    }

    /// Сверяет ожидающие записи и удерживает их неизменными во время удаления из SQLite.
    pub(in crate::gpui_client) fn delete_visible_version(
        &self,
        sync: &Sync,
        session: &crate::gpui_client::contracts::AccountSession,
        scope: DraftScope,
        version: DraftVersion,
    ) -> Result<bool, SyncError> {
        let key = Key {
            owner: session.owner.clone(),
            generation: session.generation,
            stream: scope.stream_id,
            topic: scope.topic_id,
        };
        let slots = self.slots.lock().unwrap();
        if slots.get(&key).is_some_and(|slot| {
            slot.send.is_some()
                || slot.pending.as_ref().is_some_and(|pending| {
                    pending.value.id == version.id && pending.value.version > version.version
                })
        }) {
            return Ok(false);
        }
        sync.delete_local_draft(&session.owner, session.generation, scope, version)
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
    /// Подтверждает очистку предыдущей обычной копии перед постановкой шифртекста в очередь.
    pub(super) fn flush_protection(&self, sync: &Sync) -> bool {
        self.buffer.write(&self.key, sync)
    }

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
            if saved
                && slot.protected.as_ref().is_some_and(|pending| {
                    pending.value.id == self.version.id
                        && pending.value.version <= self.version.version
                })
            {
                slot.protected = None;
            }
            slot.send = None;
            if slot.pending.is_none()
                && slot.protected.is_none()
                && slot.purges.is_empty()
                && !slot.writer
            {
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
    access: DraftAccess,        // Подтверждение владельца и темы.
    id: Uuid,                   // UUID записи, переживающий закрытие окна.
    version: u64,               // Последняя версия SQLite или ещё ожидающего снимка.
    stamp: Option<(u64, u64)>,  // Версии поля и вкладок, для которых уже снят снимок.
    pub(super) protected: bool, // UUID прежней копии закрыт для обычных записей.
}

impl Composer {
    /// Восстанавливает ввод только если с начала чтения его не редактировали.
    pub(super) fn hydrate_draft(
        &mut self,
        access: DraftAccess,
        saved: Option<PlaintextDraft>,
        restored_protected: bool,
        stamp_at_start: (u64, u64),
        cx: &mut Context<Self>,
    ) {
        let mut session = DraftSession {
            access,
            id: saved.as_ref().map_or_else(Uuid::new_v4, |value| value.id),
            version: saved.as_ref().map_or(0, |value| value.version),
            stamp: None,
            protected: restored_protected,
        };
        let editor = self.editor.read(cx);
        if (editor.version(), self.replies.revision) == stamp_at_start
            && !editor.composing()
            && editor.text().is_empty()
            && self.replies.sources().is_empty()
        {
            let newly_protected = !restored_protected
                && self
                    .plain_target
                    .as_ref()
                    .is_some_and(PlainSendTarget::fragments_allowed)
                && saved
                    .as_ref()
                    .is_some_and(super::fragments::is_protected_draft);
            if let Some(saved) = saved {
                let text = self.replies.restore_draft(saved, cx);
                self.editor
                    .update(cx, |editor, cx| editor.show_reply(&text, cx));
                cx.emit(super::ComposerEvent::DraftRestored);
            }
            if !newly_protected {
                session.stamp = Some((self.editor.read(cx).version(), self.replies.revision));
            }
        }
        self.draft_session = Some(session);
        self.persist_draft(cx);
    }

    /// Ставит подтверждённый ввод в буфер до того, как окно может сменить маршрут.
    pub(super) fn persist_draft(&mut self, cx: &mut Context<Self>) {
        if self.edit.is_some() {
            return;
        }
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
        let key = Key::new(&self.address, session.access.scope());
        let (markdown, replies) = self.replies.draft_snapshot(editor.text());
        let mut value = PlaintextDraft {
            id: session.id,
            version: session.version + 1,
            markdown,
            replies,
        };
        let marked = super::fragments::is_protected_draft(&value);
        let native = self
            .plain_target
            .as_ref()
            .filter(|target| target.fragments_allowed());
        let literal = self
            .plain_target
            .as_ref()
            .filter(|target| target.literal_markers_allowed());
        if (marked || session.protected) && native.is_none() && literal.is_none() {
            return;
        }
        let protected = marked && (session.protected || native.is_some());
        session.version = value.version;
        session.stamp = Some(stamp);
        if session.protected && !protected {
            // Снятие защиты открывает новый черновик, не оживляя очищенный UUID.
            session.id = Uuid::new_v4();
            session.version = 1;
            value.id = session.id;
            value.version = session.version;
        }
        session.protected = protected;
        log::debug!(
            "event=workspace_composer_draft_staged stream_id={} topic_id={} version={}",
            key.stream,
            key.topic,
            value.version
        );
        let buffer = runtime.drafts.clone();
        let pending = Pending {
            access: session.access.clone(),
            value,
            literal_target: if marked && !protected {
                literal.cloned()
            } else {
                None
            },
        };
        let should_write = if protected {
            buffer.stage_protected(key.clone(), pending, native.cloned())
        } else {
            buffer.stage(key.clone(), pending)
        };
        if !should_write {
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
        // У чистого серверного снимка ещё нет локальной версии для связи с очередью.
        if let Some(session) = self.draft_session.as_mut()
            && session.version == 0
        {
            session.stamp = None;
        }
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

#[cfg(test)]
mod protected_tests {
    use super::*;

    /// Первый маркер убирает текст из обычной очереди до любого обращения к базе.
    #[test]
    fn protected_draft_moves_pending_text_to_editor_memory_before_disk() {
        let storage = workspace_core::storage::Storage::in_memory().unwrap();
        let owner = OwnerContext {
            account_id: workspace_core::context::AccountId("draft-owner".into()),
            server: "https://workspace.test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        };
        let scope = DraftScope {
            stream_id: Uuid::new_v4(),
            topic_id: Uuid::new_v4(),
        };
        let access = storage.prepare_draft_access(&owner, scope).unwrap();
        let key = Key {
            owner,
            generation: SessionGeneration(4),
            stream: scope.stream_id,
            topic: scope.topic_id,
        };
        let buffer = DraftBuffer::default();
        let id = Uuid::new_v4();
        let snapshot = |version, markdown: &str| Pending {
            access: access.clone(),
            literal_target: None,
            value: PlaintextDraft {
                id,
                version,
                markdown: markdown.into(),
                replies: None,
            },
        };
        assert!(buffer.stage(key.clone(), snapshot(1, "обычный")));
        assert!(!buffer.stage_protected(key.clone(), snapshot(2, ":::незаконченный"), None));
        assert!(!buffer.stage_protected(key.clone(), snapshot(3, ":::открытый фрагмент:::"), None));
        let slots = buffer.slots.lock().unwrap();
        let slot = &slots[&key];
        assert!(slot.pending.is_none());
        assert_eq!(slot.protected.as_ref().unwrap().value.version, 3);
        assert!(slot.purges.is_empty());
    }
}
