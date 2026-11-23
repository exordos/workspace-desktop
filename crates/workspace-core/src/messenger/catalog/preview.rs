//! Загружает последние сообщения для каталога и связывает их с доступными строками.

use std::collections::{HashMap, HashSet};

use futures::{StreamExt, stream};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    auth::{Auth, Service, SessionSnapshot},
    context::OwnerContext,
    messenger::{
        encryption::{
            DecryptionError, EncryptionActivation, MessageEncryption, MessageReadPermission,
        },
        rest::WireHistoryMessage,
    },
    storage::{HistoryMessage, HistoryTime, KeyServicePayload, MessageBody},
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
    pub text: SidebarPreviewBody,    // Обычная подпись или исходный зашифрованный конверт.
}

/// Содержимое фонового превью. Расшифрованный текст в этой модели не хранится.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SidebarPreviewBody {
    Summary(String), // Короткая строка из обычного Markdown.
    Encrypted {
        key_id: Uuid,        // Точное поколение, указанное в сообщении.
        ciphertext: String,  // Канонический конверт из REST.
        owner: OwnerContext, // Вход, для которого сервер вернул сообщение.
        stream_id: Uuid,     // Подтверждённый стрим с конвертом.
    },
    KeyService(SidebarKeyServiceKind), // Только вид служебного события для подписи.
}

/// Служебное событие для стандартной подписи, без данных запроса или выдачи.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SidebarKeyServiceKind {
    Requested, // Участник запросил ключ.
    Granted,   // Участник передал ключ.
    Rejected,  // Участник отклонил запрос.
    Announced, // Владелец объявил новое поколение.
}

impl From<&str> for SidebarPreviewBody {
    /// Создаёт подпись обычного сообщения из готовой строки.
    fn from(text: &str) -> Self {
        Self::Summary(text.to_owned())
    }
}

impl From<String> for SidebarPreviewBody {
    /// Принимает подготовленную подпись обычного сообщения.
    fn from(text: String) -> Self {
        Self::Summary(text)
    }
}

impl SidebarPreviewBody {
    /// Возвращает подпись до расшифровки или после удаления ключа.
    pub fn fallback_text(&self) -> &str {
        match self {
            Self::Summary(text) => text,
            Self::Encrypted { .. } => "Нужен ключ",
            Self::KeyService(kind) => match kind {
                SidebarKeyServiceKind::Requested => "Запрос ключа",
                SidebarKeyServiceKind::Granted => "Ключ передан",
                SidebarKeyServiceKind::Rejected => "Запрос отклонён",
                SidebarKeyServiceKind::Announced => "Объявлен новый ключ",
            },
        }
    }
}

impl SidebarPreview {
    /// Возвращает каноническое содержимое превью для выбранной строки.
    pub fn body(&self) -> &SidebarPreviewBody {
        &self.text
    }

    /// Подготавливает безопасную короткую подпись под точным ключом стрима.
    /// Вызывать в фоновом потоке с уже проверенной строкой каталога.
    pub fn prepare_summary(
        &self,
        stream_id: Uuid,
        encryption: &MessageEncryption,
        activation: &EncryptionActivation,
    ) -> Result<Option<ProtectedSidebarSummary>, DecryptionError> {
        let SidebarPreviewBody::Encrypted {
            key_id,
            ciphertext,
            owner,
            stream_id: original_stream,
        } = &self.text
        else {
            return Ok(None);
        };
        if &activation.session().owner != owner || stream_id != *original_stream {
            return Err(DecryptionError::Stale);
        }
        let prepared = encryption.prepare(activation, stream_id, *key_id)?;
        let accepted = encryption.accept(prepared.decrypt(ciphertext))?;
        let text = accepted
            .with_body(|markdown| Zeroizing::new(preview_text(markdown)))
            .ok_or(DecryptionError::Stale)?;
        Ok(Some(ProtectedSidebarSummary {
            permission: accepted.into_permission(),
            text,
        }))
    }
}

/// Временная подпись, доступная, пока действует ключ сообщения.
pub struct ProtectedSidebarSummary {
    permission: MessageReadPermission, // Не даёт показать старую подпись при отзыве ключа.
    text: Zeroizing<String>,           // Сокращённая строка для представления.
}

impl ProtectedSidebarSummary {
    /// Читает одну короткую строку и возвращает `None` после отзыва ключа.
    pub fn with_text<T>(&self, read: impl FnOnce(&str) -> T) -> Option<T> {
        self.permission.with_current(|| read(&self.text))
    }
}

/// Превью одного входа с UUID канала для повторной проверки при публикации.
pub(crate) type SidebarPreviews = HashMap<Uuid, (Uuid, SidebarPreview)>;

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
                let row: WireHistoryMessage = serde_json::from_value(row).ok()?;
                let row = row.into_presentation(owner).ok()??;
                if ids.binary_search(&row.id).is_err() {
                    return None;
                }
                if matches!(row.body, MessageBody::Encrypted { .. })
                    && !self.storage.quote_source_matches(owner, &row).ok()?
                {
                    return None;
                }
                let stream = row.stream_id;
                let preview = make_preview(row, owner, &names)?;
                Some((preview.message_id, (stream, preview)))
            }));
        }
        // При публикации сверяем UUID и маршрут каждой строки с текущим каталогом.
        Ok(auth.with_active_generation(owner, session.generation, || previews)?)
    }
}

/// Собирает подпись обычного сообщения или переносит конверт без расшифровки.
fn make_preview(
    row: HistoryMessage,
    owner: &OwnerContext,
    names: &HashMap<Uuid, String>,
) -> Option<SidebarPreview> {
    let author_id = row.metadata.author_id?;
    let text = match row.body {
        MessageBody::Markdown(text) => {
            let summary = preview_text(&text);
            if summary.is_empty() {
                return None;
            }
            SidebarPreviewBody::Summary(summary)
        }
        MessageBody::Encrypted { key_id, ciphertext } => SidebarPreviewBody::Encrypted {
            key_id,
            ciphertext,
            owner: owner.clone(),
            stream_id: row.stream_id,
        },
        MessageBody::KeyService { payload, .. } => {
            let kind = match payload {
                KeyServicePayload::Request { .. } => SidebarKeyServiceKind::Requested,
                KeyServicePayload::Grant { .. } => SidebarKeyServiceKind::Granted,
                KeyServicePayload::Reject(_) => SidebarKeyServiceKind::Rejected,
                KeyServicePayload::Announced { .. } => SidebarKeyServiceKind::Announced,
            };
            SidebarPreviewBody::KeyService(kind)
        }
    };
    Some(SidebarPreview {
        message_id: row.id,
        topic_id: row.topic_id,
        author_id,
        author_name: names.get(&author_id).cloned(),
        created_at: row.metadata.created_at,
        text,
    })
}

/// Собирает одну короткую строку и заменяет адреса подписью ссылки.
pub(in crate::messenger) fn preview_text(markdown: &str) -> String {
    let markdown = crate::messenger::encryption::fragments::fragment_preview(markdown);
    let markdown = crate::messenger::forward::forward_preview(&markdown);
    let links = inline_link_labels(&markdown);
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use crate::{
        context::{AccountId, SessionGeneration},
        messenger::encryption::{
            ChatKeyAddress, ChatKeyStore, EncryptionSession, GroupKey, KeyStoreError,
        },
    };

    use super::*;

    /// Возвращает стабильный ключ до закрытия выбранного профиля.
    struct Key;

    impl ChatKeyStore for Key {
        /// Читает заданные тестовые байты.
        fn load(&self, _: &ChatKeyAddress) -> Result<Option<GroupKey>, KeyStoreError> {
            Ok(Some(GroupKey::from_bytes([27; 32])))
        }

        /// Запись тесту не нужна.
        fn save(&self, _: &ChatKeyAddress, _: &GroupKey) -> Result<(), KeyStoreError> {
            unreachable!()
        }

        /// Тест отбирает разрешение через выбор профиля.
        fn remove(&self, _: &ChatKeyAddress) -> Result<(), KeyStoreError> {
            unreachable!()
        }
    }

    /// Превью верного владельца сохраняет только исходный конверт и маршрут.
    #[test]
    fn encrypted_preview_is_canonical_and_summary_is_revocable() {
        let owner = OwnerContext {
            account_id: AccountId("account".into()),
            server: "https://workspace.test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        };
        let stream = Uuid::from_u128(10);
        let topic = Uuid::from_u128(11);
        let key = Uuid::from_u128(12);
        let message = Uuid::from_u128(13);
        let engine = MessageEncryption::new(Arc::new(Key));
        let activation = engine
            .select_session(Some(EncryptionSession {
                owner: owner.clone(),
                generation: SessionGeneration(2),
                device_id: Uuid::from_u128(14),
            }))
            .unwrap();
        let ciphertext = engine
            .prepare(&activation, stream, key)
            .unwrap()
            .seal("карточка [файл](https://private.test/path)")
            .unwrap();
        let raw = json!({
            "uuid": message, "stream_uuid": stream, "topic_uuid": topic,
            "project_id": "project", "user_uuid": "user", "author_uuid": Uuid::from_u128(15),
            "created_at": "2026-01-01T12:00:00Z", "source_name": "native",
            "payload": {"kind": "e2ee", "key_uuid": key, "content": ciphertext},
        });
        let decode = |row| {
            serde_json::from_value::<WireHistoryMessage>(row)
                .unwrap()
                .into_presentation(&owner)
        };
        let mut foreign = raw.clone();
        foreign["user_uuid"] = json!("other");
        assert!(decode(foreign).is_err());
        let preview = make_preview(decode(raw).unwrap().unwrap(), &owner, &HashMap::new()).unwrap();
        assert_eq!((preview.message_id, preview.topic_id), (message, topic));
        assert_eq!(
            preview.body(),
            &SidebarPreviewBody::Encrypted {
                key_id: key,
                ciphertext,
                owner: owner.clone(),
                stream_id: stream,
            }
        );
        assert!(matches!(
            preview.prepare_summary(Uuid::from_u128(16), &engine, &activation),
            Err(DecryptionError::Stale)
        ));
        let summary = preview
            .prepare_summary(stream, &engine, &activation)
            .unwrap()
            .unwrap();
        assert_eq!(
            summary.with_text(str::to_owned).as_deref(),
            Some("карточка файл")
        );
        engine.select_session(None);
        assert_eq!(summary.with_text(str::to_owned), None);
        assert_eq!(preview.body().fallback_text(), "Нужен ключ");
    }
}
