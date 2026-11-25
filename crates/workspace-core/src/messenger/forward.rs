//! Кодирует нативную пересылку, разбирает снимок сообщения и проверяет её точные маршруты.

use std::{borrow::Cow, fmt::Write, ops::Range};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use markdown::{ParseOptions, mdast::Node, to_mdast};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::Service,
    context::{OwnerContext, SessionGeneration},
    storage::{DraftScope, HistoryScope, HistoryTime, PreparedSend},
    sync::{MAX_MESSAGE_CHARACTERS, PrepareSendError, Sync, SyncError},
};

use super::{
    MessengerSource,
    encryption::fragments::{fragment_links, has_protected_draft},
    quote::SourceCheckError,
    references::{display_label, media_references},
    send::DirectSendError,
};

const PREFIX: &str = "urn:forward:";
pub const MAX_FORWARD_DEPTH: usize = 3;

/// Источник, который пользователь видит в карточке пересылки.
#[derive(Debug, Eq, PartialEq)]
pub struct ForwardSource {
    pub label: String,           // Название исходного канала или личного чата.
    pub direct: bool,            // Источник был личным разговором.
    pub created_at: HistoryTime, // Время исходного сообщения.
}

/// Замороженный исходный снимок. Допускает обычный формат старого клиента.
#[derive(Debug, Eq, PartialEq)]
pub struct ForwardReference {
    pub id: Uuid,                      // Публичный UUID исходного сообщения.
    pub author: String,                // Подпись ссылки из исходного Markdown.
    pub snapshot: String,              // Исходный Markdown или буквальный открытый текст.
    pub plain: bool,                   // Electron переслал выделение из открытого сообщения.
    pub source: Option<ForwardSource>, // У старых пересылок подписи источника может не быть.
}

impl ForwardReference {
    /// Собирает стандартную ссылку Electron с безопасной подписью автора и снимком.
    pub fn to_markdown(&self) -> String {
        let author = self.author.chars().fold(String::new(), |mut label, ch| {
            if ch.is_ascii_punctuation() {
                label.push('\\');
            }
            label.push(if ch.is_control() { ' ' } else { ch });
            label
        });
        let mut link = format!(
            "[{author}]({PREFIX}{}?snapshot={}",
            self.id,
            URL_SAFE_NO_PAD.encode(&self.snapshot),
        );
        if self.plain {
            link.push_str("&format=plain");
        }
        if let Some(source) = &self.source {
            let _ = write!(link, "&source={}", URL_SAFE_NO_PAD.encode(&source.label));
            if source.direct {
                link.push_str("&source_kind=direct");
            }
            let _ = write!(
                link,
                "&created_at={}",
                source.created_at.as_str().replace(':', "%3A"),
            );
        }
        link.push(')');
        link
    }

    /// Проверяет формат снимка, обязательные пары полей и отсутствие повторённых полей.
    fn parse(url: &str, author: String) -> Option<Self> {
        if url.len() > MAX_MESSAGE_CHARACTERS || !is_forward_link(url) {
            return None;
        }
        let (id, query) = url[PREFIX.len()..].split_once('?')?;
        let uuid = Uuid::parse_str(id).ok()?;
        if uuid.is_nil() || !uuid.hyphenated().to_string().eq_ignore_ascii_case(id) {
            return None;
        }
        let (mut snapshot, mut format, mut label, mut kind, mut created_at) =
            (None, None, None, None, None);
        for parameter in query.split('&') {
            let (field, value) = parameter.split_once('=')?;
            let slot = match field {
                "snapshot" => &mut snapshot,
                "format" if value == "plain" => &mut format,
                "source" => &mut label,
                "source_kind" if value == "direct" => &mut kind,
                "created_at" => &mut created_at,
                _ => return None,
            };
            if slot.replace(value).is_some() {
                return None;
            }
        }
        let source = match (label, created_at) {
            (Some(label), Some(created_at)) => {
                let label = decode(label)?.trim().to_owned();
                if label.is_empty() || label.encode_utf16().count() > 512 {
                    return None;
                }
                Some(ForwardSource {
                    label,
                    direct: kind.is_some(),
                    created_at: HistoryTime::parse(percent_decoded(created_at)?.trim())?,
                })
            }
            (None, None) if kind.is_none() => None,
            _ => return None,
        };
        Some(Self {
            id: uuid,
            author,
            snapshot: decode(snapshot?)?,
            plain: format.is_some(),
            source,
        })
    }
}

/// Место пересылки или неисправная ссылка, которую интерфейс заменит закрытым блоком.
pub struct ForwardSpan {
    pub range: Range<usize>, // Байтовый диапазон исходного Markdown.
    pub reference: Option<ForwardReference>, // None скрывает неподдерживаемый или неверный адрес.
}

/// Проверяет зарезервированный префикс независимо от регистра.
pub fn is_forward_link(url: &str) -> bool {
    url.get(..PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
}

/// Сверяет ссылку открытой пересылки и возвращает только адрес оригинала.
pub(super) fn forward_original_id(url: &str) -> Option<Uuid> {
    ForwardReference::parse(url.trim(), String::new()).map(|reference| reference.id)
}

/// Разбирает отдельные ссылки снимка. Неподдерживаемые размещения скрываются по месту.
pub fn forward_spans(markdown: &str) -> Vec<ForwardSpan> {
    if !markdown
        .as_bytes()
        .windows(PREFIX.len())
        .any(|candidate| candidate.eq_ignore_ascii_case(PREFIX.as_bytes()))
    {
        return Vec::new();
    }
    let Ok(document) = to_mdast(markdown, &ParseOptions::gfm()) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    let mut pending = vec![(&document, false)];
    while let Some((node, standalone)) = pending.pop() {
        let (url, author) = match node {
            Node::Link(link) => (&link.url, standalone.then(|| display_label(node))),
            Node::Image(image) => (&image.url, None),
            Node::Definition(definition) => (&definition.url, None),
            Node::Paragraph(paragraph) => {
                let only_link = matches!(paragraph.children.as_slice(), [Node::Link(_)]);
                pending.extend(
                    paragraph
                        .children
                        .iter()
                        .rev()
                        .map(|child| (child, only_link)),
                );
                continue;
            }
            _ => {
                pending.extend(
                    node.children()
                        .into_iter()
                        .flatten()
                        .rev()
                        .map(|child| (child, false)),
                );
                continue;
            }
        };
        if let Some(position) = node.position().filter(|_| is_forward_link(url)) {
            spans.push(ForwardSpan {
                range: position.start.offset..position.end.offset,
                reference: author.and_then(|author| ForwardReference::parse(url, author)),
            });
        }
    }
    spans
}

/// Подставляет нейтральную подпись для карточек в обычных коротких превью.
pub fn forward_preview(markdown: &str) -> Cow<'_, str> {
    let spans = forward_spans(markdown);
    if spans.is_empty() {
        return Cow::Borrowed(markdown);
    }
    let mut preview = String::new();
    let mut end = 0;
    for span in spans {
        preview.push_str(&markdown[end..span.range.start]);
        preview.push_str("Пересланное сообщение");
        end = span.range.end;
    }
    preview.push_str(&markdown[end..]);
    Cow::Owned(preview)
}

/// Ищет настоящий защищённый фрагмент в оригинале или во вложенной пересылке.
pub fn contains_protected_snapshot(markdown: &str, depth: usize) -> bool {
    !fragment_links(markdown).is_empty()
        || depth > 0
            && forward_spans(markdown).into_iter().any(|span| {
                span.reference.is_some_and(|reference| {
                    !reference.plain && contains_protected_snapshot(&reference.snapshot, depth - 1)
                })
            })
}

/// Проверяет, требуется ли нативный маршрут для уже подготовленной пересылки.
pub(super) fn has_forwarded_fragment(markdown: &str) -> bool {
    forward_spans(markdown).into_iter().any(|span| {
        span.reference.is_some_and(|reference| {
            !reference.plain
                && contains_protected_snapshot(&reference.snapshot, MAX_FORWARD_DEPTH - 1)
        })
    })
}

/// Публичный оригинал из текущей истории и отображаемые подписи для снимка.
#[derive(Clone)]
pub struct ForwardOrigin {
    pub scope: HistoryScope, // Исходный разговор и его тема, если она известна.
    pub message_id: Uuid,    // Публичный UUID подтверждённого сообщения.
    pub author: String,      // Имя автора для подписи пересылки.
    pub chat: String,        // Название исходного разговора для подписи.
}

/// Адрес оригинала для существующей команды пересылки защищённых фрагментов.
pub type ProtectedForwardOrigin = ForwardOrigin;

/// Причина, по которой пересылка не поставлена в очередь.
#[derive(Debug, Error)]
pub enum ProtectedForwardError {
    #[error(transparent)]
    Source(#[from] SourceCheckError), // Оригинал отсутствует, изменился или недоступен.
    #[error(transparent)]
    Destination(#[from] DirectSendError), // Тема закрыта, недоступна или её тип изменился.
    #[error(transparent)]
    Queue(#[from] PrepareSendError), // Запись не поставлена в очередь.
    #[error(transparent)]
    Session(#[from] SyncError), // Сессия закрылась во время запроса.
    #[error("the original has no forwardable content")]
    InvalidSource, // Источник или выделение не подходят для снимка.
    #[error("the forwarded snapshot exceeds the message limit")]
    TooLong, // Строка со снимком и заголовком больше лимита отправки.
}

/// Выбирает допустимое тело снимка для старого маршрута фрагментов или меню сообщения.
enum ForwardContent<'a> {
    Protected,
    Message(Option<&'a str>),
}

/// Проверяет, поддерживает ли очередь пересылку такого тела в обычную тему.
pub fn can_forward_markdown(text: &str) -> bool {
    !text.trim().is_empty()
        && !has_protected_draft(text)
        && (contains_protected_snapshot(text, MAX_FORWARD_DEPTH - 1)
            || media_references(text).is_empty())
}

/// Для защищённых фрагментов сохраняет полный снимок, для обычных допускает выделение.
fn forward_snapshot<'a>(
    stored: &'a str,
    content: ForwardContent<'a>,
) -> Result<(&'a str, bool), ProtectedForwardError> {
    if !can_forward_markdown(stored) {
        return Err(ProtectedForwardError::InvalidSource);
    }
    if contains_protected_snapshot(stored, MAX_FORWARD_DEPTH - 1) {
        return Ok((stored, false));
    }
    match content {
        ForwardContent::Protected => Err(ProtectedForwardError::InvalidSource),
        ForwardContent::Message(None) => Ok((stored, false)),
        ForwardContent::Message(Some(fragment))
            if !fragment.is_empty() && stored.contains(fragment) =>
        {
            Ok((fragment, true))
        }
        ForwardContent::Message(Some(_)) => Err(ProtectedForwardError::InvalidSource),
    }
}

impl MessengerSource {
    /// Берёт канонический зашифрованный снимок и сохраняет его для отправки в обычную тему.
    pub async fn prepare_protected_forward(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        origin: &ProtectedForwardOrigin,
        target: HistoryScope,
    ) -> Result<PreparedSend, ProtectedForwardError> {
        self.prepare_forward_content(
            sync,
            owner,
            generation,
            origin,
            target,
            ForwardContent::Protected,
        )
        .await
    }

    /// Ставит один подтверждённый снимок или его открытый фрагмент в обычную очередь.
    pub async fn prepare_message_forward(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        origin: &ForwardOrigin,
        target: HistoryScope,
        fragment: Option<&str>,
    ) -> Result<PreparedSend, ProtectedForwardError> {
        self.prepare_forward_content(
            sync,
            owner,
            generation,
            origin,
            target,
            ForwardContent::Message(fragment),
        )
        .await
    }

    /// Проверяет источник и цель в той же сессии, затем сохраняет готовую пересылку.
    async fn prepare_forward_content(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        origin: &ForwardOrigin,
        target: HistoryScope,
        content: ForwardContent<'_>,
    ) -> Result<PreparedSend, ProtectedForwardError> {
        let access = sync
            .prepare_fragment_origins(
                owner,
                generation,
                &[origin.scope.stream_id, target.stream_id],
            )
            .map_err(SyncError::from)?;
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await
            .map_err(SyncError::from)?;
        let source_is_direct = self
            .rest
            .forward_source_is_direct(owner, &token, origin.scope.stream_id)
            .await
            .map_err(SourceCheckError::from)?;
        let topic_id = self.rest.fragment_send_topic(owner, &token, target).await?;
        let (original, fence) = self
            .quote_original_with_fence(
                sync,
                owner,
                generation,
                Some(origin.scope),
                origin.message_id,
            )
            .await
            .map_err(SourceCheckError::from)?;
        let stored = original
            .body
            .markdown()
            .ok_or(ProtectedForwardError::InvalidSource)?;
        let (snapshot, plain) = forward_snapshot(stored, content)?;
        let markdown = ForwardReference {
            id: original.id,
            author: display_or_id(
                &origin.author,
                original.metadata.author_id.unwrap_or(original.id),
            ),
            snapshot: snapshot.to_owned(),
            plain,
            source: Some(ForwardSource {
                label: display_or_id(&origin.chat, original.stream_id),
                direct: source_is_direct,
                created_at: original.metadata.created_at.clone(),
            }),
        }
        .to_markdown();
        if markdown.chars().count() > MAX_MESSAGE_CHARACTERS {
            return Err(ProtectedForwardError::TooLong);
        }
        Ok(sync.prepare_forward(
            owner,
            generation,
            DraftScope {
                stream_id: target.stream_id,
                topic_id,
            },
            markdown,
            (&original, fence),
            &access,
        )?)
    }
}

/// Сохраняет приемлемую подпись для Electron или использует стабильный UUID.
fn display_or_id(label: &str, id: Uuid) -> String {
    let label = label.trim();
    if label.is_empty() || label.encode_utf16().count() > 512 {
        id.to_string()
    } else {
        label.to_owned()
    }
}

/// Декодирует снимок в точном формате Base64url без завершающих знаков.
fn decode(value: &str) -> Option<String> {
    String::from_utf8(URL_SAFE_NO_PAD.decode(value).ok()?).ok()
}

/// Разбирает параметр времени с правилами decodeURIComponent, без замены плюса пробелом.
fn percent_decoded(value: &str) -> Option<String> {
    let mut encoded = value.bytes();
    let mut bytes = Vec::with_capacity(value.len());
    while let Some(byte) = encoded.next() {
        bytes.push(if byte == b'%' {
            let high = char::from(encoded.next()?).to_digit(16)?;
            let low = char::from(encoded.next()?).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()?
        } else {
            byte
        });
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Сверяет ссылку с отдельным JS-генератором формата Electron и разбирает тот же снимок.
    #[test]
    fn electron_wire_preserves_the_original_fragment_snapshot() {
        let expected = "[Ира](urn:forward:10000000-0000-4000-8000-000000000042?snapshot=0LTQviBb0YHQtdC60YDQtdGCXSh1cm46c2VjcmV0OnYxOjEwMDAwMDAwLTAwMDAtNDAwMC04MDAwLTAwMDAwMDAwMDAwNToxMDAwMDAwMC0wMDAwLTQwMDAtODAwMC0wMDAwMDAwMDAwODA6RlJZWEdCa2FHeHdkSGg4Z0lTSWpKQ1VtSnlncEtpc3NMUzR2TURFeU16UzdxdmtSY0Y1RDg5b2V0eFBaNWNLM3RmZUNwOFBRLWFiWmtBNFp3eFdLeGQwQTBzQWdSLWcpINC_0L7RgdC70LU&source=0J3QsNGC0LjQstC90YvQuSDRh9Cw0YI&source_kind=direct&created_at=2026-01-01T12%3A00%3A00.000000Z)";
        let link = forward_spans(expected).pop().unwrap().reference.unwrap();
        assert_eq!(link.to_markdown(), expected);
        assert_eq!(link.author, "Ира");
        assert_eq!(link.source.as_ref().unwrap().label, "Нативный чат");
        assert!(link.source.unwrap().direct);
        let fragments = fragment_links(&link.snapshot);
        assert_eq!(fragments.len(), 1);
        assert_eq!(
            fragments[0].reference.stream_id().to_string(),
            "10000000-0000-4000-8000-000000000005"
        );
        assert_eq!(
            fragments[0].reference.key_id().to_string(),
            "10000000-0000-4000-8000-000000000080"
        );
        assert!(has_forwarded_fragment(expected));
        assert_eq!(forward_preview(expected), "Пересланное сообщение");
        let repeated = expected.replace("&source=", "&snapshot=AQ&source=");
        assert!(forward_spans(&repeated)[0].reference.is_none());
    }
}
