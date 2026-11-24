//! Разбирает обычный Markdown при принятии снимка, чтобы сдвиг бабла менял только раскладку.

use std::ops::Range;

use gpui::SharedString;
use uuid::Uuid;
use workspace_core::messenger::{
    catalog::MessagePreview,
    encryption::fragments::{
        FragmentDisplayKind, FragmentDisplaySpan, FragmentReference, fragment_display_spans,
    },
    forward::{ForwardReference, ForwardSpan, MAX_FORWARD_DEPTH, forward_spans},
    quote::{self, Block},
    references::MediaReference,
};

use crate::gpui_client::{composer::short_reply_text, preferences::Language};

/// Данные тела, которыми могут пользоваться несколько проходов layout и paint.
#[derive(Eq, PartialEq)]
pub(super) struct Body {
    pub text: SharedString,        // Исходный обычный текст, нужен для выделения.
    pub parts: Vec<BodyPart>,      // Разобранный текст и ссылки на цитаты.
    excerpt: Option<SharedString>, // Короткий текст, если в исходном сообщении он есть.
    pub caption: Option<MediaCaption>, // Подпись после извлечения карточек, если у строки есть медиа.
    pub has_fragments: bool, // Нужны спойлеры или скрытие служебного адреса из исходного текста.
}

/// Подготовка подписи к карточкам, независимая от геометрии и загрузки файла.
#[derive(Eq, PartialEq)]
pub(super) struct MediaCaption {
    pub text: SharedString, // Подпись без ссылок, представленных отдельными карточками.
    pub parts: Vec<BodyPart>, // Текст и цитаты подписи.
    pub spans: Vec<(Range<usize>, SharedString)>, // Фрагменты подписи и их исходные байтовые границы.
}

/// Подготовленная часть обычного тела. Источник цитаты проверяется по текущему окну при показе.
#[derive(Eq, PartialEq)]
pub(super) enum BodyPart {
    Text(SharedString),      // Отдельный участок текста для GPUI.
    Preview(MessagePreview), // Открытый снимок пересылки с переводимыми служебными частями.
    Fragment {
        slot: usize,                          // Порядковое место спойлера среди вложений строки.
        reference: Option<FragmentReference>, // Точная ссылка либо закрытая ошибка формата.
    },
    Forward(Option<ForwardBody>), // Снимок пересылки или закрытая ошибка формата.
    Quote {
        id: Uuid,                       // Публичный UUID исходного сообщения.
        author: SharedString,           // Подпись, если источник отсутствует.
        fragment: Option<SharedString>, // Зафиксированный фрагмент, если был выбран.
    },
}

/// Части вложенной карточки без открытого текста её зашифрованных фрагментов.
#[derive(Eq, PartialEq)]
pub(super) struct ForwardBody {
    pub id: Uuid,                     // Публичный UUID исходного сообщения для перехода.
    pub author: SharedString,         // Подпись автора на исходной ссылке.
    pub source: Option<SharedString>, // Название исходного чата, если было сохранено.
    pub parts: Vec<BodyPart>,         // Разобранный исходный Markdown без конвертов в тексте.
}

impl Body {
    /// Проверяет, что ссылка на тот же оригинал ещё находится в подтверждённом теле.
    pub(super) fn contains_forward(&self, source: Uuid) -> bool {
        forward_at(&self.parts, source)
    }

    /// Собирает тело и вырезает ссылки на самостоятельные карточки только при смене снимка.
    pub(super) fn new(text: &str, media: &[MediaReference]) -> Self {
        let caption = (!media.is_empty()).then(|| MediaCaption::new(text, media));
        let text: SharedString = text.to_owned().into();
        let (parts, has_fragments) = parse(&text, 0, &mut 0);
        let excerpt = parts.iter().find_map(|part| match part {
            BodyPart::Text(line) if !line.trim().is_empty() => Some(short_reply_text(line).into()),
            _ => None,
        });
        Self {
            text,
            parts,
            excerpt,
            caption,
            has_fragments,
        }
    }

    /// Показывает превью источника, подставляя подпись на языке открытого окна.
    pub(super) fn excerpt(&self, language: Language) -> SharedString {
        self.excerpt.clone().unwrap_or_else(|| {
            language
                .tr(
                    if self
                        .parts
                        .iter()
                        .any(|part| matches!(part, BodyPart::Forward(_)))
                    {
                        "message.forwarded"
                    } else if self.has_fragments {
                        "e2ee.secretFragment"
                    } else {
                        "composer.quote"
                    },
                )
                .into()
        })
    }

    /// Узнаёт текст без ссылок на отдельные цитаты, в том числе при наличии карточек.
    pub(super) fn plain(&self) -> bool {
        !self.has_fragments && matches!(self.parts.as_slice(), [BodyPart::Text(_)])
    }

    /// Возвращает именно тот фрагмент, который может быть смонтирован для этой строки.
    pub(super) fn fragment(&self, position: usize) -> Option<&FragmentReference> {
        let parts = self
            .caption
            .as_ref()
            .map_or(&self.parts, |caption| &caption.parts);
        fragment_at(parts, position)
    }
}

impl MediaCaption {
    /// Собирает подпись и участки точного выделения за один проход по ссылкам.
    fn new(body: &str, media: &[MediaReference]) -> Self {
        let mut text = String::new();
        let mut spans = Vec::new();
        let mut previous = 0;
        let ranges = media
            .iter()
            .map(|reference| reference.range.clone())
            .chain(std::iter::once(body.len()..body.len()));
        for range in ranges {
            let start = previous;
            previous = range.end;
            let Some(part) = body.get(start..range.start) else {
                continue;
            };
            text.push_str(part);
            let left = part.trim_start();
            let visible = left.trim_end();
            let start = start + part.len() - left.len();
            if !visible.is_empty() {
                spans.push((start..start + visible.len(), visible.to_owned().into()));
            }
        }
        let text: SharedString = text.into();
        Self {
            parts: parse(&text, 0, &mut 0).0,
            text,
            spans,
        }
    }
}

/// Сохраняет готовые отображаемые фрагменты и пересылки, один раз при чтении сообщения.
fn parse(markdown: &SharedString, depth: usize, slot: &mut usize) -> (Vec<BodyPart>, bool) {
    let fragments = fragment_display_spans(markdown);
    let forwards = forward_spans(markdown);
    let mut parts = Vec::new();
    for block in quote::blocks(markdown) {
        match block {
            Block::Text(text)
                if fragments.is_empty() && forwards.is_empty() && text == markdown.as_ref() =>
            {
                parts.push(BodyPart::Text(markdown.clone()));
            }
            Block::Text(text) => {
                // Части цитаты ссылаются на тот же Markdown, что и позиции ссылок.
                let start = text.as_ptr().addr() - markdown.as_ref().as_ptr().addr();
                parse_placements(text, start, &fragments, &forwards, &mut parts, depth, slot);
            }
            Block::Quote(reference) => parts.push(BodyPart::Quote {
                id: reference.message_id,
                author: reference.author.into(),
                fragment: reference.fragment.map(Into::into),
            }),
        }
    }
    (parts, !fragments.is_empty() || !forwards.is_empty())
}

/// Подходящая ссылка в исходном тексте. Совпадающие вложения заменяет внешний блок.
enum Placement<'a> {
    Fragment(&'a FragmentDisplaySpan), // Интервал спойлера или определения ссылки.
    Forward(&'a ForwardSpan),          // Интервал снимка пересылки.
}

/// Ставит структурные блоки по порядку, оставляя открытые куски отдельным текстом.
fn parse_placements(
    text: &str,
    start: usize,
    fragments: &[FragmentDisplaySpan],
    forwards: &[ForwardSpan],
    parts: &mut Vec<BodyPart>,
    depth: usize,
    slot: &mut usize,
) {
    let mut placements: Vec<_> = fragments
        .iter()
        .map(|span| (&span.range, Placement::Fragment(span)))
        .chain(
            forwards
                .iter()
                .map(|span| (&span.range, Placement::Forward(span))),
        )
        .filter(|(range, _)| range.start >= start && range.end <= start + text.len())
        .collect();
    placements.sort_by_key(|(range, _)| (range.start, std::cmp::Reverse(range.end)));
    let mut cursor = 0;
    for (range, placement) in placements {
        let range = (range.start - start)..(range.end - start);
        if range.start < cursor {
            continue;
        }
        if range.start > cursor {
            parts.push(BodyPart::Text(text[cursor..range.start].to_owned().into()));
        }
        match placement {
            Placement::Fragment(span) if span.kind != FragmentDisplayKind::Definition => {
                let reference = match &span.kind {
                    FragmentDisplayKind::Reference(reference) => Some(reference.clone()),
                    _ => None,
                };
                parts.push(BodyPart::Fragment {
                    slot: *slot,
                    reference,
                });
                *slot += 1;
            }
            Placement::Fragment(_) => {}
            Placement::Forward(span) => {
                let reference = span
                    .reference
                    .as_ref()
                    .filter(|_| depth < MAX_FORWARD_DEPTH)
                    .map(|reference| forward_body(reference, depth, slot));
                parts.push(BodyPart::Forward(reference));
            }
        }
        cursor = range.end;
    }
    if cursor < text.len() || (text.is_empty() && fragments.is_empty() && forwards.is_empty()) {
        parts.push(BodyPart::Text(text[cursor..].to_owned().into()));
    }
}

/// Разбирает подписанный снимок, не перенося его служебный URL в отображаемое тело.
fn forward_body(reference: &ForwardReference, depth: usize, slot: &mut usize) -> ForwardBody {
    let parts = if reference.plain {
        vec![BodyPart::Preview(MessagePreview::plain_forward(
            &reference.snapshot,
        ))]
    } else {
        parse(&reference.snapshot.clone().into(), depth + 1, slot).0
    };
    ForwardBody {
        id: reference.id,
        author: reference.author.clone().into(),
        source: reference
            .source
            .as_ref()
            .map(|source| source.label.clone().into()),
        parts,
    }
}

/// Ищет оригинал в основной карточке и во вложенных пересылках.
fn forward_at(parts: &[BodyPart], source: Uuid) -> bool {
    parts.iter().any(|part| match part {
        BodyPart::Forward(Some(reference)) => {
            reference.id == source || forward_at(&reference.parts, source)
        }
        _ => false,
    })
}

/// Находит тот же слот в основном теле или во вложенной карточке пересылки.
fn fragment_at(parts: &[BodyPart], position: usize) -> Option<&FragmentReference> {
    parts.iter().find_map(|part| match part {
        BodyPart::Fragment { slot, reference } if *slot == position => reference.as_ref(),
        BodyPart::Forward(Some(reference)) => fragment_at(&reference.parts, position),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Переводит служебный текст буквальной пересылки на языке получателя.
    #[test]
    fn plain_forward_snapshot_retains_translatable_parts() {
        let body = forward_body(
            &ForwardReference {
                id: Uuid::from_u128(1),
                author: "Автор".into(),
                snapshot: "Зашифрованный фрагмент [ключ](urn:secret:v9:bad)".into(),
                plain: true,
                source: None,
            },
            0,
            &mut 0,
        );
        let [BodyPart::Preview(preview)] = body.parts.as_slice() else {
            panic!("Буквальная пересылка должна использовать готовые части превью");
        };
        assert_eq!(
            Language::En.preview_text(preview),
            "Зашифрованный фрагмент Encrypted fragment"
        );
        assert_eq!(
            Language::Ru.preview_text(preview),
            "Зашифрованный фрагмент Зашифрованный фрагмент"
        );
    }

    /// Использует текущий язык для пустого фрагмента одного и того же сообщения.
    #[test]
    fn quote_excerpt_uses_the_render_language() {
        let body = Body::new("", &[]);
        assert_eq!(body.excerpt(Language::Ru).as_ref(), "Цитата");
        assert_eq!(body.excerpt(Language::En).as_ref(), "Quote");
    }

    /// Разбирает отображаемые фрагменты отдельно от их адресов и от закрытой ошибки.
    #[test]
    fn body_keeps_public_text_without_playing_secret_urls_as_text() {
        let link = "urn:secret:v1:956f084e-8992-4334-a6de-fe84ac055e46:e70f0d95-6784-4125-8dfd-83eac755cecf:AQID";
        let markdown =
            format!("До [неизвестный](urn:secret:v2:bad) и [готовый][x] после\n\n[x]: {link}");
        let body = Body::new(&markdown, &[]);
        assert!(body.has_fragments && !body.plain());
        assert_eq!(body.text.as_ref(), markdown);
        assert!(body.parts.iter().any(|part| matches!(
            part,
            BodyPart::Fragment {
                reference: None,
                ..
            }
        )));
        assert!(body.parts.iter().any(|part| matches!(
            part,
            BodyPart::Fragment {
                reference: Some(_),
                ..
            }
        )));
        assert!(body.parts.iter().all(|part| match part {
            BodyPart::Text(text) => !text.contains("urn:secret") && !text.contains("[x]"),
            _ => true,
        }));
    }
}
