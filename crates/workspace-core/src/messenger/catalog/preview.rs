//! Загружает последние сообщения для каталога и связывает их с доступными строками.

use std::collections::{HashMap, HashSet};

use futures::{StreamExt, stream};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    auth::{Auth, Service, SessionSnapshot},
    context::OwnerContext,
    storage::HistoryTime,
};

use super::{Catalog, CatalogError, Chat};

const BATCH_SIZE: usize = 100;
const CONCURRENT_BATCHES: usize = 3;
const TEXT_LIMIT: usize = 240;

/// Последнее подтверждённое сообщение, которое можно показать в строке списка.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SidebarPreview {
    pub message_id: Uuid,            // UUID последнего сообщения из каталога.
    pub topic_id: Uuid,              // Тема, в которой лежит сообщение.
    pub author_id: Uuid,             // UUID автора сообщения.
    pub author_name: Option<String>, // Имя автора, если оно уже есть в снимке каталога.
    pub created_at: HistoryTime,     // Время создания сообщения в UTC.
    pub text: String,                // Короткий текст, который показываем без разметки.
}

/// Превью одного входа с UUID канала для повторной проверки при публикации.
pub(crate) type SidebarPreviews = HashMap<Uuid, (Uuid, SidebarPreview)>;

/// Поля ответа, по которым проверяем владельца и адрес последнего сообщения.
#[derive(Deserialize)]
struct WirePreview {
    uuid: Uuid,         // Серверный UUID сообщения.
    project_id: String, // Проект, которому принадлежит сообщение.
    user_uuid: String,  // Пользователь, для которого построен ответ.
    stream_uuid: Uuid,  // Канал или личный чат сообщения.
    topic_uuid: Uuid,   // Тема сообщения.
    author_uuid: Uuid,  // Кто написал сообщение.
    created_at: String, // Серверное время отправки.
    payload: WireBody,  // Вид и данные сообщения.
}

/// Тело последнего сообщения в ответе сервера.
#[derive(Deserialize)]
struct WireBody {
    kind: String,           // Поддерживается текстовый Markdown.
    content: Option<Value>, // Содержимое для конкретного вида сообщения.
}

impl Catalog {
    /// Прочитает актуальные сообщения ограниченными пакетами, оставляя каталог доступным.
    pub(crate) async fn load_previews(
        &self,
        auth: &Auth,
        session: &SessionSnapshot,
        chats: Vec<Chat>,
    ) -> Result<SidebarPreviews, CatalogError> {
        let mut ids = HashSet::new();
        let mut names = HashMap::new();
        for chat in chats {
            ids.extend(chat.metadata.last_message_id);
            ids.extend(
                chat.topics
                    .iter()
                    .filter_map(|topic| topic.metadata.last_message_id),
            );
            if let Some(peer) = chat.metadata.direct_peer_id {
                names.insert(peer, chat.name);
            }
        }
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut ids: Vec<_> = ids.into_iter().collect();
        ids.sort_unstable();
        let owner = &session.context;
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, session.generation)
            .await?;
        let requests: Vec<_> = ids.chunks(BATCH_SIZE).map(<[_]>::to_vec).collect();
        let mut batches = stream::iter(requests)
            .map(|ids| {
                let rest = self.rest.clone();
                let owner = owner.clone();
                let token = token.clone();
                async move {
                    let rows = rest.sidebar_messages(&owner, &token, &ids).await;
                    (ids, rows)
                }
            })
            .buffer_unordered(CONCURRENT_BATCHES);
        let mut previews = HashMap::new();
        while let Some((ids, rows)) = batches.next().await {
            let Ok(rows) = rows else {
                continue;
            };
            previews.extend(rows.into_iter().filter_map(|row| {
                let row: WirePreview = serde_json::from_value(row).ok()?;
                if ids.binary_search(&row.uuid).is_err()
                    || row.project_id != owner.project_id
                    || row.user_uuid != owner.user_id
                {
                    return None;
                }
                let stream = row.stream_uuid;
                let preview = row.into_preview(&names)?;
                Some((preview.message_id, (stream, preview)))
            }));
        }
        // При публикации сверяем UUID и маршрут каждой строки с текущим каталогом.
        Ok(auth.with_active_generation(owner, session.generation, || previews)?)
    }
}

impl WirePreview {
    /// Преобразует только открытый текст, оставляя зашифрованные и прочие виды без превью.
    fn into_preview(self, names: &HashMap<Uuid, String>) -> Option<SidebarPreview> {
        if self.payload.kind != "markdown" {
            return None;
        }
        let Value::String(text) = self.payload.content? else {
            return None;
        };
        let text = preview_text(&text);
        if text.is_empty() {
            return None;
        }
        Some(SidebarPreview {
            message_id: self.uuid,
            topic_id: self.topic_uuid,
            author_id: self.author_uuid,
            author_name: names.get(&self.author_uuid).cloned(),
            created_at: HistoryTime::parse(&self.created_at)?,
            text,
        })
    }
}

/// Собирает одну короткую строку и заменяет адреса подписью ссылки.
fn preview_text(markdown: &str) -> String {
    let links = inline_link_labels(markdown);
    let text: String = links
        .split_whitespace()
        .map(|word| {
            let word = word.trim_matches(['*', '`', '~']);
            let address = word.trim_matches(['<', '>']);
            if address.starts_with("https://") || address.starts_with("http://") {
                "Ссылка"
            } else {
                word
            }
        })
        .filter(|word| !word.is_empty())
        .flat_map(|word| word.chars().chain(std::iter::once(' ')))
        .filter(|character| !character.is_control())
        .take(TEXT_LIMIT)
        .collect();
    text.trim_end().to_owned()
}

/// Оставляет подписи встроенных ссылок и вложений без их адресов.
fn inline_link_labels(mut text: &str) -> String {
    let mut labels = String::new();
    while let Some((before, destination)) = text.split_once("](") {
        let Some((prefix, label)) = before.rsplit_once('[') else {
            labels.push_str(before);
            labels.push_str("](");
            text = destination;
            continue;
        };
        let mut depth = 1;
        let Some((close, _)) = destination.char_indices().find(|&(_, character)| {
            match character {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            depth == 0
        }) else {
            break;
        };
        labels.push_str(prefix.strip_suffix('!').unwrap_or(prefix));
        labels.push_str(if label.is_empty() {
            "Ссылка"
        } else {
            label
        });
        text = &destination[close + 1..];
    }
    labels.push_str(text);
    labels
}

/// Привязывает ответы к текущему каталогу, сохраняя изменения или отзывы доступа из SQLite.
pub(crate) fn attach(owner: &OwnerContext, chats: &mut [Chat], previews: &SidebarPreviews) {
    let own_id = owner.user_id.parse::<Uuid>().ok();
    for chat in chats {
        let get = |last_message_id: Option<Uuid>| {
            let (stream_id, preview) = previews.get(&last_message_id?)?;
            (*stream_id == chat.id).then_some(preview)
        };
        let chat_preview = get(chat.metadata.last_message_id).filter(|preview| {
            chat.direct || chat.topics.iter().any(|topic| topic.id == preview.topic_id)
        });
        chat.metadata.preview = named_preview(chat_preview, own_id);
        for topic in &mut chat.topics {
            let topic_preview =
                get(topic.metadata.last_message_id).filter(|preview| preview.topic_id == topic.id);
            topic.metadata.preview = named_preview(topic_preview, own_id);
        }
    }
}

/// Подписывает своё сообщение независимо от наличия автора в справочнике.
fn named_preview(preview: Option<&SidebarPreview>, own_id: Option<Uuid>) -> Option<SidebarPreview> {
    let mut preview = preview?.clone();
    if Some(preview.author_id) == own_id {
        preview.author_name = Some("Вы".to_owned());
    }
    Some(preview)
}
