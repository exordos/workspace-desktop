//! Разбирает защищённый Markdown в отзываемые диапазоны текста, цитаты и карточки файлов.

use std::{ops::Range, sync::Arc};

use gpui::{AnyElement, App, Div, IntoElement, div, prelude::*};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        encryption::ProtectedMessage,
        quote::{self, Block},
        references::media_references,
    },
    storage::media::MediaKind,
};

use super::{
    attachments::{AttachmentKey, Cards, ProtectedAttachment},
    reply::history_quote,
    viewport::GuardedText,
};
use crate::gpui_client::{
    preferences::Language,
    ui::{Palette, TextRole, text_role},
};

/// Структура Markdown для одного отзываемого представления. Содержит только позиции и UUID.
pub(super) struct ProtectedLayout(Vec<Part>); // Отображаемые блоки в порядке исходного текста.

/// Один блок защищённого Markdown.
enum Part {
    Text(Range<usize>), // Диапазон тела, которое перечитывается непосредственно при layout.
    Quote(Uuid),        // Публичный UUID исходного сообщения, которое проверит Core.
    File {
        id: Uuid,        // Публичный ID вложения.
        kind: MediaKind, // Общий тип карточки файла.
        position: usize, // Место ссылки внутри исходного сообщения.
    },
}

impl ProtectedLayout {
    /// Берёт те же структурные Markdown блоки, которые используют обычные сообщения.
    pub(super) fn parse(text: &str) -> Self {
        let media = media_references(text);
        let mut parts = Vec::new();
        for block in quote::blocks(text) {
            match block {
                Block::Quote(quote) => parts.push(Part::Quote(quote.message_id)),
                Block::Text(block) => {
                    let start = block.as_ptr().addr() - text.as_ptr().addr();
                    let end = start + block.len();
                    let mut cursor = start;
                    for (position, file) in media
                        .iter()
                        .enumerate()
                        .filter(|(_, file)| file.range.start >= start && file.range.end <= end)
                    {
                        append_text(text, cursor..file.range.start, &mut parts);
                        parts.push(Part::File {
                            id: file.file_id,
                            kind: file.kind,
                            position,
                        });
                        cursor = file.range.end;
                    }
                    append_text(text, cursor..end, &mut parts);
                }
            }
        }
        Self(parts)
    }

    /// Проверяет повторный клик по той же ссылке, сохранившейся в текущем теле.
    pub(super) fn has_file(&self, key: AttachmentKey) -> bool {
        self.0.iter().any(|part| matches!(
            part, Part::File { id, position, .. } if *id == key.file && *position == key.position
        ))
    }

    /// Возвращает указатели цитат, не удерживая подписи и фрагменты из оригинального текста.
    pub(super) fn quotes(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.0.iter().filter_map(|part| match part {
            Part::Quote(id) => Some(*id),
            _ => None,
        })
    }

    /// Собирает те же роли текста, цитат и карточек, оставляя исходные строки в Core.
    pub(super) fn render(
        &self,
        id: Uuid,
        access: &Arc<ProtectedMessage>,
        palette: Palette,
        own: bool,
        media: Option<(&Cards, &mut App, Language)>,
        quote: impl Fn(usize, Palette, bool, Option<(&Cards, &mut App, Language)>) -> AnyElement,
    ) -> Div {
        let mut media = media;
        let mut content = div().flex().flex_col().min_w_0().max_w_full().gap_1();
        let mut quote_position = 0;
        for part in &self.0 {
            let item = match part {
                Part::Text(range) => text_role(palette, TextRole::Body)
                    .min_w_0()
                    .max_w_full()
                    .child(GuardedText::range(access.clone(), range.clone()))
                    .into_any_element(),
                Part::Quote(_) => {
                    let media = media
                        .as_mut()
                        .map(|(cards, app, language)| (*cards, &mut **app, *language));
                    let item = quote(quote_position, palette, own, media);
                    quote_position += 1;
                    item
                }
                Part::File {
                    id: file,
                    kind,
                    position,
                } => {
                    let Some((cards, app, language)) = media.as_mut() else {
                        continue;
                    };
                    let key = AttachmentKey {
                        message: id,
                        file: *file,
                        position: *position,
                    };
                    cards
                        .render_protected(
                            ProtectedAttachment::direct(key, access.clone()),
                            *kind,
                            palette,
                            *language,
                            app,
                        )
                        .into_any_element()
                }
            };
            content = content.child(item);
        }
        content
    }
}

/// Пропускает пустую подпись рядом с карточкой. Отступы остаются теми же, что у обычного Markdown.
fn append_text(text: &str, range: Range<usize>, parts: &mut Vec<Part>) {
    if text
        .get(range.clone())
        .is_some_and(|text| !text.trim().is_empty())
    {
        parts.push(Part::Text(range));
    }
}

/// Показывает ожидающую цитату через ту же рамку, что и обычный оригинал.
pub(super) fn unavailable_quote(palette: Palette, own: bool, language: Language) -> AnyElement {
    history_quote(
        palette,
        own,
        language.tr("composer.quote").into(),
        "".into(),
        false,
        language,
    )
    .into_any_element()
}
