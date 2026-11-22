//! Определяет видимые строки каталога, раскрытие тем и результаты локального поиска.
//! Получает данные мессенджера и текущий маршрут, не меняя ни то, ни другое.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;
use workspace_core::{
    messenger::catalog::{Chat, Topic},
    storage::HistoryScope,
};

const INITIAL_TOPIC_COUNT: usize = 3;

/// Независимые ступени показа тем внутри раскрытого канала.
#[derive(Clone, Copy, Default)]
enum Reveal {
    #[default]
    Initial, // Первые три темы.
    Unfinished, // Все незавершённые и уже показанные завершённые темы.
    All,        // Все темы.
}

/// Следующее действие внутри карточки и число тем, которое оно откроет.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TopicToggleKind {
    ShowMore(usize),      // Показать ещё столько тем.
    ShowCompleted(usize), // Показать столько завершённых тем.
    HideExtra,            // Вернуться к начальному набору.
}

/// Видимая группа строк, которую окно может отрисовать и связать с фокусом.
pub(super) struct ChatGroup<'a> {
    pub(super) chat: &'a Chat,                  // Родительская строка.
    pub(super) expanded: bool, // Вложенная часть видна или её принудительно открыл поиск.
    pub(super) selected: bool, // Родитель несёт выделение маршрута, если нужная тема скрыта.
    pub(super) topics: Vec<&'a Topic>, // Вложенные строки в порядке показа.
    pub(super) divider_before: Option<usize>, // Индекс первой из остальных тем в поиске.
    pub(super) toggle: Option<TopicToggleKind>, // Нет, если дополнительных ступеней нет.
}

/// Куда вернуть фокус после изменения списка тем.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum TopicToggleFocus {
    Topic(Uuid), // UUID первой вновь видимой темы.
    Button,      // Кнопка внутри карточки при сворачивании списка.
}

/// Состояние представления каталога для одного входа в Workspace.
#[derive(Default)]
pub(super) struct Presentation {
    query: String,                  // Нормализованный запрос для сравнения строк.
    expanded: HashSet<Uuid>,        // Раскрытые вручную каналы вне поиска.
    reveals: HashMap<Uuid, Reveal>, // Временные ступени раскрытия отдельных тем.
    search_reveals: HashSet<Uuid>, // Каналы, в которых показаны также несовпавшие темы текущего запроса.
}

impl Presentation {
    /// Возвращает true, когда изменился запрос и прежние поисковые раскрытия сброшены.
    pub(super) fn set_query(&mut self, raw: &str) -> bool {
        let query = raw.trim().to_lowercase();
        if query == self.query {
            return false;
        }
        self.query = query;
        self.search_reveals.clear();
        true
    }

    /// Показывает, действует ли фильтр каталога.
    pub(super) fn searching(&self) -> bool {
        !self.query.is_empty()
    }

    /// Возвращает раскрытые вне поиска каналы для сохранения между показами.
    pub(super) fn expanded(&self) -> &HashSet<Uuid> {
        &self.expanded
    }

    /// Восстанавливает раскрытые каналы из настроек этого входа.
    pub(super) fn set_expanded(&mut self, channels: HashSet<Uuid>) {
        self.expanded = channels;
        self.reveals.clear();
    }

    /// Проверяет обычное раскрытие канала независимо от поисковых результатов.
    pub(super) fn is_expanded(&self, channel: Uuid) -> bool {
        self.expanded.contains(&channel)
    }

    /// Раскрывает канал, когда пользователь открывает его основной разговор.
    pub(super) fn ensure_expanded(&mut self, channel: Uuid) {
        self.expanded.insert(channel);
    }

    /// Открывает достаточную ступень для темы, в которую перешли из другого экрана.
    pub(super) fn ensure_topic_visible(&mut self, chat: &Chat, target: Uuid) -> bool {
        if chat.direct {
            return false;
        }
        let Some((index, topic)) = ordered_topics(chat)
            .enumerate()
            .find(|(_, topic)| topic.id == target)
        else {
            return false;
        };
        self.ensure_expanded(chat.id);
        let visible = self
            .group(chat, None)
            .is_some_and(|group| group.topics.iter().any(|topic| topic.id == target));
        if index >= INITIAL_TOPIC_COUNT && !visible {
            let reveal = if topic.metadata.is_done {
                Reveal::All
            } else {
                Reveal::Unfinished
            };
            self.reveals.insert(chat.id, reveal);
        }
        true
    }

    /// Переключает вложенную часть, сбрасывая ступень показа при сворачивании.
    pub(super) fn toggle_channel(&mut self, channel: Uuid) {
        if self.searching() {
            return;
        }
        if self.expanded.remove(&channel) {
            self.reveals.remove(&channel);
        } else {
            self.expanded.insert(channel);
        }
    }

    /// Забывает состояние каналов, которых больше нет в полном снимке.
    pub(super) fn retain_channels(&mut self, chats: &[Chat]) {
        let channels: HashSet<_> = chats
            .iter()
            .filter(|chat| !chat.direct)
            .map(|chat| chat.id)
            .collect();
        self.expanded.retain(|id| channels.contains(id));
        self.reveals.retain(|id, _| channels.contains(id));
        self.search_reveals.retain(|id| channels.contains(id));
    }

    /// Строит видимые группы в том же порядке, в котором их передал каталог.
    #[cfg(test)]
    pub(super) fn project<'a>(
        &self,
        chats: &'a [Chat],
        selected: Option<HistoryScope>,
    ) -> Vec<ChatGroup<'a>> {
        chats
            .iter()
            .filter_map(|chat| self.group(chat, selected))
            .collect()
    }

    /// Возвращает одну видимую группу для проверки действий по актуальному снимку.
    pub(super) fn group<'a>(
        &self,
        chat: &'a Chat,
        selected: Option<HistoryScope>,
    ) -> Option<ChatGroup<'a>> {
        let expanded = !chat.direct
            && !chat.topics.is_empty()
            && (self.searching() || self.is_expanded(chat.id));
        let mut group = ChatGroup {
            chat,
            expanded,
            selected: false,
            topics: Vec::new(),
            divider_before: None,
            toggle: None,
        };
        if self.searching() {
            self.project_search(&mut group)?;
        } else if expanded {
            self.project_revealed_topics(&mut group);
        }
        group.selected = selected.is_some_and(|scope| {
            scope.stream_id == chat.id
                && scope
                    .topic_id
                    .is_none_or(|id| !group.topics.iter().any(|topic| topic.id == id))
        });
        Some(group)
    }

    /// Применяет действие, только пока оно совпадает с кнопкой в актуальной группе.
    pub(super) fn apply_toggle(
        &mut self,
        chat: &Chat,
        selected: Option<HistoryScope>,
        expected: TopicToggleKind,
    ) -> Option<TopicToggleFocus> {
        let before = self.group(chat, selected)?;
        if before.toggle != Some(expected) {
            return None;
        }
        if self.searching() {
            if expected == TopicToggleKind::HideExtra {
                self.search_reveals.remove(&chat.id);
            } else {
                self.search_reveals.insert(chat.id);
            }
        } else {
            match expected {
                TopicToggleKind::ShowMore(_) => {
                    self.reveals.insert(chat.id, Reveal::Unfinished);
                }
                TopicToggleKind::ShowCompleted(_) => {
                    self.reveals.insert(chat.id, Reveal::All);
                }
                TopicToggleKind::HideExtra => {
                    self.reveals.remove(&chat.id);
                }
            }
        }

        // Обе проекции добавляют темы после уже видимых, поэтому достаточно прежней длины.
        let first_added = self
            .group(chat, selected)?
            .topics
            .get(before.topics.len())
            .copied();
        Some(first_added.map_or(TopicToggleFocus::Button, |topic| {
            TopicToggleFocus::Topic(topic.id)
        }))
    }

    /// Отбирает совпавшие темы и добавляет остальные только по кнопке.
    fn project_search(&self, group: &mut ChatGroup<'_>) -> Option<()> {
        let chat = group.chat;
        let name_matches = self.matches(&chat.name)
            || (chat.direct
                && [
                    chat.metadata.direct_peer_username.as_deref(),
                    chat.metadata.direct_peer_email.as_deref(),
                ]
                .into_iter()
                .flatten()
                .any(|name| self.matches(name)));
        if !group.expanded {
            return name_matches.then_some(());
        }

        let (mut matching, other): (Vec<_>, Vec<_>) =
            ordered_topics(chat).partition(|topic| self.matches(&topic.name));
        if !name_matches && matching.is_empty() {
            return None;
        }

        if !other.is_empty() {
            if self.search_reveals.contains(&chat.id) {
                group.toggle = Some(TopicToggleKind::HideExtra);
                group.divider_before = (!matching.is_empty()).then_some(matching.len());
                matching.extend(other);
            } else {
                group.toggle = Some(TopicToggleKind::ShowMore(other.len()));
            }
        }
        group.topics = matching;
        Some(())
    }

    /// Вычисляет видимый префикс списка и следующую ступень раскрытия.
    fn project_revealed_topics(&self, group: &mut ChatGroup<'_>) {
        let chat = group.chat;
        let total = chat.topics.len();
        let unfinished = chat
            .topics
            .iter()
            .filter(|topic| !topic.metadata.is_done)
            .count();
        let initial = total.min(INITIAL_TOPIC_COUNT);
        let visible = match self.reveals.get(&chat.id).copied().unwrap_or_default() {
            Reveal::Initial => initial,
            Reveal::Unfinished => initial.max(unfinished),
            Reveal::All => total,
        };

        group.toggle = if visible < unfinished {
            Some(TopicToggleKind::ShowMore(unfinished - visible))
        } else if visible < total {
            Some(TopicToggleKind::ShowCompleted(total - visible))
        } else if visible > initial {
            Some(TopicToggleKind::HideExtra)
        } else {
            None
        };
        group.topics = ordered_topics(chat).take(visible).collect();
    }

    /// Сравнивает уже нормализованный запрос с названием или именем.
    fn matches(&self, text: &str) -> bool {
        text.to_lowercase().contains(&self.query)
    }
}

/// Выдаёт незавершённые темы первыми, сохраняя порядок каталога внутри обеих групп.
fn ordered_topics(chat: &Chat) -> impl Iterator<Item = &Topic> {
    chat.topics
        .iter()
        .filter(|topic| !topic.metadata.is_done)
        .chain(chat.topics.iter().filter(|topic| topic.metadata.is_done))
}

#[cfg(test)]
mod tests;
