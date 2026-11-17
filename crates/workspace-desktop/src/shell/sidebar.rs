//! Показывает каталог активного входа, ищет чаты и темы и отправляет действия оболочке.

use std::collections::HashSet;

use iced::advanced::widget::Id;
use iced::widget::{column, container, keyed, row, scrollable, text};
use iced::{Alignment, Element, Fill};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;
use workspace_core::auth::SessionSnapshot;
use workspace_core::bootstrap::{ChatList, ChatListStatus};
use workspace_core::context::SessionGeneration;
use workspace_core::messenger::catalog::{Chat, Topic};
use workspace_core::storage::HistoryScope;

use super::{ChatEntry, ChatRoute, Message};
use crate::ui_kit::{
    AvatarContent, AvatarSize, ButtonSize, ButtonVariant, ChatRowOptions, FieldSize, IconName,
    IconTone, avatar, button_with, chat_row, icon, search_input, selectable_row,
};

mod scope;
#[cfg(test)]
mod tests;

/// Действие над каталогом, который видел пользователь в своём входе.
#[derive(Clone, Debug)]
pub enum SidebarAction {
    Search(String),      // Текст, введённый в поле поиска.
    ClearSearch,         // Очистить запрос и вернуть фокус полю.
    ToggleChannel(Uuid), // Раскрыть или свернуть темы указанного канала.
    Retry,               // Снова загрузить список после ошибки.
}

/// Состояние каталога для одного входа, которое сохраняется при открытии разговора.
#[derive(Default)]
pub(super) struct Sidebar {
    pub(super) query: String,           // Текущий текст в поле поиска.
    pub(super) expanded: HashSet<Uuid>, // Каналы, вручную раскрытые пользователем.
}

/// Различает строки чатов и темы при обновлении списка виджетов.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CatalogKey {
    Chat(Uuid),  // UUID личного чата или канала.
    Topic(Uuid), // UUID строки темы.
}

/// Переиспользует поиск UI-кита и прокручивает только строки каталога.
pub(super) fn view<'a>(
    session: &SessionSnapshot,
    list: Option<&'a ChatList>,
    state: Option<&Sidebar>,
    selected: Option<HistoryScope>,
) -> Element<'a, Message> {
    let owner = session.context.clone();
    let generation = session.generation;
    let action = move |action| Message::Sidebar {
        owner: owner.clone(),
        generation,
        action,
    };
    let query = state.map_or("", |state| state.query.as_str());
    let mut panel = column![
        text("Чаты и каналы").size(18),
        search_input(
            search_id(generation),
            "Поиск по чатам",
            query,
            FieldSize::Compact,
            Some({
                let action = action.clone();
                move |query| action(SidebarAction::Search(query))
            }),
            action(SidebarAction::ClearSearch),
        ),
    ]
    .spacing(8);

    match list.map(|list| list.status) {
        Some(ChatListStatus::Failed) => {
            panel = panel.push(
                column![
                    text("Не удалось загрузить чаты").size(13),
                    button_with(
                        "Повторить",
                        crate::ui_kit::ButtonOptions {
                            variant: ButtonVariant::Neutral,
                            size: ButtonSize::Small,
                            ..Default::default()
                        },
                        Some(action(SidebarAction::Retry)),
                    ),
                ]
                .spacing(6),
            );
        }
        Some(ChatListStatus::Ready) => {}
        _ => panel = panel.push(text("Загрузка чатов…").size(13)),
    }

    let mut rows = keyed::Column::new().width(Fill).spacing(4);
    if let Some(chats) = list.and_then(|list| list.chats.as_deref()) {
        let query = query.trim().to_lowercase();
        let searching = !query.is_empty();
        let mut found = false;
        for chat in chats {
            let name_matches = matches_query(&chat.name, &query);
            let visible_topics = chat
                .topics
                .iter()
                .filter(|topic| name_matches || matches_query(&topic.name, &query));
            if !name_matches && visible_topics.clone().next().is_none() {
                continue;
            }
            let expanded =
                searching || state.is_some_and(|state| state.expanded.contains(&chat.id));
            let scope = HistoryScope {
                stream_id: chat.id,
                topic_id: None,
            };
            let on_press = if chat.direct {
                Some(open(session, scope))
            } else if !searching && !chat.topics.is_empty() {
                Some(action(SidebarAction::ToggleChannel(chat.id)))
            } else {
                None
            };
            let selected_chat = selected.is_some_and(|selected| {
                selected.stream_id == chat.id && (selected.topic_id.is_none() || !expanded)
            });
            found = true;
            let key = CatalogKey::Chat(chat.id);
            rows = rows.push(
                key,
                scope::bind(key, chat_widget(chat, expanded, selected_chat, on_press)),
            );
            if !chat.direct && expanded {
                for topic in visible_topics {
                    let scope = HistoryScope {
                        stream_id: chat.id,
                        topic_id: Some(topic.id),
                    };
                    let key = CatalogKey::Topic(topic.id);
                    rows = rows.push(
                        key,
                        scope::bind(
                            key,
                            topic_widget(topic, selected == Some(scope), open(session, scope)),
                        ),
                    );
                }
            }
        }
        if !found && list.is_some_and(|list| list.status == ChatListStatus::Ready) {
            panel = panel.push(
                text(if chats.is_empty() {
                    "У вас пока нет чатов"
                } else {
                    "Ничего не найдено"
                })
                .size(13),
            );
        }
    }

    scope::bind(
        (session.context.clone(), generation),
        container(panel.push(scrollable(rows).height(Fill).id(scroll_id(generation))))
            .width(Fill)
            .height(Fill)
            .padding(12)
            .into(),
    )
}

/// Проверяет, есть ли в текущем каталоге канал с темами для раскрытия.
pub(super) fn can_toggle(list: Option<&ChatList>, stream: Uuid) -> bool {
    list.and_then(|list| list.chats.as_deref())
        .is_some_and(|chats| {
            chats
                .iter()
                .any(|chat| chat.id == stream && !chat.direct && !chat.topics.is_empty())
        })
}

/// Находит отображаемую подпись разговора и проверяет связь темы с каналом.
pub(super) fn catalog_title(list: Option<&ChatList>, scope: HistoryScope) -> Option<&str> {
    let chat = list?
        .chats
        .as_deref()?
        .iter()
        .find(|chat| chat.id == scope.stream_id)?;
    match scope.topic_id {
        None if chat.direct => Some(&chat.name),
        Some(id) if !chat.direct => chat
            .topics
            .iter()
            .find(|topic| topic.id == id)
            .map(|topic| topic.name.as_str()),
        _ => None,
    }
}

/// Возвращает идентификатор поля для переброски фокуса после очистки запроса.
pub(super) fn search_id(generation: SessionGeneration) -> Id {
    Id::from(format!("sidebar-search-{}", generation.0))
}

/// Возвращает идентификатор прокрутки текущего каталога.
pub(super) fn scroll_id(generation: SessionGeneration) -> Id {
    Id::from(format!("sidebar-scroll-{}", generation.0))
}

/// Строит Latest для лички или темы, с привязкой к источнику каталога.
fn open(session: &SessionSnapshot, scope: HistoryScope) -> Message {
    Message::OpenChat {
        route: ChatRoute {
            owner: session.context.clone(),
            scope,
            entry: ChatEntry::Latest,
        },
        generation: session.generation,
    }
}

/// Сравнивает имя с запросом без учёта регистра.
fn matches_query(name: &str, query: &str) -> bool {
    query.is_empty() || name.to_lowercase().contains(query)
}

/// Собирает строку каталога. Для канала стрелка показывает состояние списка тем.
fn chat_widget(
    chat: &Chat,
    expanded: bool,
    selected: bool,
    on_press: Option<Message>,
) -> Element<'_, Message> {
    let initial = chat
        .name
        .trim_start()
        .graphemes(true)
        .next()
        .map_or(AvatarContent::Placeholder, AvatarContent::Initials);
    let avatar = avatar(AvatarSize::Medium, initial, None);
    let avatar = if chat.direct || chat.topics.is_empty() {
        avatar
    } else {
        row![
            icon(
                if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
                14,
                IconTone::Muted,
                None
            ),
            avatar,
        ]
        .align_y(Alignment::Center)
        .spacing(4)
        .into()
    };
    chat_row(ChatRowOptions {
        avatar,
        title: &chat.name,
        preview: None,
        time: None,
        unread: None,
        mention: false,
        selected,
        on_press,
    })
}

/// Рисует компактную выбираемую строку под родительским каналом.
fn topic_widget(topic: &Topic, selected: bool, on_press: Message) -> Element<'_, Message> {
    let label = row![text("#"), text(&topic.name).wrapping(text::Wrapping::None)]
        .spacing(6)
        .align_y(Alignment::Center);
    container(selectable_row(label, selected, Some(on_press)))
        .padding(iced::Padding::default().left(24))
        .width(Fill)
        .into()
}
