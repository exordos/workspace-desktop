//! Хранит точки чтения недавних разговоров в памяти окна и удаляет устаревшие записи.

use std::collections::VecDeque;

use uuid::Uuid;
use workspace_core::{
    context::{OwnerContext, SessionGeneration},
    storage::HistoryScope,
};

use crate::gpui_client::{
    contracts::{AccountSession, ConversationAddress},
    conversation::ReadingAnchor,
};

const MAX_POSITIONS: usize = 32;

/// Различает темы одного канала и разные входы в один аккаунт.
#[derive(PartialEq, Eq)]
struct Key {
    session: AccountSession, // Подтверждённый вход, которому принадлежит история.
    scope: HistoryScope,     // Разговор или конкретная тема.
}

impl From<&ConversationAddress> for Key {
    /// Берёт устойчивую область чтения из адреса открытия.
    fn from(address: &ConversationAddress) -> Self {
        Self {
            session: AccountSession {
                owner: address.route.owner.clone(),
                generation: address.generation,
            },
            scope: address.route.scope,
        }
    }
}

/// Недавние точки чтения, от самой старой до самой новой.
#[derive(Default)]
pub(super) struct HistoryPositions {
    entries: VecDeque<(Key, ReadingAnchor)>, // Разговоры и точки чтения в порядке сохранения.
}

impl HistoryPositions {
    /// Передаёт сохранённую позицию новому экрану того же разговора.
    pub(super) fn take(&mut self, address: &ConversationAddress) -> Option<ReadingAnchor> {
        let key = Key::from(address);
        let index = self.entries.iter().position(|(stored, _)| *stored == key)?;
        self.entries.remove(index).map(|(_, anchor)| anchor)
    }

    /// Сохраняет опору при уходе с экрана. Для хвоста достаточно штатного открытия последних.
    pub(super) fn save(&mut self, address: &ConversationAddress, anchor: ReadingAnchor) {
        self.take(address);
        self.entries.push_back((Key::from(address), anchor));
        if self.entries.len() > MAX_POSITIONS {
            self.entries.pop_front();
        }
    }

    /// Оставляет только позиции действующих поколений авторизации.
    pub(super) fn keep_sessions(
        &mut self,
        keep: impl Fn(&OwnerContext, SessionGeneration) -> bool,
    ) {
        self.entries
            .retain(|(key, _)| keep(&key.session.owner, key.session.generation));
    }

    /// Удаляет точку отозванной темы и общего вида канала, где тема тоже могла быть видна.
    pub(super) fn revoke(
        &mut self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        topic_id: Option<Uuid>,
    ) {
        self.entries.retain(|(key, _)| {
            !(key.session.owner == *owner
                && key.session.generation == generation
                && key.scope.stream_id == stream_id
                && (topic_id.is_none()
                    || key.scope.topic_id.is_none()
                    || key.scope.topic_id == topic_id))
        });
    }
}
