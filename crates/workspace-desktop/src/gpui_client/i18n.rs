//! Читает встраиваемые подписи мессенджера на языке выбранного аккаунта.

use std::borrow::Cow;

use super::preferences::Language;

rust_i18n::i18n!("locales");

impl Language {
    /// Возвращает перевод на выбранном языке либо запасную русскую строку.
    pub(super) fn tr(self, key: &'static str) -> &'static str {
        let backend = _rust_i18n_backend();
        match backend
            .translate(self.text("ru", "en"), key)
            .or_else(|| backend.translate("ru", key))
        {
            Some(Cow::Borrowed(value)) => value,
            _ => key,
        }
    }

    /// Подставляет данные в переведённый шаблон с именованными полями.
    pub(super) fn tr_with(self, key: &'static str, values: &[(&str, &str)]) -> String {
        let (patterns, replacements): (Vec<_>, Vec<_>) = values
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned()))
            .unzip();
        rust_i18n::replace_patterns(self.tr(key), &patterns, &replacements)
    }
}
