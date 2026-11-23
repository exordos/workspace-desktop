//! Проверяет исходник защищённой цитаты и готовит короткий отзываемый вывод.

use std::sync::Arc;

use markdown::{ParseOptions, mdast::Node, to_mdast};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    auth::ActiveGeneration,
    storage::{HistoryMessage, HistoryScope, MessageBody, media::MediaKind},
    sync::{QuoteOriginPermit, Sync, SyncError},
};

use super::{MessengerSource, OpenHistoryError, QuoteSourceFence, project_protected};
use crate::messenger::{
    encryption::{EncryptionActivation, MessageReadPermission, ProtectedMessage},
    references::media_references,
};

const EXCERPT_LENGTH: usize = 160;

/// Авторитетное размещение цитаты, включая исходник без ключа на этом устройстве.
pub struct ResolvedProtectedQuote {
    pub scope: HistoryScope,             // Точный стрим и тема оригинала.
    pub key_id: Option<Uuid>,            // Исходное поколение; None для обычного сообщения.
    pub own: bool,                       // Автор оригинала совпадает с текущим входом.
    pub content: Option<ProtectedQuote>, // Отзываемое содержимое, если оно раскрыто.
}

/// Разрешает только подготовленную выдержку и IDs вложений, не выдавая тела оригинала.
pub struct ProtectedQuote {
    access: QuoteAccess,                // Действующие версии двух строк и их ключей.
    source: Uuid,                       // Публичный UUID исходного сообщения.
    citing_scope: HistoryScope,         // Точная тема цитирующего сообщения.
    excerpt: Option<Zeroizing<String>>, // Только проверенная открытая строка для цитаты.
    files: Vec<(Uuid, MediaKind)>,      // Публичные ссылки из целого исходника, без подписей.
    generation: ActiveGeneration,       // Проверка входа без SQLite, сети и владения профилем.
}

/// Разрешения двух строк, которыми можно пользоваться независимо от их отображения.
#[derive(Clone)]
struct QuoteAccess {
    citing: QuoteOriginPermit,                   // Точное цитирующее сообщение.
    original: QuoteOriginPermit,                 // Точный исходный снимок.
    citing_key: MessageReadPermission,           // Действующая версия ключа цитирующего тела.
    original_key: Option<MessageReadPermission>, // None для оригинала с открытым Markdown.
}

impl QuoteAccess {
    /// Выполняет чтение в уже проверенном входе под двумя версиями строк и ключей.
    fn with_current<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        self.citing
            .with_linked(&self.original, || match &self.original_key {
                Some(original) => self.citing_key.with_linked(original, read),
                None => self.citing_key.with_current(read),
            })
            .flatten()
    }
}

/// Разрешает краткое чтение вложения из оригинала без удержания открытого текста.
pub(in crate::messenger) struct ProtectedQuoteMediaPermit {
    access: QuoteAccess, // Действующие версии исходного и цитирующего сообщений.
    source: Uuid,        // Публичный UUID оригинала, из которого берётся вложение.
    citing_scope: HistoryScope, // Тема цитирующего сообщения, привязанная к окну истории.
}

impl ProtectedQuoteMediaPermit {
    /// Возвращает исходный UUID для сверки с точным REST-ответом.
    pub(in crate::messenger) fn source_message_id(&self) -> Uuid {
        self.source
    }

    /// Возвращает область цитирующей строки, чтобы запрос относился к тому же окну.
    pub(in crate::messenger) fn citing_scope(&self) -> HistoryScope {
        self.citing_scope
    }

    /// Сверяет, что сервер вернул тот оригинал, из которого была разрешена ссылка на файл.
    pub(in crate::messenger) fn matches_source(&self, source: &HistoryMessage) -> bool {
        self.access.original.matches_message(source)
    }

    /// Проверяет доступ внутри действующей сессии, которую удерживает вызывающий модуль.
    pub(in crate::messenger) fn with_current_under_auth<T>(
        &self,
        read: impl FnOnce() -> T,
    ) -> Option<T> {
        self.access.with_current(read)
    }
}

impl ProtectedQuote {
    /// Удерживает обе строки и их ключи на условиях короткого чтения без диска и сети.
    pub fn with_current<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        self.generation
            .with_current(|| self.access.with_current(read))
            .flatten()
    }

    /// Передаёт только проверенную строку либо визуальную подпись для пустого оригинала.
    pub fn with_text<T>(&self, placeholder: &str, read: impl FnOnce(&str) -> T) -> Option<T> {
        self.with_current(|| read(self.excerpt.as_deref().map_or(placeholder, String::as_str)))
    }

    /// Возвращает IDs вложений из целого защищённого оригинала под теми же разрешениями.
    pub fn with_media_references<T>(
        &self,
        read: impl FnOnce(&[(Uuid, MediaKind)]) -> T,
    ) -> Option<T> {
        self.with_current(|| read(&self.files))
    }

    /// Выдаёт актуальное разрешение только на вложение проверенного целого оригинала.
    pub(in crate::messenger) fn media_permit(
        &self,
        activation: &EncryptionActivation,
        file_id: Uuid,
    ) -> Option<ProtectedQuoteMediaPermit> {
        if !self.access.citing_key.matches_activation(activation) {
            return None;
        }
        self.with_current(|| {
            self.files
                .iter()
                .any(|(id, _)| *id == file_id)
                .then(|| ProtectedQuoteMediaPermit {
                    access: self.access.clone(),
                    source: self.source,
                    citing_scope: self.citing_scope,
                })
        })
        .flatten()
    }
}

impl MessengerSource {
    /// Подтверждает точную ссылку, доступ и версию оригинала до подготовки отзываемого текста.
    pub async fn resolve_protected_quote(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        parent: Arc<ProtectedMessage>,
        citing: &HistoryMessage,
        source: Uuid,
        position: usize,
    ) -> Result<ResolvedProtectedQuote, OpenHistoryError> {
        if !parent.matches_activation(activation) {
            return Err(OpenHistoryError::AccessLost);
        }
        if !parent.matches_envelope(citing) {
            return Err(OpenHistoryError::StaleWindow);
        }
        let parent_guard = || {
            parent
                .with_current(|| {
                    parent.reference_excerpt(source, position).map(|excerpt| {
                        (
                            excerpt.map(|text| Zeroizing::new(text.to_owned())),
                            parent.reference_is_whole(source, position),
                        )
                    })
                })
                .flatten()
        };
        let (legacy_excerpt, whole) = parent_guard().ok_or(OpenHistoryError::MessageMissing)?;
        let profile = self
            .encryption_profile(activation)
            .map_err(|_| OpenHistoryError::AccessLost)?;
        let session = activation.session();
        let citing_scope = HistoryScope {
            stream_id: citing.stream_id,
            topic_id: Some(citing.topic_id),
        };
        let citing =
            sync.auth()
                .with_active_generation(&session.owner, session.generation, || {
                    if !sync
                        .storage()
                        .matches_encrypted_edit(&session.owner, citing)
                        .map_err(SyncError::from)?
                    {
                        return Err(OpenHistoryError::StaleWindow);
                    }
                    sync.capture_quote_origin(&session.owner, session.generation, citing)
                        .ok_or(OpenHistoryError::StaleWindow)
                })??;
        let (original, fence) = self
            .quote_original_with_fence(sync, &session.owner, session.generation, None, source)
            .await?;
        let scope = HistoryScope {
            stream_id: original.stream_id,
            topic_id: Some(original.topic_id),
        };
        let origin =
            sync.auth()
                .with_active_generation(&session.owner, session.generation, || {
                    let current =
                        QuoteSourceFence::capture(sync.storage(), &session.owner, scope, source)?;
                    if current != fence
                        || !sync
                            .storage()
                            .quote_source_matches(&session.owner, &original)
                            .map_err(SyncError::from)?
                    {
                        return Err(OpenHistoryError::StaleWindow);
                    }
                    sync.capture_quote_origin(&session.owner, session.generation, &original)
                        .ok_or(OpenHistoryError::StaleWindow)
                })??;
        let own = original.metadata.author_id.is_some()
            && original.metadata.author_id == session.owner.user_id.parse().ok();
        let engine = Arc::clone(&profile.keys);
        let activation = activation.clone();
        let key_id = match &original.body {
            MessageBody::Encrypted { key_id, .. } => Some(*key_id),
            MessageBody::Markdown(_) => None,
            MessageBody::KeyService { .. } => return Err(OpenHistoryError::MessageMissing),
        };
        let prepared = tokio::task::spawn_blocking(move || {
            let excerpt = legacy_excerpt.as_deref().map(String::as_str);
            match original.body {
                MessageBody::Markdown(text) => {
                    let (projected, _) = project_protected(&text);
                    Some((None, visible_excerpt(&projected, excerpt), Vec::new()))
                }
                MessageBody::Encrypted { key_id, ciphertext } => {
                    let holder = engine
                        .accept(
                            engine
                                .prepare(&activation, scope.stream_id, key_id)
                                .ok()?
                                .decrypt(&ciphertext),
                        )
                        .ok()?;
                    let (excerpt, media) = holder.with_body(|markdown| {
                        let media = if whole {
                            media_references(markdown)
                                .into_iter()
                                .map(|file| (file.file_id, file.kind))
                                .collect()
                        } else {
                            Vec::new()
                        };
                        (visible_excerpt(markdown, excerpt), media)
                    })?;
                    Some((Some(holder.into_permission()), excerpt, media))
                }
                MessageBody::KeyService { .. } => None,
            }
        })
        .await
        .map_err(|_| OpenHistoryError::StaleWindow)?;
        let content = prepared.and_then(|(original_key, excerpt, files)| {
            let content = ProtectedQuote {
                access: QuoteAccess {
                    citing,
                    original: origin,
                    citing_key: parent.read_permission(),
                    original_key,
                },
                source,
                citing_scope,
                excerpt,
                files,
                generation: sync
                    .auth()
                    .active_generation(&session.owner, session.generation),
            };
            content.with_current(|| ()).map(|()| content)
        });
        Ok(ResolvedProtectedQuote {
            scope,
            key_id,
            own,
            content,
        })
    }
}

/// Ищет старую подсказку только в видимом тексте, не разрезая вложенные ссылки и блоки.
fn visible_excerpt(markdown: &str, selected: Option<&str>) -> Option<Zeroizing<String>> {
    let document = to_mdast(markdown, &ParseOptions::gfm()).ok()?;
    let mut segments = Vec::new();
    let mut current = Zeroizing::new(String::new());
    visible_text(&document, &mut current, &mut segments);
    finish_segment(&mut current, &mut segments);
    let text = selected
        .filter(|selected| {
            !selected.trim().is_empty() && segments.iter().any(|span| span.contains(selected))
        })
        .or_else(|| segments.first().map(|span| span.as_str()))?;
    let line = text.trim().lines().next()?;
    let mut chars = line.chars();
    let mut result = Zeroizing::new(chars.by_ref().take(EXCERPT_LENGTH).collect::<String>());
    if chars.next().is_some() {
        result.push('…');
    }
    Some(result)
}

/// Собирает непрерывный видимый текст. Закрытые и вложенные ссылки разрывают участки.
fn visible_text(
    node: &Node,
    current: &mut Zeroizing<String>,
    segments: &mut Vec<Zeroizing<String>>,
) {
    let skip = match node {
        Node::Blockquote(_)
        | Node::Definition(_)
        | Node::Html(_)
        | Node::Image(_)
        | Node::ImageReference(_)
        | Node::LinkReference(_) => true,
        Node::Link(link) => service_urn(&link.url),
        _ => false,
    };
    if skip {
        finish_segment(current, segments);
        return;
    }
    let text = match node {
        Node::Text(text) => Some(text.value.as_str()),
        Node::Code(code) => Some(code.value.as_str()),
        Node::InlineCode(code) => Some(code.value.as_str()),
        Node::Break(_) => Some("\n"),
        _ => None,
    };
    if let Some(text) = text {
        if service_urn(text) {
            finish_segment(current, segments);
        } else {
            current.push_str(text);
        }
        return;
    }
    let block = matches!(
        node,
        Node::Paragraph(_) | Node::Heading(_) | Node::ListItem(_) | Node::TableCell(_)
    );
    if block {
        finish_segment(current, segments);
    }
    for child in node.children().into_iter().flatten() {
        visible_text(child, current, segments);
    }
    if block {
        finish_segment(current, segments);
    }
}

/// Служебные ссылки и закрытые снимки не считаются видимым оригинальным текстом.
fn service_urn(text: &str) -> bool {
    text.as_bytes()
        .windows(4)
        .any(|bytes| bytes.eq_ignore_ascii_case(b"urn:"))
}

/// Сохраняет только содержательные участки и освобождает остальные.
fn finish_segment(current: &mut Zeroizing<String>, segments: &mut Vec<Zeroizing<String>>) {
    let segment = std::mem::take(current);
    if !segment.trim().is_empty() {
        segments.push(segment);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Совпадение текста в адресе, подписи закрытой ссылки или снимке не подтверждает выдержку.
    #[test]
    fn selected_excerpt_matches_only_visible_original_text() {
        let nested = format!(
            "[nested label](urn:quote:{}?text=nested-private)",
            Uuid::from_u128(1)
        );
        let fragment = "[closed fragment](urn:secret:v1:placeholder)";
        let forward = format!(
            "[forward label](urn:forward:{}?snapshot=forward-private)",
            Uuid::from_u128(2)
        );
        let only_protected = format!("{nested}\n\n{fragment}\n\n{forward}");
        let text = format!("Visible **source** text\n\n{only_protected}\n\nLast *visible piece*");
        let (safe, _) = project_protected(&text);
        assert_eq!(
            visible_excerpt(&safe, Some("visible piece"))
                .as_deref()
                .map(String::as_str),
            Some("visible piece")
        );
        for selected in [
            "nested label",
            "nested-private",
            "closed fragment",
            "forward label",
            "forward-private",
        ] {
            assert_eq!(
                visible_excerpt(&safe, Some(selected))
                    .as_deref()
                    .map(String::as_str),
                Some("Visible source text")
            );
            assert!(visible_excerpt(&only_protected, Some(selected)).is_none());
        }
    }
}
