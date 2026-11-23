//! Проверяет источники ответа, кодирует ссылки цитирования и читает их в Markdown.

mod protected;
mod resolved;

pub(in crate::messenger) use protected::{ProtectedReferences, project_owned, project_protected};
pub(in crate::messenger) use resolved::ProtectedQuoteMediaPermit;
pub use resolved::{ProtectedQuote, ResolvedProtectedQuote};

use std::{collections::HashMap, fmt::Write, ops::Range};

use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::Service,
    context::{OwnerContext, SessionGeneration},
    storage::{HistoryMessage, HistoryScope, MessageBody, Storage},
    sync::{Sync, SyncError},
};

use super::{MessengerSource, encryption::EncryptionActivation, history::OpenHistoryError};

const ESCAPED: &str = "\\`*_{()[]#+.!|>~-";

/// Источник из снимка обычного ответа, который нужно проверить до записи.
#[derive(Clone)]
pub struct Source {
    pub message_id: Uuid,         // Публичный UUID подтверждённой строки.
    pub fragment: Option<String>, // Точный текст фрагмента, если пользователь выделил его.
}

/// Причина отказа в проверке источников ответа.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SourceCheckError {
    #[error("the quoted message is no longer accessible in this conversation")]
    Missing, // Строка скрыта, удалена или переехала в другую область.
    #[error("the selected text no longer matches the quoted message")]
    Changed, // Выделенный ранее фрагмент больше не содержится в теле.
    #[error("could not verify the quoted messages")]
    Unavailable, // Сессия или запрос не дали подтверждённого ответа.
}

impl From<OpenHistoryError> for SourceCheckError {
    /// Отбрасывает чувствительные детали запроса и сохраняет причину для редактора.
    fn from(error: OpenHistoryError) -> Self {
        match error {
            OpenHistoryError::AccessLost
            | OpenHistoryError::MessageMissing
            | OpenHistoryError::InvalidTarget => Self::Missing,
            _ => Self::Unavailable,
        }
    }
}

/// Версии оригинала и права чтения, которые должен пережить запрос.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct QuoteSourceFence {
    state: (i64, i64), // Поколение истории и последняя известная правка оригинала.
    access: i64,       // Версия отзыва доступа к стриму или теме.
}

impl QuoteSourceFence {
    /// Сверяет отзыв в точной теме перед записью пересылки.
    pub(crate) fn scope_still_accessible(
        &self,
        storage: &Storage,
        owner: &OwnerContext,
        original: &HistoryMessage,
    ) -> Result<bool, SyncError> {
        let scope = HistoryScope {
            stream_id: original.stream_id,
            topic_id: Some(original.topic_id),
        };
        Ok(storage.media_access_revision(owner, scope)? == self.access)
    }

    /// Читает локальные версии при действующем поколении авторизации.
    pub(super) fn capture(
        storage: &Storage,
        owner: &OwnerContext,
        scope: HistoryScope,
        message_id: Uuid,
    ) -> Result<Self, OpenHistoryError> {
        let state = storage
            .quote_source_revision(owner, message_id)
            .map_err(SyncError::from)?
            .ok_or(OpenHistoryError::MessageMissing)?;
        let access = storage
            .media_access_revision(owner, scope)
            .map_err(SyncError::from)?;
        Ok(Self { state, access })
    }
}

impl MessengerSource {
    /// Подтверждает точные версии целых защищённых оригиналов перед записью ответа.
    pub async fn verify_protected_reply_sources(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        scope: HistoryScope,
        originals: &[HistoryMessage],
    ) -> Result<(), SourceCheckError> {
        if originals.is_empty() {
            return Ok(());
        }
        let session = activation.session();
        let engine = &self
            .encryption_profile(activation)
            .map_err(|_| SourceCheckError::Unavailable)?
            .keys;
        for expected in originals {
            let current = self
                .load_quote_original(sync, &session.owner, session.generation, scope, expected.id)
                .await?;
            if &current != expected {
                return Err(SourceCheckError::Changed);
            }
            let MessageBody::Encrypted { key_id, ciphertext } = current.body else {
                return Err(SourceCheckError::Missing);
            };
            let engine = engine.clone();
            let activation = activation.clone();
            tokio::task::spawn_blocking(move || {
                let result = engine
                    .prepare(&activation, current.stream_id, key_id)
                    .map_err(|_| SourceCheckError::Unavailable)?
                    .decrypt(&ciphertext);
                engine
                    .accept(result)
                    .ok()
                    .and_then(|body| body.with_body(|text| !text.trim().is_empty()))
                    .filter(|readable| *readable)
                    .map(|_| ())
                    .ok_or(SourceCheckError::Missing)
            })
            .await
            .map_err(|_| SourceCheckError::Unavailable)??;
        }
        Ok(())
    }

    /// Загружает оригинал из подтверждённой области без переноса текста в историю ответа.
    pub async fn load_quote_original(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        message_id: Uuid,
    ) -> Result<HistoryMessage, OpenHistoryError> {
        self.quote_original_with_fence(sync, owner, generation, Some(scope), message_id)
            .await
            .map(|(message, _)| message)
    }

    /// Находит исходный адрес цитаты по UUID и сверяет право чтения оригинала.
    pub async fn resolve_quote_original(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        message_id: Uuid,
    ) -> Result<HistoryMessage, OpenHistoryError> {
        self.quote_original_with_fence(sync, owner, generation, None, message_id)
            .await
            .map(|(message, _)| message)
    }

    /// Читает оригинал и сохраняет локальную версию для незавершённой загрузки вложения.
    pub(super) async fn quote_original_with_fence(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        expected_scope: Option<HistoryScope>,
        message_id: Uuid,
    ) -> Result<(HistoryMessage, QuoteSourceFence), OpenHistoryError> {
        if message_id.is_nil()
            || expected_scope.is_some_and(|scope| {
                scope.stream_id.is_nil() || scope.topic_id.is_some_and(|id| id.is_nil())
            })
        {
            return Err(OpenHistoryError::InvalidTarget);
        }
        let auth = sync.auth();
        let storage = sync.storage();
        let revision = auth
            .with_active_generation(owner, generation, || {
                storage.quote_source_revision(owner, message_id)
            })?
            .map_err(SyncError::from)?
            .ok_or(OpenHistoryError::MessageMissing)?;
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let original = self
            .rest
            .presentation_anchor(owner, &token, expected_scope, message_id)
            .await?;
        let scope = HistoryScope {
            stream_id: original.stream_id,
            topic_id: Some(original.topic_id),
        };
        let verify = || -> Result<QuoteSourceFence, OpenHistoryError> {
            let current = QuoteSourceFence::capture(storage, owner, scope, message_id)?;
            if current.state != revision
                || !storage
                    .quote_source_matches(owner, &original)
                    .map_err(SyncError::from)?
            {
                return Err(OpenHistoryError::StaleWindow);
            }
            Ok(current)
        };
        let fence = auth.with_active_generation(owner, generation, verify)??;
        if let Some(direct) = expected_scope.filter(|scope| scope.topic_id.is_none()) {
            self.rest.check_history_scope(owner, &token, direct).await?;
        }
        self.rest
            .check_presentation_topic(owner, &token, original.stream_id, original.topic_id)
            .await?;
        if auth.with_active_generation(owner, generation, verify)?? != fence {
            return Err(OpenHistoryError::StaleWindow);
        }
        Ok((original, fence))
    }

    /// Подтверждает видимость и фрагменты цитат по исходному владельцу перед записью снимка.
    pub async fn verify_reply_sources(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        sources: &[Source],
    ) -> Result<(), SourceCheckError> {
        if sources.is_empty() {
            return Ok(());
        }

        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await
            .map_err(|_| SourceCheckError::Unavailable)?;
        let mut fragments: HashMap<Uuid, Vec<&str>> = HashMap::new();
        for source in sources {
            if source.message_id.is_nil() {
                return Err(SourceCheckError::Missing);
            }
            fragments
                .entry(source.message_id)
                .or_default()
                .extend(source.fragment.as_deref());
        }
        for (id, fragments) in fragments {
            // REST сверяет UUID, пользователя, проект и точную область сообщения.
            let checked = self.rest.history_anchor(owner, &token, scope, id).await;
            auth.with_active_generation(owner, generation, || ())
                .map_err(|_| SourceCheckError::Unavailable)?;
            let source = checked?;
            let Some(text) = source
                .body
                .markdown()
                .filter(|text| !text.trim().is_empty())
            else {
                return Err(SourceCheckError::Missing);
            };
            if fragments
                .iter()
                .any(|fragment| fragment.trim().is_empty() || !text.contains(fragment))
            {
                return Err(SourceCheckError::Changed);
            }
        }
        Ok(())
    }
}

/// Одна распознанная ссылка на исходное сообщение.
#[derive(Debug, Eq, PartialEq)]
pub struct Reference {
    pub message_id: Uuid,         // Публичный UUID исходной строки.
    pub author: String,           // Подпись из Markdown, если автор исходной строки недоступен.
    pub fragment: Option<String>, // Выделенный фрагмент без URL-кодирования.
}

/// Блок текста или отдельная цитата, как её записывает композер.
#[derive(Debug, Eq, PartialEq)]
pub enum Block<'a> {
    Text(&'a str),    // Исходный Markdown без интерпретации.
    Quote(Reference), // Валидная явная ссылка на отдельной строке.
}

/// Собирает Markdown-ссылку Electron с экранированной подписью и UTF-8 фрагментом.
pub fn reference(message_id: Uuid, author: &str, fragment: Option<&str>) -> String {
    let mut result = String::from("[");
    for ch in author.trim().chars() {
        if ESCAPED.contains(ch) {
            result.push('\\');
        }
        result.push(if matches!(ch, '\r' | '\n') { ' ' } else { ch });
    }
    write!(result, "](urn:quote:{message_id}").unwrap();
    if let Some(fragment) = fragment.filter(|text| !text.trim().is_empty()) {
        result.push_str("?text=");
        for byte in fragment.bytes() {
            if unreserved(byte) {
                result.push(byte.into());
            } else {
                write!(result, "%{byte:02X}").unwrap();
            }
        }
    }
    result.push(')');
    result
}

/// Проверяет символ без экранирования в параметре фрагмента.
fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// Декодирует только корректный UTF-8 параметр, без частичных ответов.
fn fragment(text: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut input = text.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        match byte {
            b'%' => {
                let hi = char::from(input.next()?).to_digit(16)?;
                let lo = char::from(input.next()?).to_digit(16)?;
                bytes.push((hi * 16 + lo) as u8);
            }
            byte if unreserved(byte) => bytes.push(byte),
            _ => return None,
        }
    }
    String::from_utf8(bytes)
        .ok()
        .filter(|text| !text.trim().is_empty())
}

/// Разбирает ровно одну ссылку, оставляя прочий Markdown текстом.
fn parse(line: &str) -> Option<Reference> {
    let (author, destination) = line
        .strip_prefix('[')?
        .strip_suffix(')')?
        .rsplit_once("](urn:quote:")?;
    let mut label = String::new();
    let mut chars = author.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => label.push(chars.next().filter(|ch| ESCAPED.contains(*ch))?),
            '[' | ']' | '`' => return None,
            ch => label.push(ch),
        }
    }
    if label.trim().is_empty() {
        return None;
    }
    let (id, encoded) = destination
        .split_once("?text=")
        .map_or((destination, None), |(id, encoded)| (id, Some(encoded)));
    if id.len() != 36 {
        return None;
    }
    let message_id = Uuid::parse_str(id).ok().filter(|id| !id.is_nil())?;
    let fragment = match encoded {
        Some(text) => Some(fragment(text)?),
        None => None,
    };
    Some(Reference {
        message_id,
        author: label,
        fragment,
    })
}

/// Узнаёт открывающую или закрывающую строку блока кода.
fn fence(line: &str) -> Option<(u8, usize, bool)> {
    let stripped = line.trim_start_matches(' ');
    if line.len() - stripped.len() > 3 {
        return None;
    }
    let marker = *stripped.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let count = stripped.bytes().take_while(|&byte| byte == marker).count();
    (count >= 3).then(|| (marker, count, stripped[count..].trim().is_empty()))
}

/// Находит те же самостоятельные цитаты вместе с их точными позициями в исходном Markdown.
fn quote_lines(markdown: &str) -> impl Iterator<Item = (Range<usize>, Reference)> + '_ {
    let mut result = Vec::new();
    let mut code = None;
    let mut boundary = true;
    let mut line_start = 0;
    let mut lines = markdown.split_inclusive('\n').peekable();
    while let Some(raw) = lines.next() {
        let line = raw.trim_end_matches(['\r', '\n']);
        let line_end = line_start + raw.len();
        if let Some((marker, length, plain)) = fence(line) {
            match code {
                Some((active, size)) if plain && active == marker && length >= size => code = None,
                None => code = Some((marker, length)),
                _ => {}
            }
        } else if code.is_none()
            && boundary
            && lines.peek().is_none_or(|next| next.trim().is_empty())
            && let Some(reference) = parse(line)
        {
            result.push((line_start..line_start + line.len(), reference));
        }
        boundary = line.trim().is_empty();
        line_start = line_end;
    }
    result.into_iter()
}

/// Выделяет самостоятельные цитаты между пустыми строками. Ссылки внутри кода остаются текстом.
pub fn blocks(markdown: &str) -> Vec<Block<'_>> {
    if !markdown.contains("](urn:quote:") {
        return vec![Block::Text(markdown)];
    }

    let mut result = Vec::new();
    let mut text_start = 0;
    for (line, reference) in quote_lines(markdown) {
        let text = markdown[text_start..line.start].trim_matches(['\r', '\n']);
        if !text.is_empty() {
            result.push(Block::Text(text));
        }
        result.push(Block::Quote(reference));
        text_start = line.end;
    }

    if result.is_empty() {
        return vec![Block::Text(markdown)];
    }
    let text = markdown[text_start..].trim_matches(['\r', '\n']);
    if !text.is_empty() {
        result.push(Block::Text(text));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Цитаты теряют снимок текста, а разметка, пробелы, код и адрес оригинала остаются.
    #[test]
    fn protected_projection_removes_only_structural_quote_excerpts() {
        let id = Uuid::from_u128(4);
        let first = reference(id, "Тест ?text=", Some("old-secret Ъ"));
        let link = reference(id, "Тест ?text=", None);
        let code = format!("```\r\n{first}\r\n```");
        let original = format!("**ответ**  \r\n\r\n{first}\r\n\r\n{code}\r\n\r\n{first}\r\n");
        let expected = format!("**ответ**  \r\n\r\n{link}\r\n\r\n{code}\r\n\r\n{link}\r\n");
        let (safe, _) = project_protected(&original);
        assert_eq!(safe, expected);
        assert!(matches!(
            project_protected(&safe).0,
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// Сверяет литералы Markdown и percent-encoding по формату Electron.
    #[test]
    fn quote_link_round_trips_utf8_and_markdown() {
        let id = Uuid::from_u128(4);
        let link = reference(id, "Анна [Тест]", Some("Я 😺!\n&"));
        assert_eq!(
            link,
            "[Анна \\[Тест\\]](urn:quote:00000000-0000-0000-0000-000000000004?text=%D0%AF%20%F0%9F%98%BA%21%0A%26)"
        );
        let markdown = format!("{link}\n\nОтвет");
        assert_eq!(
            blocks(&markdown),
            vec![
                Block::Quote(Reference {
                    message_id: id,
                    author: "Анна [Тест]".into(),
                    fragment: Some("Я 😺!\n&".into())
                }),
                Block::Text("Ответ"),
            ]
        );
    }

    /// Проверяет, что похожие строки в коде и ошибочные ссылки не порождают карточки.
    #[test]
    fn leaves_fences_escaped_and_invalid_links_literal() {
        let link = reference(Uuid::from_u128(4), "Автор", None);
        for input in [
            format!("```rust\n\n{link}\n\n```"),
            format!("~~~\n\n{link}\n\n~~~"),
            format!("    {link}"),
            format!("\\{link}"),
            format!("`{link}`"),
            format!("{link} с текстом"),
            link.replace(')', "?text=%FG)"),
            format!("Параграф\n{link}"),
        ] {
            assert_eq!(blocks(&input), [Block::Text(&input)]);
        }
        assert!(matches!(
            &blocks(&format!("```\nкод\n```\n\n{link}"))[..],
            [Block::Text(_), Block::Quote(_)]
        ));
    }
}
