//! Собирает короткое превью из пользовательского текста и переводимых служебных вставок.

use zeroize::Zeroize;

use crate::messenger::{
    encryption::fragments::{FragmentDisplayKind, fragment_display_spans},
    forward::forward_spans,
};

/// Часть превью без адресов и содержимого защищённых фрагментов.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreviewPart {
    Text(String),      // Текст автора или видимая подпись ссылки.
    Link,              // Адрес или ссылка без собственной подписи.
    ProtectedFragment, // Закрытый фрагмент сообщения.
    Forwarded,         // Карточка пересланного сообщения.
}

/// Текст превью, в котором интерфейс может перевести только служебные вставки.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MessagePreview(Vec<PreviewPart>); // Части в порядке исходного текста.

impl MessagePreview {
    /// Предоставляет части превью для оформления выбранным интерфейсом.
    pub fn parts(&self) -> &[PreviewPart] {
        &self.0
    }

    /// Проверяет, осталась ли хотя бы одна видимая часть.
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Сохраняет буквальный снимок пересылки, заменяя только найденные служебные ссылки.
    pub fn plain_forward(markdown: &str) -> Self {
        let fragments = fragment_display_spans(markdown).into_iter().map(|span| {
            let part = (span.kind != FragmentDisplayKind::Definition)
                .then_some(PreviewPart::ProtectedFragment);
            (span.range, part)
        });
        let forwards = forward_spans(markdown)
            .into_iter()
            .map(|span| (span.range, Some(PreviewPart::Forwarded)));
        let mut spans: Vec<_> = fragments.chain(forwards).collect();
        // Если диапазоны вложены, внешняя служебная ссылка закрывает их целиком.
        spans.sort_by_key(|(range, _)| (range.start, std::cmp::Reverse(range.end)));
        let mut end = 0;
        let mut result = Self::default();
        for (range, part) in spans {
            if range.start < end {
                continue;
            }
            result.push_text(&markdown[end..range.start]);
            result.0.extend(part);
            end = range.end;
        }
        result.push_text(&markdown[end..]);
        result
    }

    /// Убирает адреса и лишние пробелы, считая каждую служебную вставку одной частью лимита.
    pub(in crate::messenger) fn short(markdown: &str, limit: usize) -> Self {
        let source = zeroize::Zeroizing::new(Self::plain_forward(markdown));
        let mut projected = zeroize::Zeroizing::new(Self::default());
        for part in source.parts() {
            match part {
                PreviewPart::Text(text) => projected.append_link_labels(text),
                part => projected.0.push(part.clone()),
            }
        }
        let mut result = projected.normalized();
        result.truncate(limit);
        result
    }

    /// Склеивает обычные подписи ссылок с соседним текстом и скрывает назначения.
    fn append_link_labels(&mut self, mut text: &str) {
        while let Some((before, destination)) = text.split_once("](") {
            let Some((prefix, label)) = before.rsplit_once('[') else {
                self.push_text(before);
                self.push_text("](");
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
            self.push_text(prefix.strip_suffix('!').unwrap_or(prefix));
            if label.is_empty() {
                self.0.push(PreviewPart::Link);
            } else {
                self.push_text(label);
            }
            text = &destination[close + 1..];
        }
        self.push_text(text);
    }

    /// Сжимает пробелы, убирает оформление и заменяет веб-адреса, включая видимые подписи ссылок.
    fn normalized(&self) -> Self {
        let mut result = Self::default();
        let mut gap = false;
        for part in self.parts() {
            match part {
                PreviewPart::Text(text) => {
                    for chunk in text.split_inclusive(char::is_whitespace) {
                        let plain = chunk.trim_end_matches(char::is_whitespace);
                        let word = plain.trim_matches(['*', '`', '~']);
                        let address = word.trim_matches(['<', '>']);
                        if !word.is_empty() {
                            result.push_gap(&mut gap);
                            if address.starts_with("https://") || address.starts_with("http://") {
                                result.0.push(PreviewPart::Link);
                            } else if word.chars().any(char::is_control) {
                                let visible: String =
                                    word.chars().filter(|ch| !ch.is_control()).collect();
                                result.push_text(&visible);
                            } else {
                                result.push_text(word);
                            }
                        }
                        gap |= plain.len() != chunk.len();
                    }
                }
                part => {
                    result.push_gap(&mut gap);
                    result.0.push(part.clone());
                }
            }
        }
        result
    }

    /// Выводит один пробел, только когда он был между видимыми частями.
    fn push_gap(&mut self, gap: &mut bool) {
        if *gap && !self.is_empty() {
            self.push_text(" ");
        }
        *gap = false;
    }

    /// Склеивает последовательные части авторского текста.
    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.0.last_mut() {
            Some(PreviewPart::Text(previous)) => previous.push_str(text),
            _ => self.0.push(PreviewPart::Text(text.to_owned())),
        }
    }

    /// Ограничивает число символов, сохраняя Unicode и целые служебные части.
    fn truncate(&mut self, mut remaining: usize) {
        let mut kept = 0;
        for part in &mut self.0 {
            if remaining == 0 {
                break;
            }
            if let PreviewPart::Text(text) = part {
                if let Some((end, _)) = text.char_indices().nth(remaining) {
                    text.truncate(end);
                }
                remaining -= text.chars().count();
            } else {
                remaining -= 1;
            }
            kept += 1;
        }
        // Удалённая часть защищённого превью не остаётся в освобождаемой строке.
        for part in &mut self.0[kept..] {
            if let PreviewPart::Text(text) = part {
                text.zeroize();
            }
        }
        self.0.truncate(kept);
        if let Some(PreviewPart::Text(text)) = self.0.last_mut() {
            text.truncate(text.trim_end().len());
            if text.is_empty() {
                self.0.pop();
            }
        }
    }
}

impl From<&str> for MessagePreview {
    /// Превращает уже подготовленный обычный текст в одну часть.
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

impl From<String> for MessagePreview {
    /// Переносит уже подготовленный обычный текст без повторного разбора.
    fn from(text: String) -> Self {
        if text.is_empty() {
            Self::default()
        } else {
            Self(vec![PreviewPart::Text(text)])
        }
    }
}

impl Zeroize for MessagePreview {
    /// Стирает авторский текст, когда закрывается отзывное зашифрованное превью.
    fn zeroize(&mut self) {
        for part in &mut self.0 {
            if let PreviewPart::Text(text) = part {
                text.zeroize();
            }
        }
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Отличает авторские подписи от служебных маркеров и убирает адреса.
    #[test]
    fn short_preview_keeps_author_text_and_redacts_targets() {
        let markdown = "Ссылка [любая подпись](https://private.example) https://blocked.example \
            [скрыто](urn:secret:v2:bad) [пересылка](urn:forward:bad)";
        let preview = MessagePreview::short(markdown, 240);
        assert_eq!(
            preview.parts(),
            &[
                PreviewPart::Text("Ссылка любая подпись ".into()),
                PreviewPart::Link,
                PreviewPart::Text(" ".into()),
                PreviewPart::ProtectedFragment,
                PreviewPart::Text(" ".into()),
                PreviewPart::Forwarded,
            ]
        );
    }

    /// Сохраняет соседство авторского текста и скрывает URL, показанный как подпись ссылки.
    #[test]
    fn short_preview_preserves_adjacent_labels_and_filters_visible_urls() {
        let markdown = "до[подпись](https://destination.example)после Ссылка \
            [https://private.example/path](https://destination.example)";
        let preview = MessagePreview::short(markdown, 240);
        assert_eq!(
            preview.parts(),
            &[
                PreviewPart::Text("доподписьпосле Ссылка ".into()),
                PreviewPart::Link,
            ]
        );
    }

    /// Не меняет обычный пользовательский текст и пробелы в плоской пересылке.
    #[test]
    fn plain_forward_only_replaces_service_links() {
        let preview = MessagePreview::plain_forward(
            "Ссылка\n[автор](urn:forward:bad)  [подпись](https://example.test)",
        );
        assert_eq!(
            preview.parts(),
            &[
                PreviewPart::Text("Ссылка\n".into()),
                PreviewPart::Forwarded,
                PreviewPart::Text("  [подпись](https://example.test)".into()),
            ]
        );
    }
}
