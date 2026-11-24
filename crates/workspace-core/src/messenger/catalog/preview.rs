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
    storage::{
        CatalogMessage, HistoryMessage, HistoryScope, HistoryTime, KeyServicePayload, MessageBody,
    },
};

use super::{Catalog, CatalogError, Chat};

mod text;
pub use text::{MessagePreview, PreviewPart};

const BATCH_SIZE: usize = 100;
const CONCURRENT_BATCHES: usize = 3;
const TEXT_LIMIT: usize = 240;

/// Последнее подтверждённое сообщение, которое можно показать в строке списка.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SidebarPreview {
    pub message_id: Uuid,            // UUID последнего известного сообщения.
    pub topic_id: Uuid,              // Тема, в которой лежит сообщение.
    pub author_id: Uuid,             // UUID автора или nil, если журнал его не передал.
    pub author_name: Option<String>, // Имя автора, если оно уже есть в снимке каталога.
    pub created_at: HistoryTime,     // Время создания сообщения в UTC.
    pub text: SidebarPreviewBody,    // Обычная подпись или исходный зашифрованный конверт.
}

/// Содержимое фонового превью. Расшифрованный текст в этой модели не хранится.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SidebarPreviewBody {
    Summary(MessagePreview), // Короткие части из обычного Markdown.
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
        Self::Summary(text.into())
    }
}

impl From<String> for SidebarPreviewBody {
    /// Принимает подготовленную подпись обычного сообщения.
    fn from(text: String) -> Self {
        Self::Summary(text.into())
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
            .with_body(|markdown| Zeroizing::new(MessagePreview::short(markdown, TEXT_LIMIT)))
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
    text: Zeroizing<MessagePreview>,   // Отзывные части превью для оформления интерфейсом.
}

impl ProtectedSidebarSummary {
    /// Читает короткое превью только до отзыва ключа.
    pub fn with_preview<T>(&self, read: impl FnOnce(&MessagePreview) -> T) -> Option<T> {
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
                let preview = make_preview(&row, owner, &names)?;
                Some((preview.message_id, (stream, preview)))
            }));
        }
        // При публикации сверяем UUID и маршрут каждой строки с текущим каталогом.
        Ok(auth.with_active_generation(owner, session.generation, || previews)?)
    }
}

/// Собирает подпись обычного сообщения или переносит конверт без расшифровки.
fn make_preview(
    row: &HistoryMessage,
    owner: &OwnerContext,
    names: &HashMap<Uuid, String>,
) -> Option<SidebarPreview> {
    let author_id = row.metadata.author_id.unwrap_or(Uuid::nil());
    let text = match &row.body {
        MessageBody::Markdown(text) => {
            let summary = MessagePreview::short(text, TEXT_LIMIT);
            if summary.is_empty() {
                return None;
            }
            SidebarPreviewBody::Summary(summary)
        }
        MessageBody::Encrypted { key_id, ciphertext } => SidebarPreviewBody::Encrypted {
            key_id: *key_id,
            ciphertext: ciphertext.clone(),
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
        created_at: row.metadata.created_at.clone(),
        text,
    })
}

/// Подтверждённое сообщение из общей базы или из точного запроса каталога.
#[derive(Clone, Copy)]
enum Candidate<'a> {
    Remote(&'a SidebarPreview), // Тело, полученное по UUID из снимка каталога.
    Local(&'a CatalogMessage),  // Строка из общей базы, доступная истории.
}

impl Candidate<'_> {
    /// Использует серверный порядок по времени создания и публичному UUID.
    fn order(&self) -> (&HistoryTime, Uuid) {
        match self {
            Self::Remote(preview) => (&preview.created_at, preview.message_id),
            Self::Local(row) => (&row.message.metadata.created_at, row.message.id),
        }
    }

    /// Готовит UUID и превью вместе, включая случай, когда тело пока нельзя показать.
    fn presentation(
        self,
        owner: &OwnerContext,
        own_id: Option<Uuid>,
        names: &HashMap<Uuid, String>,
    ) -> (Option<Uuid>, Option<SidebarPreview>) {
        let mut preview = match self {
            Self::Remote(fetched) => Some(fetched.clone()),
            Self::Local(row) if row.can_preview => make_preview(&row.message, owner, names),
            Self::Local(_) => None,
        };
        if let Some(preview) = &mut preview
            && Some(preview.author_id) == own_id
        {
            preview.author_name = Some("Вы".to_owned());
        }
        (Some(self.order().1), preview)
    }
}

/// Сравнивает порядок двух известных сообщений, если они есть.
fn latest<'a>(a: Option<Candidate<'a>>, b: Option<Candidate<'a>>) -> Option<Candidate<'a>> {
    a.into_iter()
        .chain(b)
        .max_by(|a, b| a.order().cmp(&b.order()))
}

/// Выбирает самое свежее подтверждённое сообщение отдельно для темы и её разговора.
pub(super) fn attach(
    owner: &OwnerContext,
    chats: &mut [Chat],
    messages: &HashMap<HistoryScope, CatalogMessage>,
    previews: Option<&SidebarPreviews>,
) {
    let own_id = owner.user_id.parse::<Uuid>().ok();
    let names = chats
        .iter()
        .filter_map(|chat| Some((chat.metadata.direct_peer_id?, chat.name.clone())))
        .collect();
    for chat in chats {
        let get = |last_message_id: Option<Uuid>| {
            let (stream_id, preview) = previews?.get(&last_message_id?)?;
            (*stream_id == chat.id).then_some(preview)
        };
        let remote = get(chat.metadata.last_message_id)
            .filter(|preview| {
                chat.direct || chat.topics.iter().any(|topic| topic.id == preview.topic_id)
            })
            .map(Candidate::Remote);
        let scope = HistoryScope {
            stream_id: chat.id,
            topic_id: None,
        };
        let mut chat_latest = latest(messages.get(&scope).map(Candidate::Local), remote);
        for topic in &mut chat.topics {
            let scope = HistoryScope {
                topic_id: Some(topic.id),
                ..scope
            };
            let remote = get(topic.metadata.last_message_id)
                .filter(|preview| preview.topic_id == topic.id)
                .map(Candidate::Remote);
            let local = messages.get(&scope).map(Candidate::Local);
            if let Some(message) = latest(local, remote) {
                (topic.metadata.last_message_id, topic.metadata.preview) =
                    message.presentation(owner, own_id, &names);
                chat_latest = latest(chat_latest, Some(message));
            }
        }
        if let Some(message) = chat_latest {
            (chat.metadata.last_message_id, chat.metadata.preview) =
                message.presentation(owner, own_id, &names);
        }
    }
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
        let preview =
            make_preview(&decode(raw).unwrap().unwrap(), &owner, &HashMap::new()).unwrap();
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
            summary.with_preview(Clone::clone),
            Some("карточка файл".into())
        );
        engine.select_session(None);
        assert_eq!(summary.with_preview(Clone::clone), None);
        assert!(matches!(preview.body(), SidebarPreviewBody::Encrypted { .. }));
    }
}
