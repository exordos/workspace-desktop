//! Проверяет, что аватар переживает перезапуск, вовремя обновляется по журналу
//! и не возвращается в кэш после замены или сброса.

use crate::{
    context::AccountId,
    messenger::SOURCE_ID,
    storage::{ApplyOutcome, Cursor, EventWrite},
};

use super::*;

const USER: Uuid = Uuid::from_u128(1);
const FIRST: &str = "urn:image:00000000-0000-0000-0000-000000000010";
const SECOND: &str = "urn:image:00000000-0000-0000-0000-000000000011";

/// Создаёт изолированный контекст для одного и того же UUID на экране.
fn owner(account: &str) -> OwnerContext {
    OwnerContext {
        account_id: AccountId(account.into()),
        server: "https://example.invalid".into(),
        organization_id: "org".into(),
        project_id: "project".into(),
        user_id: account.into(),
    }
}

/// Описывает позицию одного журнала для записи и сравнения.
fn cursor(version: u64) -> Cursor {
    Cursor {
        epoch_generation: "epoch".into(),
        epoch_version: version,
    }
}

/// Сохраняет фото через те же операции, что и загрузчик.
fn save_photo(db: &Storage, owner: &OwnerContext, user_id: Uuid, now: i64) -> AvatarFence {
    let pending = db.avatar_snapshot(owner, user_id, now).unwrap();
    let saved = db
        .save_avatar_reference(owner, user_id, pending.fence, Some(FIRST), now)
        .unwrap()
        .unwrap();
    assert!(
        db.save_avatar_bytes(owner, user_id, saved.fence, "image/png", b"png", now)
            .unwrap()
    );
    saved.fence
}

/// Читает байты после повторного открытия SQLite, даже если UUID совпал у разных аккаунтов.
#[test]
fn reopens_cached_blob_for_same_owner_only() {
    let profile = tempfile::tempdir().unwrap();
    let path = profile.path().join("workspace.sqlite");
    let a = owner("a");
    {
        let db = Storage::open(&path).unwrap();
        save_photo(&db, &a, USER, 1);
    }

    let db = Storage::open(path).unwrap();
    let restored = db.avatar_snapshot(&a, USER, 2).unwrap();
    assert_eq!(restored.reference.as_deref(), Some(FIRST));
    let image = restored.image.unwrap();
    assert_eq!(image.format, "image/png");
    assert_eq!(image.bytes, b"png");
    let independent = db.avatar_snapshot(&owner("b"), USER, 2).unwrap();
    assert!(independent.checked_at.is_none());
    assert!(independent.image.is_none());
}

/// Фиксирует смену ссылки и курсор вместе, сохраняет фото при той же ссылке и отбрасывает повтор.
#[test]
fn avatar_events_update_reference_atomically_and_reject_late_bytes() {
    let db = Storage::in_memory().unwrap();
    let owner = owner("a");
    let first = save_photo(&db, &owner, USER, 1);
    let apply = |position, reference| {
        db.apply(
            &owner,
            SOURCE_ID,
            &cursor(position),
            EventWrite {
                avatar: Some(AvatarEvent::Snapshot {
                    user_id: USER,
                    reference,
                }),
                ..Default::default()
            },
        )
    };
    assert!(apply(1, Some(FIRST)).unwrap().avatar_changed.is_empty());
    assert!(db.avatar_snapshot(&owner, USER, 2).unwrap().image.is_some());

    db.connection
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_cursor BEFORE INSERT ON sync_cursors WHEN NEW.epoch_version = 2
             BEGIN SELECT RAISE(ABORT, 'test failure'); END;",
        )
        .unwrap();
    assert!(apply(2, Some(SECOND)).is_err());
    assert_eq!(db.cursor(&owner, SOURCE_ID).unwrap(), Some(cursor(1)));
    let rolled_back = db.avatar_snapshot(&owner, USER, 3).unwrap();
    assert_eq!(rolled_back.reference.as_deref(), Some(FIRST));
    assert!(rolled_back.image.is_some());

    db.connection
        .lock()
        .unwrap()
        .execute("DROP TRIGGER reject_cursor", [])
        .unwrap();
    assert_eq!(apply(2, Some(SECOND)).unwrap().avatar_changed, vec![USER]);
    assert_eq!(db.cursor(&owner, SOURCE_ID).unwrap(), Some(cursor(2)));
    assert!(
        !db.save_avatar_bytes(&owner, USER, first, "image/png", b"old", 4)
            .unwrap()
    );
    assert_eq!(
        apply(1, Some(FIRST)).unwrap().outcome,
        ApplyOutcome::Duplicate
    );
    let replacement = db.avatar_snapshot(&owner, USER, 4).unwrap();
    assert_eq!(replacement.reference.as_deref(), Some(SECOND));
    assert!(replacement.image.is_none());

    assert!(
        db.save_avatar_bytes(&owner, USER, replacement.fence, "image/png", b"new", 5)
            .unwrap()
    );
    assert_eq!(apply(3, None).unwrap().avatar_changed, vec![USER]);
    let removed = db.avatar_snapshot(&owner, USER, 5).unwrap();
    assert!(removed.checked_at.is_some());
    assert!(removed.reference.is_none());
    assert!(removed.image.is_none());
}

/// Удаление файла убирает все ссылки на него только у получившего событие владельца.
#[test]
fn deleted_file_clears_matching_avatars_and_rejects_late_bytes() {
    let db = Storage::in_memory().unwrap();
    let a = owner("a");
    let b = owner("b");
    let second_user = Uuid::from_u128(2);
    let other_user = Uuid::from_u128(3);
    let first = save_photo(&db, &a, USER, 1);
    save_photo(&db, &a, second_user, 1);
    save_photo(&db, &b, USER, 1);
    let other = db.avatar_snapshot(&a, other_user, 1).unwrap();
    let other = db
        .save_avatar_reference(&a, other_user, other.fence, Some(SECOND), 1)
        .unwrap()
        .unwrap();
    assert!(
        db.save_avatar_bytes(&a, other_user, other.fence, "image/png", b"other", 1)
            .unwrap()
    );

    let mut deleted = db
        .apply(
            &a,
            SOURCE_ID,
            &cursor(1),
            EventWrite {
                avatar: Some(AvatarEvent::FileDeleted {
                    file_id: Uuid::from_u128(0x10),
                }),
                ..Default::default()
            },
        )
        .unwrap();
    deleted.avatar_changed.sort();
    assert_eq!(deleted.avatar_changed, vec![USER, second_user]);
    assert_eq!(db.cursor(&a, SOURCE_ID).unwrap(), Some(cursor(1)));
    for user in [USER, second_user] {
        let removed = db.avatar_snapshot(&a, user, 2).unwrap();
        assert!(removed.reference.is_none());
        assert!(removed.checked_at.is_some());
        assert!(removed.image.is_none());
    }
    assert!(db.avatar_snapshot(&b, USER, 2).unwrap().image.is_some());
    assert!(
        db.avatar_snapshot(&a, other_user, 2)
            .unwrap()
            .image
            .is_some()
    );

    // Прежняя ссылка может встретиться в снимке профиля, но прежние байты уже не вернуть.
    db.apply(
        &a,
        SOURCE_ID,
        &cursor(2),
        EventWrite {
            avatar: Some(AvatarEvent::Snapshot {
                user_id: USER,
                reference: Some(FIRST),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        !db.save_avatar_bytes(&a, USER, first, "image/png", b"old", 3)
            .unwrap()
    );
    assert!(db.avatar_snapshot(&a, USER, 3).unwrap().image.is_none());
}

/// Неполное событие оставляет фото, но сброс или сверка отклоняют начатые ранее ответы.
#[test]
fn reconciliation_fences_already_running_requests() {
    let db = Storage::in_memory().unwrap();
    let a = owner("a");
    let b = owner("b");
    let first = save_photo(&db, &a, USER, 1);
    let other_owner = save_photo(&db, &b, USER, 1);

    let applied = db
        .apply(
            &a,
            SOURCE_ID,
            &cursor(1),
            EventWrite {
                avatar: Some(AvatarEvent::Reconcile { user_id: USER }),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(applied.avatar_changed, vec![USER]);
    let stale = db.avatar_snapshot(&a, USER, 2).unwrap();
    assert!(stale.checked_at.is_none());
    assert!(stale.image.is_some());
    assert!(!db.avatar_fence_is_current(&a, USER, first).unwrap());
    assert!(
        db.save_avatar_reference(&a, USER, first, Some(SECOND), 2)
            .unwrap()
            .is_none()
    );

    let unseen = Uuid::from_u128(2);
    let pending = db.avatar_snapshot(&a, unseen, 2).unwrap();
    db.reset_source(&a, SOURCE_ID).unwrap();
    assert!(
        db.save_avatar_reference(&a, unseen, pending.fence, Some(FIRST), 3)
            .unwrap()
            .is_none()
    );
    assert!(!db.avatar_fence_is_current(&a, USER, stale.fence).unwrap());
    assert!(db.avatar_fence_is_current(&b, USER, other_owner).unwrap());
}

/// При заполнении общего бюджета убирает самые старые байты, сохраняя ссылку для загрузки.
#[test]
fn byte_budget_can_evict_across_owners_after_a_cache_hit() {
    let db = Storage::in_memory().unwrap();
    let a = owner("a");
    let b = owner("b");
    save_photo(&db, &a, USER, 1);
    let old = save_photo(&db, &b, USER, 2);
    let recent = Uuid::from_u128(2);
    save_photo(&db, &a, recent, 3);
    assert!(db.avatar_snapshot(&a, USER, 4).unwrap().image.is_some());

    trim(&db.connection.lock().unwrap(), 6, None).unwrap();
    let evicted = db.avatar_snapshot(&b, USER, 5).unwrap();
    assert!(evicted.image.is_none());
    assert_eq!(evicted.reference.as_deref(), Some(FIRST));
    assert_eq!(evicted.fence, old);
    assert!(db.avatar_snapshot(&a, USER, 5).unwrap().image.is_some());
    assert!(db.avatar_snapshot(&a, recent, 5).unwrap().image.is_some());
}

/// Повторная сверка со свежей отметкой не переписывает строку с байтами.
#[test]
fn recent_unchanged_reference_skips_update() {
    let db = Storage::in_memory().unwrap();
    let owner = owner("a");
    let stamp = save_photo(&db, &owner, USER, 1);
    db.connection
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_update BEFORE UPDATE ON directory_avatars
                 BEGIN SELECT RAISE(ABORT, 'unexpected avatar update'); END;",
        )
        .unwrap();

    let snapshot = db
        .save_avatar_reference(&owner, USER, stamp, Some(FIRST), 2)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.fence, stamp);
    assert_eq!(snapshot.checked_at, Some(1));
    assert!(snapshot.image.is_some());
}

/// При одинаковом времени оставляет только что сохранённый файл, даже если его строка старше.
#[test]
fn budget_prefers_saved_image_when_timestamps_tie() {
    let db = Storage::in_memory().unwrap();
    let owner = owner("a");
    let second = Uuid::from_u128(2);
    save_photo(&db, &owner, USER, 1);
    save_photo(&db, &owner, second, 1);

    {
        let connection = db.connection.lock().unwrap();
        let first_row: i64 = connection
            .query_row("SELECT min(rowid) FROM directory_avatars", [], |row| {
                row.get(0)
            })
            .unwrap();
        trim(&connection, 3, Some(first_row)).unwrap();
    }
    assert!(db.avatar_snapshot(&owner, USER, 2).unwrap().image.is_some());
    assert!(
        db.avatar_snapshot(&owner, second, 2)
            .unwrap()
            .image
            .is_none()
    );
}
