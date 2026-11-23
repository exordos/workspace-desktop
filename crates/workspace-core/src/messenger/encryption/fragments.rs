//! Защищает отдельные фрагменты обычного Markdown.
//! Здесь описаны ссылка на шифртекст, разбор редактора и отзыв открытого текста.

use std::{
    borrow::Cow,
    ops::Range,
    sync::{Arc, Mutex, Weak},
};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use markdown::{ParseOptions, mdast::Node, to_mdast};
use ring::rand::{SecureRandom, SystemRandom};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    DecryptionAccess, DecryptionError, EncryptionActivation, EncryptionError,
    FragmentDecryptionResult, GroupKey, KEY_BYTES, MessageDecryption, MessageEncryption, Selection,
    key_address,
};
use crate::sync::MAX_MESSAGE_CHARACTERS;

mod drafts;
mod edits;
mod operations;
pub(in crate::messenger) use drafts::run_protected_draft_purges;
pub use edits::FragmentEdit;
pub use operations::FragmentMessageError;

const MARKER: &str = ":::";
const URN_PREFIX: &str = "urn:secret:";
const MAX_ENVELOPE_CHARACTERS: usize = 40_000;

/// Ссылка на конверт из смешанного сообщения, в том числе из пересылки.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FragmentReference {
    stream_id: Uuid,   // Исходный чат даже при чтении сообщения в другой теме или чате.
    key_id: Uuid,      // Поколение ключа, под которым зашифрован этот фрагмент.
    envelope: Vec<u8>, // Соль, шифртекст и тег из ссылки, без открытого текста.
}

impl FragmentReference {
    /// Разбирает ссылку формата Electron и проверяет каноническое кодирование её конверта.
    pub fn parse(link: &str) -> Option<Self> {
        let link = link.trim().strip_prefix("urn:secret:v1:")?;
        let (stream_id, rest) = link.split_once(':')?;
        let (key_id, envelope) = rest.split_once(':')?;
        if envelope.is_empty() || envelope.len() > MAX_ENVELOPE_CHARACTERS {
            return None;
        }
        Some(Self {
            stream_id: parse_hyphenated_uuid(stream_id)?,
            key_id: parse_hyphenated_uuid(key_id)?,
            envelope: URL_SAFE_NO_PAD.decode(envelope).ok()?,
        })
    }

    /// Возвращает исходный чат для чтения ключа и запроса доступа.
    pub fn stream_id(&self) -> Uuid {
        self.stream_id
    }

    /// Возвращает одно точное поколение для чтения и подписки на изменения.
    pub fn key_id(&self) -> Uuid {
        self.key_id
    }
}

/// Ссылка в исходной строке Markdown, которую можно заменить целиком.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FragmentLink {
    pub range: Range<usize>,          // Границы всей Markdown-ссылки в байтах.
    pub reference: FragmentReference, // Исходный чат, ключ и зашифрованные байты.
}

/// Вид содержимого, которым отображение заменяет служебный синтаксис в Markdown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FragmentDisplayKind {
    Reference(FragmentReference), // Доступность проверит исходный чат ссылки.
    Unavailable, // Зарезервированный адрес не разобрался, показываем закрытый фрагмент.
    Definition,  // Определение ссылки не рисуется в тексте сообщения.
}

/// Интервал исходной строки, в котором отображение заменяет служебный синтаксис.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FragmentDisplaySpan {
    pub range: Range<usize>,       // Байтовый диапазон ссылки или её определения.
    pub kind: FragmentDisplayKind, // Состояние, учитывающее только разбор формата.
}

/// Почему не получилось подготовить или открыть защищённые диапазоны.
#[derive(Debug, Error)]
pub enum FragmentError {
    #[error("invalid protected fragment")]
    InvalidFragment, // Диапазон пуст, не закрыт или границы ссылки нельзя подтвердить.
    #[error("protected message exceeds the markdown limit")]
    TooLong, // Готовая строка с шифртекстом превышает ограничение обычного сообщения.
    #[error(transparent)]
    Decryption(#[from] DecryptionError), // Исходное поколение недоступно или уже заменено.
    #[error(transparent)]
    Encryption(#[from] EncryptionError), // Ключ отправки не удалось использовать.
}

/// Определяет чувствительный ввод сразу после первого маркера, даже без закрывающего.
pub fn has_protected_draft(markdown: &str) -> bool {
    markdown.contains(MARKER)
}

/// Определяет зарезервированный адрес фрагмента, включая неверную или неизвестную версию.
pub fn is_fragment_link(url: &str) -> bool {
    url.trim()
        .get(..URN_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(URN_PREFIX))
}

/// Собирает пригодные к показу ссылки. Повреждение одной ссылки не скрывает остальные.
pub fn fragment_links(markdown: &str) -> Vec<FragmentLink> {
    display_links(fragment_display_spans(markdown))
}

/// Возвращает места спойлеров, включая закрытые неисправные ссылки и скрытые определения.
pub fn fragment_display_spans(markdown: &str) -> Vec<FragmentDisplaySpan> {
    let mut links = Vec::new();
    let _ = collect_links(markdown, 0, LinkScan::Reading, &mut links);
    links
}

/// Оставляет в коротких превью нейтральную подпись вместо конвертов и их определений.
pub fn fragment_preview(markdown: &str) -> Cow<'_, str> {
    let spans = fragment_display_spans(markdown);
    if spans.is_empty() {
        return Cow::Borrowed(markdown);
    }
    let mut preview = String::new();
    let mut end = 0;
    for span in spans {
        if span.range.start < end {
            continue;
        }
        preview.push_str(&markdown[end..span.range.start]);
        if span.kind != FragmentDisplayKind::Definition {
            preview.push_str("Зашифрованный фрагмент");
        }
        end = span.range.end;
    }
    preview.push_str(&markdown[end..]);
    Cow::Owned(preview)
}

/// Оставляет разрешимые ссылки для работы с конвертами.
fn display_links(spans: Vec<FragmentDisplaySpan>) -> Vec<FragmentLink> {
    spans
        .into_iter()
        .filter_map(|span| match span.kind {
            FragmentDisplayKind::Reference(reference) => Some(FragmentLink {
                range: span.range,
                reference,
            }),
            FragmentDisplayKind::Unavailable | FragmentDisplayKind::Definition => None,
        })
        .collect()
}

/// Для правки требует точные границы всех замен. Безопасный показ использует отдельный режим.
fn editable_fragment_links(markdown: &str) -> Result<Vec<FragmentLink>, FragmentError> {
    let mut links = Vec::new();
    collect_links(markdown, 0, LinkScan::Editing, &mut links)?;
    let links = display_links(links);
    if links
        .windows(2)
        .any(|pair| pair[0].range.end > pair[1].range.start)
    {
        return Err(FragmentError::InvalidFragment);
    }
    Ok(links)
}

impl MessageEncryption {
    /// Использует последнее локальное поколение или записывает новый ключ без смены режима чата.
    pub(super) fn fragment_sending_key(
        &self,
        activation: &EncryptionActivation,
        stream_id: Uuid,
        new_key: bool,
    ) -> Result<MessageDecryption, EncryptionError> {
        let _operation = self.store_operations.lock().unwrap();
        if !new_key {
            match self.available_sending_key_locked(activation, stream_id, None) {
                Err(EncryptionError::MissingKey) => {}
                result => return result,
            }
        }
        let mut bytes = Zeroizing::new([0; KEY_BYTES]);
        SystemRandom::new()
            .fill(bytes.as_mut())
            .map_err(|_| EncryptionError::Seal)?;
        let key = GroupKey::from_bytes(*bytes);
        let id = Uuid::new_v4();
        if !self
            .selection
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|selection| &selection.activation == activation)
        {
            return Err(EncryptionError::Stale);
        }
        // Очередь хранилища удерживается до записи, поэтому два первых фрагмента
        // с обычным выбором не заведут два поколения.
        self.store
            .save(&key_address(activation.session(), stream_id, id), &key)?;
        self.publish_key_change(activation, stream_id, id, true);
        self.prepare_locked(activation, stream_id, id)
            .map_err(EncryptionError::from)
    }

    /// Готовит расшифровку по исходному чату после проверки его доступности вызывающим кодом.
    /// По завершении рабочей задачи экран принимает закрытый результат методом `accept_fragment`.
    pub fn prepare_fragment(
        &self,
        activation: &EncryptionActivation,
        reference: &FragmentReference,
    ) -> Result<FragmentDecryptionResult, DecryptionError> {
        let key = self.prepare(activation, reference.stream_id, reference.key_id)?;
        Ok(FragmentDecryptionResult(
            key.decrypt(&STANDARD.encode(&reference.envelope)),
        ))
    }

    /// Открывает все ссылки для редактора. После отзыва любого исходного ключа текст недоступен.
    /// Вызывающий код проверяет происхождение сообщения и доступ к каждому исходному чату.
    pub fn restore_fragment_draft(
        &self,
        activation: &EncryptionActivation,
        markdown: &str,
    ) -> Result<Option<RestoredFragmentDraft>, FragmentError> {
        let links = editable_fragment_links(markdown)?;
        if links.is_empty() {
            return Ok(None);
        }
        if has_protected_draft(markdown) {
            return Err(FragmentError::InvalidFragment);
        }

        let mut restored = Zeroizing::new(String::with_capacity(markdown.len()));
        let mut permissions = Vec::with_capacity(links.len());
        let mut end = 0;
        for FragmentLink { range, reference } in links {
            let result = self.prepare_fragment(activation, &reference)?.0;
            let plaintext = crate::messenger::quote::project_owned(result.plaintext?).0;
            if plaintext.trim().is_empty() || has_protected_draft(&plaintext) {
                return Err(FragmentError::InvalidFragment);
            }
            restored.push_str(&markdown[end..range.start]);
            restored.push_str(MARKER);
            restored.push_str(&plaintext);
            restored.push_str(MARKER);
            permissions.push(result.access);
            end = range.end;
        }
        restored.push_str(&markdown[end..]);
        let draft = RestoredFragmentDraft {
            selection: Arc::downgrade(&self.selection),
            permissions,
            markdown: restored,
        };
        draft.with_markdown(|_| ()).ok_or(DecryptionError::Stale)?;
        Ok(Some(draft))
    }
}

/// Открытая правка в памяти, разрешение на которую связано со всеми исходными ключами.
pub struct RestoredFragmentDraft {
    selection: Weak<Mutex<Option<Selection>>>, // Актуальный выбор аккаунта без удержания сервиса.
    permissions: Vec<DecryptionAccess>,        // Проверки исходных ключей редактора.
    markdown: Zeroizing<String>,               // Текст с маркерами только для редактора.
}

impl RestoredFragmentDraft {
    /// Читает текст в коротком замыкании без вызовов шифрования или изменения ключей.
    pub fn with_markdown<T>(&self, read: impl FnOnce(&str) -> T) -> Option<T> {
        let selection = self.selection.upgrade()?;
        let selection = selection.lock().unwrap();
        self.is_current(selection.as_ref())
            .then(|| read(&self.markdown))
    }

    /// Проверяет все исходные поколения под уже взятой блокировкой выбора.
    fn is_current(&self, selection: Option<&Selection>) -> bool {
        self.permissions
            .iter()
            .all(|permission| permission.is_current(selection))
    }
}

/// Шифрует каждый диапазон со своей солью и оставляет открытые части Markdown на месте.
pub(super) fn seal_fragments(
    markdown: &str,
    key: &MessageDecryption,
) -> Result<String, FragmentError> {
    let ranges = protected_ranges(markdown)?;
    let mut sealed = String::new();
    let mut end = 0;
    for (range, plaintext) in ranges {
        let envelope = key.seal(plaintext)?;
        let bytes = STANDARD
            .decode(envelope)
            .map_err(|_| EncryptionError::Seal)?;
        let encoded = URL_SAFE_NO_PAD.encode(bytes);
        if encoded.len() > MAX_ENVELOPE_CHARACTERS {
            return Err(FragmentError::TooLong);
        }
        sealed.push_str(&markdown[end..range.start]);
        sealed.push_str(&format!(
            "[Зашифрованный фрагмент](urn:secret:v1:{}:{}:{encoded})",
            key.access.stream_id,
            key.key_id()
        ));
        end = range.end;
    }
    sealed.push_str(&markdown[end..]);
    if sealed.chars().count() > MAX_MESSAGE_CHARACTERS {
        return Err(FragmentError::TooLong);
    }
    Ok(sealed)
}

/// Разбирает парные маркеры слева направо и возвращает только занятые диапазоны.
fn protected_ranges(markdown: &str) -> Result<Vec<(Range<usize>, &str)>, FragmentError> {
    let mut markers = markdown.match_indices(MARKER);
    let mut ranges = Vec::new();
    while let Some((start, _)) = markers.next() {
        let (close, _) = markers.next().ok_or(FragmentError::InvalidFragment)?;
        let text = &markdown[start + MARKER.len()..close];
        if text.trim().is_empty() {
            return Err(FragmentError::InvalidFragment);
        }
        ranges.push((start..close + MARKER.len(), text));
    }
    Ok(ranges)
}

/// Принимает только UUID с дефисами, как в ссылках Electron.
fn parse_hyphenated_uuid(value: &str) -> Option<Uuid> {
    if value.len() != 36 {
        return None;
    }
    Uuid::parse_str(value).ok()
}

/// Режим разбора определяет, можно ли пропускать отдельные неисправные или неоднозначные ссылки.
#[derive(Clone, Copy, Eq, PartialEq)]
enum LinkScan {
    Reading, // Показ остальных фрагментов продолжается.
    Editing, // Исходник можно открыть только если все ссылки заменяются целиком.
}

/// Обходит реальные ссылки в тексте, цитатах, списках и таблицах.
fn collect_links(
    markdown: &str,
    base: usize,
    scan: LinkScan,
    links: &mut Vec<FragmentDisplaySpan>,
) -> Result<(), FragmentError> {
    if !markdown
        .as_bytes()
        .windows(URN_PREFIX.len())
        .any(|candidate| candidate.eq_ignore_ascii_case(URN_PREFIX.as_bytes()))
    {
        return Ok(());
    }
    let document =
        to_mdast(markdown, &ParseOptions::gfm()).map_err(|_| FragmentError::InvalidFragment)?;
    let definitions = link_definitions(&document);
    let mut pending = vec![&document];
    while let Some(node) = pending.pop() {
        let placement = match node {
            Node::Link(link) => push_fragment_link(markdown, base, scan, node, &link.url, links),
            Node::LinkReference(link) => match definitions.get(link.identifier.as_str()) {
                Some(url) => push_fragment_link(markdown, base, scan, node, url, links),
                None => Ok(()),
            },
            Node::Definition(definition)
                if scan == LinkScan::Reading && is_fragment_link(&definition.url) =>
            {
                push_fragment_link(markdown, base, scan, node, &definition.url, links)
            }
            Node::Code(code)
                if code.lang.as_deref().is_some_and(|lang| {
                    lang.eq_ignore_ascii_case("quote") || lang.eq_ignore_ascii_case("spoiler")
                }) =>
            {
                collect_fenced_links(markdown, base, scan, code, links)
            }
            _ => {
                pending.extend(node.children().into_iter().flatten().rev());
                Ok(())
            }
        };
        if scan == LinkScan::Editing {
            placement?;
        }
    }
    Ok(())
}

/// Учитывает первое определение Markdown для полного, сокращённого и неявного обращения.
fn link_definitions(document: &Node) -> std::collections::HashMap<&str, &str> {
    let mut definitions = std::collections::HashMap::new();
    let mut pending = vec![document];
    while let Some(node) = pending.pop() {
        if let Node::Definition(definition) = node {
            definitions
                .entry(definition.identifier.as_str())
                .or_insert(definition.url.as_str());
        }
        pending.extend(node.children().into_iter().flatten().rev());
    }
    definitions
}

/// Проверяет одну ссылку и её диапазон. При правке цель должна целиком лежать в заменяемом тексте.
fn push_fragment_link(
    markdown: &str,
    base: usize,
    scan: LinkScan,
    node: &Node,
    url: &str,
    links: &mut Vec<FragmentDisplaySpan>,
) -> Result<(), FragmentError> {
    if !is_fragment_link(url) {
        return Ok(());
    }
    let kind = if matches!(node, Node::Definition(_)) {
        FragmentDisplayKind::Definition
    } else {
        match FragmentReference::parse(url) {
            Some(reference) => FragmentDisplayKind::Reference(reference),
            None if scan == LinkScan::Reading => FragmentDisplayKind::Unavailable,
            None => return Err(FragmentError::InvalidFragment),
        }
    };
    let position = node.position().ok_or(FragmentError::InvalidFragment)?;
    let range = position.start.offset..position.end.offset;
    let raw = markdown
        .get(range.clone())
        .ok_or(FragmentError::InvalidFragment)?;
    if scan == LinkScan::Editing {
        // У ссылок с определением конверт остался бы в другой строке исходника.
        let explicit = matches!(node, Node::Link(_))
            && (raw.starts_with('[')
                || raw
                    .strip_prefix('<')
                    .and_then(|text| text.strip_suffix('>'))
                    == Some(url));
        if !explicit {
            return Err(FragmentError::InvalidFragment);
        }
    }
    links.push(FragmentDisplaySpan {
        range: base + range.start..base + range.end,
        kind,
    });
    Ok(())
}

/// Разбирает только наследуемые Markdown-блоки цитаты и спойлера.
fn collect_fenced_links(
    markdown: &str,
    base: usize,
    scan: LinkScan,
    code: &markdown::mdast::Code,
    links: &mut Vec<FragmentDisplaySpan>,
) -> Result<(), FragmentError> {
    let mut inner = Vec::new();
    collect_links(&code.value, 0, scan, &mut inner)?;
    if inner.is_empty() {
        return Ok(());
    }
    let position = code
        .position
        .as_ref()
        .ok_or(FragmentError::InvalidFragment)?;
    let start = position.start.offset;
    let raw = markdown
        .get(start..position.end.offset)
        .ok_or(FragmentError::InvalidFragment)?;
    relocate_fenced_links(raw, &code.value, base + start, inner, links)
}

/// Переносит позиции ссылки из тела наследуемой цитаты или спойлера в исходный Markdown.
fn relocate_fenced_links(
    fenced: &str,
    inner: &str,
    base: usize,
    links: Vec<FragmentDisplaySpan>,
    output: &mut Vec<FragmentDisplaySpan>,
) -> Result<(), FragmentError> {
    let (header, body) = fenced
        .split_once('\n')
        .ok_or(FragmentError::InvalidFragment)?;
    let mut source_lines = body.split_inclusive('\n');
    let mut source_start = base + header.len() + 1;
    let mut inner_start = 0;
    let mut starts = Vec::new();
    for line in inner.split('\n') {
        let source = source_lines.next().ok_or(FragmentError::InvalidFragment)?;
        let prefix = source
            .trim_end_matches(['\r', '\n'])
            .strip_suffix(line)
            .filter(|prefix| {
                prefix
                    .bytes()
                    .all(|byte| matches!(byte, b' ' | b'\t' | b'>'))
            })
            .ok_or(FragmentError::InvalidFragment)?;
        starts.push((inner_start, source_start + prefix.len()));
        source_start += source.len();
        inner_start += line.len() + 1;
    }
    let position = |offset| {
        let (inner_start, source_start) =
            starts[starts.partition_point(|(start, _)| *start <= offset) - 1];
        source_start + offset - inner_start
    };
    output.extend(links.into_iter().map(|link| FragmentDisplaySpan {
        range: position(link.range.start)..position(link.range.end),
        kind: link.kind,
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Использует короткий синтетический конверт для проверки лексики Markdown.
    const URN: &str = "urn:secret:v1:956f084e-8992-4334-a6de-fe84ac055e46:e70f0d95-6784-4125-8dfd-83eac755cecf:AQID";

    /// Отдельный неверный адрес не мешает разобрать обычную, авто- и ссылочную формы.
    #[test]
    fn fragment_read_ignores_only_malformed_link_and_keeps_real_markdown_forms() {
        let markdown = format!(
            "[ошибка](urn:secret:v2:bad) [обычная]({URN}) <{URN}> [другая][ref] [ref][] [ref]\n\n[ref]: {URN}\n\n`<{URN}>`"
        );
        let spans = fragment_links(&markdown)
            .into_iter()
            .map(|link| {
                assert_eq!(link.reference, FragmentReference::parse(URN).unwrap());
                markdown[link.range].to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            [
                format!("[обычная]({URN})"),
                format!("<{URN}>"),
                "[другая][ref]".to_owned(),
                "[ref][]".to_owned(),
                "[ref]".to_owned(),
            ]
        );
    }

    /// Правка требует, чтобы весь конверт можно было убрать вместе с исходной ссылкой.
    #[test]
    fn fragment_edit_rejects_references_that_leave_the_old_envelope_in_the_definition() {
        assert!(editable_fragment_links(&format!("<{URN}>")).is_ok());
        assert!(editable_fragment_links("[ошибка](urn:secret:v2:bad)").is_err());
        assert!(editable_fragment_links(&format!("[описание][ref]\n\n[ref]: {URN}")).is_err());
    }
}
