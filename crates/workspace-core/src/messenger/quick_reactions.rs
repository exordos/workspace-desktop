//! Ранжирует быстрые реакции по подтверждённым добавлениям за семь дней.
//! Здесь же находятся базовый набор и каталог имён, которыми обмениваются клиент и сервер.

mod emoji;

use std::{
    cmp::Reverse,
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

pub use emoji::{canonical_reaction_name, reaction_glyph};

const BASE: [(&str, &str); 6] = [
    ("heart", "❤️"),
    ("thumbs_up", "👍"),
    ("joy", "😂"),
    ("open_mouth", "😮"),
    ("cry", "😢"),
    ("clap", "👏"),
];
const POPULAR_LIMIT: usize = 5;

/// Снимок дневных счётчиков для одного владельца. Подходит для повторного расчёта после полуночи.
#[derive(Clone, Debug, Default)]
pub struct QuickReactionUsage {
    pub(crate) days: Vec<ReactionDay>, // Счётчики по UTC-дням и серверным именам реакций.
}

/// Сколько раз пользователь успешно добавил конкретную реакцию за день.
#[derive(Clone, Debug)]
pub(crate) struct ReactionDay {
    pub(crate) day: i64,     // День UTC, считая с 1970-01-01.
    pub(crate) name: String, // Имя реакции из подтверждённой серверной записи.
    pub(crate) count: u64,   // Число успешных добавлений. Снятие реакции его не меняет.
}

impl QuickReactionUsage {
    /// Возвращает список для пузыря или меню с учётом текущего дня UTC.
    /// Для известных эмодзи возвращает символ, для остальных сохраняет серверное имя.
    pub fn ranked(&self) -> Vec<String> {
        self.ranked_at(utc_day())
    }

    /// Добавляет до пяти популярных реакций к базовым и сортирует весь набор по частоте.
    fn ranked_at(&self, today: i64) -> Vec<String> {
        let mut counts = HashMap::<_, u64>::new();
        let recent = first_day(today)..=today;
        for day in self.days.iter().filter(|day| recent.contains(&day.day)) {
            let count = counts
                .entry(canonical_reaction_name(&day.name))
                .or_default();
            *count = count.saturating_add(day.count);
        }

        let mut popular: Vec<_> = counts
            .iter()
            .filter(|(name, count)| {
                **count > 0 && !BASE.iter().any(|(base, _)| name.as_ref() == *base)
            })
            .map(|(name, count)| (name.as_ref(), *count))
            .collect();
        popular.sort_unstable_by(|(left, left_count), (right, right_count)| {
            right_count
                .cmp(left_count)
                .then_with(|| emoji::compare(left, right))
        });

        let mut ordered: Vec<_> = BASE
            .into_iter()
            .map(|(name, glyph)| (Some(glyph), name, counts.get(name).copied().unwrap_or(0)))
            .chain(
                popular
                    .into_iter()
                    .take(POPULAR_LIMIT)
                    .map(|(name, count)| (None, name, count)),
            )
            .collect();
        // При равном счёте стабильная сортировка сохраняет порядок базовых и популярных.
        ordered.sort_by_key(|(_, _, count)| Reverse(*count));
        ordered
            .into_iter()
            .map(|(base_glyph, name, _)| {
                base_glyph
                    .or_else(|| emoji::known_glyph(name))
                    .unwrap_or(name)
                    .to_owned()
            })
            .collect()
    }
}

/// Возвращает номер календарного дня UTC.
pub(crate) fn utc_day() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
        / 86_400
}

/// Выбирает первый включённый день семидневного окна.
pub(crate) fn first_day(today: i64) -> i64 {
    today - 6
}

#[cfg(test)]
mod tests;
