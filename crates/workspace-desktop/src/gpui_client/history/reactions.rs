//! Связывает быстрые реакции истории с Core. Готовит кнопки и счётчики для текущего окна.

use std::{collections::HashMap, rc::Rc};

use gpui::{App, Context, Div, SharedString, Styled, WeakEntity};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        message_actions::MessageTarget,
        quick_reactions::{QuickReactionUsage, canonical_reaction_name, reaction_glyph},
        reactions::ReactionToggle,
    },
    storage::{HistoryMessage, HistoryWindow, MessageBody, MessageReaction},
};

use super::{Conversation, State, encryption::EncryptedPresentation};
use crate::gpui_client::{
    preferences::Language,
    ui::{Palette, TextRole, label},
};

/// Ожидающее переключение своей реакции на конкретной строке.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingReaction {
    operation: Uuid,        // Связывает ответ рабочего потока с ожидающей кнопкой.
    emoji: String,          // Исходное обозначение эмодзи, с которым запущен запрос.
    own: bool,              // Ожидаемая отметка собственной реакции после запроса.
    status: ReactionStatus, // Ждём сеть или снимок после успешной записи.
}

/// Шаг между кликом, ответом сервера и показом подтверждённой строки.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ReactionStatus {
    Waiting, // Повторное нажатие временно недоступно.
    Applied {
        previous: Option<Uuid>, // Прежнее чтение могло взять старые счётчики.
    },
}

/// Состояние реакций открытого разговора.
#[derive(Default)]
pub(super) struct Reactions {
    usage: QuickReactionUsage, // Снимок использования для владельца рабочего окна.
    usage_request: Option<Uuid>, // Проверка порядка ответов на перечитывание списка.
    pending: HashMap<Uuid, HashMap<String, PendingReaction>>, // Ожидание по сообщению и эмодзи.
    rows: Rc<HashMap<Uuid, ReactionRow>>, // Последние счётчики окна, подготовленные для отрисовки.
    failed: bool,              // Последнюю команду можно повторить.
}

impl Reactions {
    /// Убирает ход кнопок и ошибки, привязанные к закрытому снимку истории.
    pub(super) fn clear_visible(&mut self) {
        self.pending.clear();
        self.rows = Rc::default();
        self.failed = false;
    }
}

/// Локализованная кнопка для одного сообщения.
pub(super) struct QuickReaction {
    pub emoji: SharedString, // Точное обозначение для команды.
    pub glyph: SharedString, // Символ, который виден на кнопке.
    pub label: SharedString, // Подсказка на языке аккаунта.
    pub own: bool,           // У пользователя есть эта реакция с учётом ожидающего запроса.
    pub pending: bool,       // Повторное нажатие временно недоступно.
}

/// Выбор эмодзи, общий для всех сообщений одного окна.
#[derive(Eq, PartialEq)]
struct Choice {
    key: SharedString,   // Символ или имя, которое принимает сервер.
    glyph: SharedString, // Символ из рейтинга Core.
    identity: String,    // Ключ для сопоставления с серверной группой.
    label: SharedString, // Подсказка на языке аккаунта.
}

/// Группы и ожидающие реакции отдельной строки после подготовки к отрисовке.
#[derive(Clone, Eq, PartialEq)]
struct ReactionRow {
    groups: Vec<MessageReaction>, // Счётчики с учётом своих ожидающих действий.
    pending: HashMap<String, PendingReaction>, // Ключи кнопок, ожидающих ответа.
}

impl ReactionRow {
    /// Применяет только ту часть ожидающего действия, которой ещё нет в SQLite.
    fn new(
        groups: &[MessageReaction],
        pending: Option<&HashMap<String, PendingReaction>>,
        current_user: Option<Uuid>,
    ) -> Self {
        let mut groups = groups.to_vec();
        let pending = pending.cloned().unwrap_or_default();
        for (identity, action) in &pending {
            let group = preferred_group(&groups, identity);
            if let Some(index) = group {
                let group = &mut groups[index];
                if group.is_own == action.own {
                    continue;
                }
                group.is_own = action.own;
                group.count = if action.own {
                    group.count.saturating_add(1)
                } else {
                    group.count.saturating_sub(1)
                };
                if let (Some(users), Some(user)) = (&mut group.users, current_user) {
                    users.retain(|candidate| *candidate != user);
                    if action.own {
                        users.push(user);
                    }
                }
            } else if action.own {
                groups.push(MessageReaction {
                    emoji: action.emoji.clone(),
                    count: 1,
                    users: current_user.map(|user| vec![user]),
                    is_own: true,
                });
            }
        }
        groups.retain(|group| group.count > 0);
        groups.sort_by(|left, right| left.emoji.cmp(&right.emoji));
        Self { groups, pending }
    }
}

/// Данные и маршрут действий, общие для кнопок у пузыря и будущей полосы в меню.
#[derive(Clone)]
pub(super) struct ReactionControls {
    choices: Rc<[Choice]>, // Популярные и базовые эмодзи в порядке Core.
    rows: Rc<HashMap<Uuid, ReactionRow>>, // Только подтверждённые доступные сообщения окна.
    conversation: WeakEntity<Conversation>, // Действие пропадает вместе с экраном.
}

impl ReactionControls {
    /// Возвращает быстрые реакции, учитывая общие настройки и собственные отметки строки.
    pub(super) fn quick(&self, id: Uuid) -> Vec<QuickReaction> {
        let row = self.rows.get(&id);
        self.choices
            .iter()
            .map(|choice| {
                let group = row.and_then(|row| {
                    preferred_group(&row.groups, &choice.identity).map(|index| &row.groups[index])
                });
                QuickReaction {
                    emoji: group
                        .map_or_else(|| choice.key.clone(), |group| group.emoji.clone().into()),
                    glyph: choice.glyph.clone(),
                    label: choice.label.clone(),
                    own: group.is_some_and(|group| group.is_own),
                    pending: row
                        .and_then(|row| row.pending.get(&choice.identity))
                        .is_some_and(|pending| pending.status == ReactionStatus::Waiting),
                }
            })
            .collect()
    }

    /// Возвращает текущие счётчики, включая изменения до подтверждения сервера.
    pub(super) fn groups(&self, id: Uuid) -> &[MessageReaction] {
        self.rows.get(&id).map_or(&[], |row| row.groups.as_slice())
    }

    /// Проверяет, ждёт ли нажатая реакция подтверждения.
    pub(super) fn is_pending(&self, id: Uuid, emoji: &str) -> bool {
        self.rows
            .get(&id)
            .and_then(|row| row.pending.get(canonical_reaction_name(emoji).as_ref()))
            .is_some_and(|pending| pending.status == ReactionStatus::Waiting)
    }

    /// Сравнивает входы кэшируемой строки без проверки остальных сообщений.
    pub(super) fn same_row(&self, other: &Self, id: Uuid) -> bool {
        self.rows.get(&id) == other.rows.get(&id) && self.choices == other.choices
    }

    /// Передаёт намерение экрану, который проверит адрес и доступ повторно.
    pub(super) fn toggle(&self, id: Uuid, emoji: &str, app: &mut App) {
        let _ = self
            .conversation
            .update(app, |view, cx| view.toggle_reaction(id, emoji, cx));
    }
}

/// Выбирает группу эмодзи по каноническому имени, отдавая приоритет собственной реакции.
fn preferred_group(groups: &[MessageReaction], identity: &str) -> Option<usize> {
    groups
        .iter()
        .enumerate()
        .filter(|(_, group)| canonical_reaction_name(&group.emoji) == identity)
        .max_by_key(|(_, group)| (group.is_own, group.emoji == identity))
        .map(|(index, _)| index)
}

/// Подбирает подпись базового эмодзи или подставляет популярный символ в общий перевод.
fn choice_label(locale: Language, glyph: &str) -> SharedString {
    let key = match glyph {
        "❤️" => "a11y.like",
        "👍" => "a11y.thumbsUp",
        "😂" => "a11y.joy",
        "😮" => "a11y.surprised",
        "😢" => "a11y.crying",
        "👏" => "a11y.clap",
        _ => {
            return locale
                .tr_with("a11y.quickReaction", &[("emoji", glyph)])
                .into();
        }
    };
    locale.tr(key).into()
}

impl Conversation {
    /// Загружает один рейтинг для владельца, отбрасывая ответы предыдущих чтений.
    pub(super) fn load_reaction_usage(&mut self, cx: &mut Context<Self>) {
        let Some(runtime) = self.avatar_runtime.clone() else {
            return;
        };
        let operation = Uuid::new_v4();
        self.reactions.usage_request = Some(operation);
        let address = self.address.clone();
        cx.spawn(async move |view, cx| {
            let reply = runtime
                .conversation(address, |services, address| async move {
                    services
                        .sync
                        .quick_reaction_usage(&address.route.owner, address.generation)
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.address != reply.address || view.reactions.usage_request != Some(operation)
                {
                    return;
                }
                view.reactions.usage_request = None;
                if let Ok(Ok(usage)) = reply.value {
                    view.reactions.usage = usage;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Готовит общий список быстрых реакций и состояние подтверждённых строк.
    pub(super) fn reaction_controls(
        &self,
        locale: Language,
        cx: &Context<Self>,
    ) -> Option<ReactionControls> {
        let State::Ready(_) = &self.state else {
            return None;
        };
        self.avatar_runtime.as_ref()?;
        Some(ReactionControls {
            choices: self
                .reactions
                .usage
                .ranked()
                .into_iter()
                .map(|key| {
                    let glyph = reaction_glyph(&key);
                    Choice {
                        identity: canonical_reaction_name(&key).into_owned(),
                        label: choice_label(locale, &glyph),
                        glyph: glyph.into_owned().into(),
                        key: key.into(),
                    }
                })
                .collect::<Rc<[_]>>(),
            rows: self.reactions.rows.clone(),
            conversation: cx.entity().downgrade(),
        })
    }

    /// Разрешает реакцию у серверного сообщения, тело которого сейчас открыто.
    fn can_react(&self, message: &HistoryMessage) -> bool {
        !message.id.is_nil()
            && match &message.body {
                MessageBody::Markdown(_) => true,
                MessageBody::Encrypted { .. } => self.encryption.as_ref()
                    .and_then(|encryption| encryption.presentation(message.id))
                    .is_some_and(|presentation| {
                        matches!(presentation, EncryptedPresentation::Ready(access) if access.with_current(|| ()).is_some())
                    }),
                MessageBody::KeyService { .. } => false,
            }
    }

    /// Меняет свою отметку для текущей строки, пока сервер подтверждает запрос.
    fn toggle_reaction(&mut self, id: Uuid, emoji: &str, cx: &mut Context<Self>) {
        let (State::Ready(history), Some(runtime)) = (&self.state, self.avatar_runtime.clone())
        else {
            return;
        };
        let Some(message) = history
            .messages
            .iter()
            .find(|row| row.id == id && self.can_react(row))
        else {
            return;
        };
        let identity = canonical_reaction_name(emoji).into_owned();
        let pending = self.reactions.pending.entry(id).or_default();
        let previous = pending.get(&identity);
        if previous.is_some_and(|pending| pending.status == ReactionStatus::Waiting) {
            return;
        }
        let own = previous.map_or_else(
            || {
                preferred_group(&message.reactions, &identity)
                    .is_some_and(|index| message.reactions[index].is_own)
            },
            |pending| pending.own,
        );
        let operation = Uuid::new_v4();
        let emoji = emoji.to_owned();
        pending.insert(
            identity.clone(),
            PendingReaction {
                operation,
                emoji: emoji.clone(),
                own: !own,
                status: ReactionStatus::Waiting,
            },
        );
        self.reactions.failed = false;
        let target = MessageTarget {
            scope: self.address.route.scope,
            topic_id: message.topic_id,
            message_id: message.id,
        };
        self.refresh_reaction_row(id);
        self.viewport.invalidate_row_height(id);
        cx.notify();
        let address = self.address.clone();
        log::debug!("gpui_history op_id={operation} event=reaction_started source_id={id}");
        cx.spawn(async move |view, cx| {
            let reply = runtime
                .conversation(address, move |services, address| async move {
                    services
                        .messenger
                        .toggle_message_reaction(
                            &services.auth,
                            &services.sync,
                            &address.route.owner,
                            address.generation,
                            target,
                            &emoji,
                        )
                        .await
                })
                .await;
            let result = reply.value.ok().and_then(Result::ok);
            log::debug!(
                "gpui_history op_id={operation} event=reaction_completed source_id={id} success={}",
                result.is_some()
            );
            let _ = view.update(cx, |view, cx| {
                if view.address != reply.address {
                    return;
                }
                let Some(pending) = view.reactions.pending.get_mut(&id) else {
                    return;
                };
                let Some(entry) = pending
                    .get_mut(&identity)
                    .filter(|entry| entry.operation == operation)
                else {
                    return;
                };
                if let Some(result) = result {
                    entry.own = result == ReactionToggle::Added;
                    entry.status = ReactionStatus::Applied {
                        previous: view.active.map(|flight| flight.id),
                    };
                    view.queue_read(cx);
                } else {
                    pending.remove(&identity);
                    if pending.is_empty() {
                        view.reactions.pending.remove(&id);
                    }
                    view.reactions.failed = view.has_access();
                }
                view.refresh_reaction_row(id);
                view.viewport.invalidate_row_height(id);
                cx.notify();
            });
        })
        .detach();
    }

    /// Перечитывает счётчики одной строки после запуска или окончания команды.
    fn refresh_reaction_row(&mut self, id: Uuid) {
        let State::Ready(history) = &self.state else {
            return;
        };
        let pending = self.reactions.pending.get(&id);
        let user = self.address.route.owner.user_id.parse().ok();
        let projected = history
            .messages
            .iter()
            .find(|row| row.id == id)
            .filter(|row| pending.is_some() || !row.reactions.is_empty())
            .map(|row| ReactionRow::new(&row.reactions, pending, user));
        let rows = Rc::make_mut(&mut self.reactions.rows);
        if let Some(row) = projected {
            rows.insert(id, row);
        } else {
            rows.remove(&id);
        }
    }

    /// Готовит счётчики принятой истории и переизмеряет только изменившиеся строки.
    pub(super) fn accept_reaction_rows(&mut self, history: &HistoryWindow, flight: Uuid) {
        self.reactions.pending.retain(|_, pending| {
            pending.retain(|_, action| {
                action.status == ReactionStatus::Waiting
                    || action.status
                        == (ReactionStatus::Applied {
                            previous: Some(flight),
                        })
            });
            !pending.is_empty()
        });
        let user = self.address.route.owner.user_id.parse().ok();
        let projected: HashMap<_, _> = history
            .messages
            .iter()
            .filter_map(|row| {
                let pending = self.reactions.pending.get(&row.id);
                if pending.is_none() && row.reactions.is_empty() {
                    return None;
                }
                Some((row.id, ReactionRow::new(&row.reactions, pending, user)))
            })
            .collect();
        for (id, row) in &projected {
            if self.reactions.rows.get(id) != Some(row) {
                self.viewport.invalidate_row_height(*id);
            }
        }
        for id in self
            .reactions
            .rows
            .keys()
            .filter(|id| !projected.contains_key(id))
        {
            self.viewport.invalidate_row_height(*id);
        }
        self.reactions.rows = Rc::new(projected);
    }

    /// Показывает ошибку команды в открытом разговоре.
    pub(super) fn reaction_error(&self, palette: Palette, locale: Language) -> Option<Div> {
        self.reactions.failed.then(|| {
            label(
                palette,
                TextRole::Caption,
                locale.tr("message.reactionError"),
            )
            .px_4()
            .py_1()
        })
    }

    /// Проверяет собственную отметку в принятом снимке после ответа сервера.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_reaction_confirmed(
        &self,
        id: Uuid,
        emoji: &str,
        selected: bool,
    ) -> bool {
        let State::Ready(history) = &self.state else {
            return false;
        };
        self.reactions
            .pending
            .get(&id)
            .is_none_or(|pending| !pending.contains_key(emoji))
            && history
                .messages
                .iter()
                .find(|row| row.id == id)
                .is_some_and(|row| {
                    row.reactions
                        .iter()
                        .any(|group| group.emoji == emoji && group.is_own)
                        == selected
                })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Строка сразу показывает отметку и не удваивает её, когда раньше ответа пришло событие.
    #[test]
    fn confirmed_add_does_not_double_optimistic_count() {
        let user = Uuid::from_u128(1);
        let pending = HashMap::from([(
            canonical_reaction_name("👍").into_owned(),
            PendingReaction {
                operation: Uuid::from_u128(2),
                emoji: "👍".into(),
                own: true,
                status: ReactionStatus::Waiting,
            },
        )]);
        let prediction = ReactionRow::new(&[], Some(&pending), Some(user));
        assert!(prediction.groups[0].is_own);
        assert_eq!(prediction.groups[0].count, 1);

        let confirmed = MessageReaction {
            emoji: "thumbsup".into(),
            count: 1,
            users: Some(vec![user]),
            is_own: true,
        };
        let after_event =
            ReactionRow::new(std::slice::from_ref(&confirmed), Some(&pending), Some(user));
        assert_eq!(after_event.groups, [confirmed]);
    }

    /// При снятии своей реакции чужая отметка и счётчик остаются доступны.
    #[test]
    fn optimistic_remove_keeps_other_reactors() {
        let user = Uuid::from_u128(1);
        let peer = Uuid::from_u128(2);
        let pending = HashMap::from([(
            canonical_reaction_name("❤️").into_owned(),
            PendingReaction {
                operation: Uuid::from_u128(3),
                emoji: "❤️".into(),
                own: false,
                status: ReactionStatus::Waiting,
            },
        )]);
        let group = MessageReaction {
            emoji: "heart".into(),
            count: 2,
            users: Some(vec![user, peer]),
            is_own: true,
        };
        let projected = ReactionRow::new(&[group], Some(&pending), Some(user));
        assert_eq!(projected.groups[0].count, 1);
        assert_eq!(projected.groups[0].users, Some(vec![peer]));
        assert!(!projected.groups[0].is_own);
    }
}
