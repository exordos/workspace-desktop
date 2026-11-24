//! Загружает встраиваемые каталоги, выбирает перевод по языку аккаунта и подставляет значения.

use std::{
    borrow::Cow,
    collections::HashSet,
    iter,
    sync::{LazyLock, Mutex},
};

use icu_locale_core::Locale;
use icu_plurals::{PluralCategory, PluralRules};
use serde::{Deserialize, Deserializer, Serialize};

rust_i18n::i18n!("locales");

static LANGUAGE_CODES: LazyLock<Vec<Cow<'static, str>>> = LazyLock::new(|| {
    let mut codes = _rust_i18n_backend().available_locales();
    codes.sort_unstable_by(|left, right| {
        (left.as_ref() != Language::Ru.code(), left)
            .cmp(&(right.as_ref() != Language::Ru.code(), right))
    });
    let mut seen = HashSet::new();
    codes.retain(|code| seen.insert(code.to_ascii_lowercase()));
    codes
});
static MISSING_KEYS: LazyLock<Mutex<HashSet<(&'static str, &'static str)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Код языка из встраиваемого каталога. В профиле сохраняется обычной строкой.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(super) struct Language(&'static str); // Код локали BCP 47, например ru или pt-BR.

impl Language {
    /// Язык по умолчанию и запасной каталог для неполных переводов.
    #[allow(non_upper_case_globals)]
    pub const Ru: Self = Self("ru");
    /// Английский каталог, который уже можно выбрать в сохранённых настройках.
    #[allow(non_upper_case_globals)]
    pub const En: Self = Self("en");

    /// Возвращает подпись из старой пары строк для ещё не перенесённых компонентов.
    pub fn text(self, ru: &'static str, en: &'static str) -> &'static str {
        if self == Self::En { en } else { ru }
    }

    /// Перечисляет языки, которые упакованы в текущую сборку Desktop.
    pub fn available() -> impl Iterator<Item = Self> {
        LANGUAGE_CODES.iter().map(|code| Self(code.as_ref()))
    }

    /// Возвращает код для сохранения, выбора языка и работы с каталогом.
    pub fn code(self) -> &'static str {
        self.0
    }

    /// Показывает собственное название языка, используя его код как запасной вариант.
    pub fn name(self) -> &'static str {
        lookup(self.code(), "locale.name").unwrap_or(self.code())
    }

    /// Возвращает строку без подстановок, используя русский каталог, если перевода ещё нет.
    pub fn tr(self, key: &'static str) -> &'static str {
        self.fallbacks()
            .find_map(|locale| lookup(locale, key))
            .unwrap_or_else(|| self.missing(key))
    }

    /// Подставляет значения в маркеры вида %{name} после выбора перевода.
    pub fn tr_with(self, key: &'static str, values: &[(&str, &str)]) -> String {
        interpolate(self.tr(key), values.iter().copied())
    }

    /// Выбирает числовую форму каждого проверяемого языка и подставляет количество.
    pub fn plural(self, key: &'static str, count: u64) -> String {
        self.plural_with(key, count, &[])
    }

    /// Подставляет количество и дополнительные значения, например имя автора уведомления.
    pub fn plural_with(self, key: &'static str, count: u64, values: &[(&str, &str)]) -> String {
        let template =
            plural_template(self, key, count, lookup).unwrap_or_else(|| self.missing(key));
        let count = count.to_string();
        interpolate(
            template,
            iter::once(("count", count.as_str())).chain(values.iter().copied()),
        )
    }

    /// Проверяет региональную локаль, общий язык и русский каталог в таком порядке.
    fn fallbacks(self) -> impl Iterator<Item = &'static str> {
        iter::successors(Some(self.code()), |code| {
            code.rsplit_once('-').map(|(parent, _)| parent)
        })
        .chain(iter::once(Self::Ru.code()))
    }

    /// Сопоставляет код без учёта регистра и сохраняет написание из каталога.
    fn for_code(code: &str) -> Option<Self> {
        Self::available().find(|lang| lang.code().eq_ignore_ascii_case(code))
    }

    /// Показывает нейтральную подпись и один раз пишет в журнал, какой ключ потерялся.
    fn missing(self, key: &'static str) -> &'static str {
        if MISSING_KEYS
            .lock()
            .is_ok_and(|mut seen| seen.insert((self.code(), key)))
        {
            log::warn!(
                "Локализация: нет перевода для языка {} и ключа {key}",
                self.code()
            );
        }
        "—"
    }
}

impl Default for Language {
    /// Выбирает русский для нового профиля и недоступного сохранённого языка.
    fn default() -> Self {
        Self::Ru
    }
}

impl<'de> Deserialize<'de> for Language {
    /// Читает прежние и новые коды, сохраняя остальные настройки при неизвестном языке.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let code = String::deserialize(deserializer)?;
        Ok(Self::for_code(&code).unwrap_or_default())
    }
}

/// Читает только указанный каталог, чтобы при смене языка пересчитать числовую форму.
fn lookup(locale: &str, key: &str) -> Option<&'static str> {
    let backend = _rust_i18n_backend();
    let value = backend.translate(locale, key).or_else(|| {
        let canonical = Language::for_code(locale)?.code();
        (canonical != locale)
            .then(|| backend.translate(canonical, key))
            .flatten()
    })?;
    // Встраиваемый backend хранит все значения как ссылки на строки внутри бинарника.
    match value {
        Cow::Borrowed(value) => Some(value),
        Cow::Owned(_) => unreachable!("embedded translation is borrowed"),
    }
}

/// Ищет форму для числа, затем универсальную строку этого же языка и только потом другой каталог.
fn plural_template(
    language: Language,
    key: &'static str,
    count: u64,
    lookup: impl Fn(&str, &str) -> Option<&'static str>,
) -> Option<&'static str> {
    let other_key = format!("{key}_other");
    language.fallbacks().find_map(|locale| {
        let category = plural_category(locale, count);
        if category == "other" {
            return lookup(locale, &other_key);
        }
        lookup(locale, &format!("{key}_{category}")).or_else(|| lookup(locale, &other_key))
    })
}

/// Подставляет именованные значения, не меняя текст самих аргументов.
fn interpolate<'a>(template: &str, values: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let (patterns, replacements): (Vec<_>, Vec<_>) =
        values.map(|(name, value)| (name, value.to_owned())).unzip();
    rust_i18n::replace_patterns(template, &patterns, &replacements)
}

/// Возвращает суффикс ключа для целого неотрицательного числа по правилам CLDR.
fn plural_category(locale: &str, count: u64) -> &'static str {
    let category = locale
        .parse::<Locale>()
        .ok()
        .and_then(|locale| PluralRules::try_new_cardinal(locale.into()).ok())
        .map_or(PluralCategory::Other, |rules| rules.category_for(count));
    match category {
        PluralCategory::Zero => "zero",
        PluralCategory::One => "one",
        PluralCategory::Two => "two",
        PluralCategory::Few => "few",
        PluralCategory::Many => "many",
        PluralCategory::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет, что коды встраиваемых файлов можно сохранять и читать в профиле.
    #[test]
    fn all_catalog_languages_round_trip() {
        let languages: Vec<_> = Language::available().collect();
        assert!(languages.contains(&Language::Ru));
        assert!(languages.contains(&Language::En));
        for language in languages {
            let json = serde_json::to_string(&language).unwrap();
            assert_eq!(json, format!("\"{}\"", language.code()));
            assert_eq!(serde_json::from_str::<Language>(&json).unwrap(), language);
            let uppercase = format!("\"{}\"", language.code().to_ascii_uppercase());
            assert_eq!(
                serde_json::from_str::<Language>(&uppercase).unwrap(),
                language
            );
            assert_ne!(language.name(), language.code());
        }
    }

    /// Проверяет, что при отсутствии перевода форма пересчитывается для русского текста.
    #[test]
    fn plural_fallback_recalculates_category() {
        let language = Language("ja-x-missing");
        assert_eq!(plural_category(language.code(), 3), "other");
        assert_eq!(language.plural("channel.participants", 3), "3 участника");
    }

    /// Предпочитает универсальную строку выбранного языка перед переводом другого языка.
    #[test]
    fn keeps_current_language_if_it_has_a_generic_plural() {
        let text = plural_template(Language::En, "counter", 1, |locale, key| {
            match (locale, key) {
                ("en", "counter_other") => Some("Messages: %{count}"),
                ("ru", "counter_one") => Some("%{count} сообщение"),
                _ => None,
            }
        });
        assert_eq!(text, Some("Messages: %{count}"));
    }

    /// Проверяет, что имена подставляются буквально даже при совпадении с маркером перевода.
    #[test]
    fn interpolates_values_without_reinterpreting_their_text() {
        let template = "%{count} сообщения от %{sender}";
        let values = [("count", "3"), ("sender", "%{count}")];
        assert_eq!(
            interpolate(template, values.into_iter()),
            "3 сообщения от %{count}"
        );
    }

    /// При отсутствии ключа показывает один нейтральный текст для всех способов перевода.
    #[test]
    fn missing_translation_has_neutral_label() {
        let language = Language::En;
        let key = "test.missingTranslation";
        assert_eq!(language.tr(key), "—");
        assert_eq!(language.tr_with(key, &[("name", "Workspace")]), "—");
        assert_eq!(language.plural(key, 3), "—");
    }
}
