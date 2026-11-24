//! Проверяет набор по умолчанию, границы окна, псевдонимы и сохранение популярности.

use super::{BASE, QuickReactionUsage, ReactionDay, canonical_reaction_name, reaction_glyph};
use crate::{
    context::{AccountId, OwnerContext},
    storage::Storage,
};

/// Создаёт строку дневного счётчика для фиксированного дня UTC.
fn row(day: i64, name: &str, count: u64) -> ReactionDay {
    ReactionDay {
        day,
        name: name.into(),
        count,
    }
}

/// Без истории предлагает шесть реакций в исходном порядке.
#[test]
fn default_order_matches_electron() {
    assert_eq!(
        QuickReactionUsage::default().ranked_at(100),
        BASE.map(|(_, glyph)| glyph)
    );
}

/// Сводит глифы и псевдонимы, исключает дни вне окна и сохраняет порядок при равенстве.
#[test]
fn ranks_base_and_five_popular_reactions_over_seven_utc_days() {
    let usage = QuickReactionUsage {
        days: vec![
            row(93, "⛄", 100),
            row(101, "💥", 100),
            row(94, "👍", 4),
            row(100, "+1", 3),
            row(100, "red_heart", 3),
            row(100, "❤️", 1),
            row(100, "🎉", 4),
            row(100, "rocket", 2),
            row(99, "🚀", 4),
            row(100, "fire", 3),
            row(100, "✨", 3),
            row(100, "💯", 3),
            row(100, "💪", 1),
        ],
    };
    assert_eq!(
        usage.ranked_at(100),
        [
            "👍", "🚀", "❤️", "🎉", "✨", "💯", "🔥", "😂", "😮", "😢", "👏"
        ]
    );
    assert_eq!(usage.ranked_at(101)[0], "💥");
    assert_eq!(usage.ranked_at(101)[1], "🚀");
    assert_eq!(usage.ranked_at(108), BASE.map(|(_, glyph)| glyph));
}

/// Отображает известные имена как эмодзи и сохраняет неизвестный ключ для команды.
#[test]
fn resolves_the_workspace_catalog_and_keeps_raw_unknown_reactions() {
    assert_eq!(canonical_reaction_name(" :Thumbs-Up: "), "thumbs_up");
    assert_eq!(canonical_reaction_name("❤"), "heart");
    assert_eq!(canonical_reaction_name("❤️"), "heart");
    assert_eq!(canonical_reaction_name("🔥"), "fire");
    assert_eq!(reaction_glyph("heart"), "❤️");
    assert_eq!(reaction_glyph("thumbs_up"), "👍");
    assert_eq!(reaction_glyph(" :hot-pepper: "), "🌶️");
    assert_eq!(
        canonical_reaction_name("workspace_custom"),
        "workspace_custom"
    );
    assert_eq!(reaction_glyph("workspace_custom"), ":workspace_custom:");
}

/// Считает повторные добавления, разделяет проекты и сохраняет счётчики после перезапуска.
#[test]
fn usage_is_persistent_and_project_scoped() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("workspace.sqlite");
    let owner = OwnerContext {
        account_id: AccountId("account".into()),
        server: "https://workspace.test".into(),
        organization_id: "org".into(),
        project_id: "first".into(),
        user_id: "user".into(),
    };
    let second_project = OwnerContext {
        project_id: "second".into(),
        ..owner.clone()
    };
    let store = Storage::open(&path).unwrap();
    store
        .record_quick_reaction(&owner, 7, 13, "rocket")
        .unwrap();
    store.record_quick_reaction(&owner, 9, 15, "heart").unwrap();
    store.record_quick_reaction(&owner, 9, 15, "heart").unwrap();
    store
        .record_quick_reaction(&second_project, 9, 15, "fire")
        .unwrap();
    store.record_quick_reaction(&owner, 14, 20, "joy").unwrap();
    store.clear_rebuildable_messenger_cache().unwrap();
    drop(store);

    let store = Storage::open(path).unwrap();
    let first = store.quick_reaction_usage(&owner, 0).unwrap();
    assert_eq!(first.days.len(), 2);
    assert_eq!(
        first
            .days
            .iter()
            .find(|row| row.name == "heart")
            .unwrap()
            .count,
        2
    );
    assert_eq!(first.ranked_at(20), ["❤️", "😂", "👍", "😮", "😢", "👏"]);
    assert_eq!(
        store
            .quick_reaction_usage(&second_project, 0)
            .unwrap()
            .ranked_at(20)[0],
        "🔥"
    );
}
