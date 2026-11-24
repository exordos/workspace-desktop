//! Сопоставляет эмодзи, серверные имена и их псевдонимы по каталогу Workspace.

use std::{borrow::Cow, cmp::Ordering, collections::HashMap, sync::LazyLock};

/// Одна запись каталога, общая для символа и всех его имён.
#[derive(Clone, Copy)]
struct Emoji {
    name: &'static str,  // Первое имя из каталога, которое сохраняет сервер.
    glyph: &'static str, // Символ с признаком показа в виде эмодзи, если это нужно.
    order: usize,        // Положение символа при сравнении реакций с равными счётчиками.
}

static CATALOG: LazyLock<HashMap<Cow<'static, str>, Emoji>> = LazyLock::new(|| {
    let mut catalog = HashMap::new();
    for (order, line) in include_str!("emoji.tsv").lines().enumerate() {
        let Some((glyph, names)) = line.split_once('\t') else {
            continue;
        };
        let mut names = names.split('\t');
        let Some(name) = names.next() else {
            continue;
        };
        let emoji = Emoji { name, glyph, order };
        catalog.insert(Cow::Borrowed(glyph), emoji);
        if glyph.contains('\u{fe0f}') {
            catalog.insert(Cow::Owned(glyph.replace('\u{fe0f}', "")), emoji);
        }
        catalog.extend(
            std::iter::once(name)
                .chain(names)
                .map(|name| (Cow::Borrowed(name), emoji)),
        );
    }
    catalog
});

/// Возвращает имя, которое сервер сохраняет для известного эмодзи или псевдонима.
/// Неизвестная реакция сохраняет свой ключ, чтобы её можно было снять.
pub fn canonical_reaction_name(value: &str) -> Cow<'_, str> {
    let value = value.trim();
    Cow::Borrowed(lookup(value).map_or(value, |emoji| emoji.name))
}

/// Возвращает эмодзи для показа. Для неизвестных имён использует форму `:имя:`.
pub fn reaction_glyph(value: &str) -> Cow<'_, str> {
    if let Some(glyph) = known_glyph(value) {
        return Cow::Borrowed(glyph);
    }
    if value.is_ascii()
        && let Some(shortcode) = normalized_shortcode(value)
    {
        return Cow::Owned(format!(":{shortcode}:"));
    }
    Cow::Borrowed(value)
}

/// Извлекает отображаемый символ, когда реакция есть в каталоге Workspace.
pub(super) fn known_glyph(value: &str) -> Option<&'static str> {
    lookup(value).map(|emoji| emoji.glyph)
}

/// Сравнивает отображаемые эмодзи в порядке каталога. Неизвестные ключи идут по имени.
pub(super) fn compare(left: &str, right: &str) -> Ordering {
    match (lookup(left), lookup(right)) {
        (Some(left), Some(right)) => left.order.cmp(&right.order),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

/// Ищет сначала точное имя или символ, затем форму без различий в написании.
fn lookup(value: &str) -> Option<Emoji> {
    if let Some(emoji) = CATALOG.get(value) {
        return Some(*emoji);
    }
    if value.contains('\u{fe0f}') {
        return CATALOG.get(value.replace('\u{fe0f}', "").as_str()).copied();
    }
    normalized_shortcode(value).and_then(|name| CATALOG.get(name.as_str()).copied())
}

/// Повторяет формат имён Workspace, например ` :Thumbs-Up: ` становится `thumbs_up`.
fn normalized_shortcode(value: &str) -> Option<String> {
    let value = value.trim().trim_matches(':');
    let mut normalized = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character == '_' || character == '-' || character.is_whitespace() {
            separator = !normalized.is_empty();
        } else {
            if separator {
                normalized.push('_');
                separator = false;
            }
            normalized.extend(character.to_lowercase());
        }
    }
    (!normalized.is_empty()).then_some(normalized)
}
