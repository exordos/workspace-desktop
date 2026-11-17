//! Проверяет сохранение снимков папок, долговечность сверки и защиту от поздних ответов.

use super::*;
use crate::{
    context::AccountId,
    messenger::{
        SOURCE_ID,
        folders::{ChatType, FolderItem, FolderKind},
    },
    storage::{ApplyOutcome, Cursor, EventWrite, HistoryChange},
};

const FOLDER: Uuid = Uuid::nil();
const ITEM: Uuid = Uuid::from_u128(1);
const STREAM: Uuid = Uuid::from_u128(2);
const OTHER_FOLDER: Uuid = Uuid::from_u128(3);
const TIME: &str = "2026-11-17T11:00:00Z";

/// Делает владельца в общем проекте с отдельными аккаунтом и пользователем.
fn owner(name: &str) -> OwnerContext {
    OwnerContext {
        account_id: AccountId(name.into()),
        server: "https://workspace.test".into(),
        organization_id: "organization".into(),
        project_id: "project".into(),
        user_id: name.into(),
    }
}

/// Создаёт снимок системной папки с приватной группой.
fn folder(count: u32) -> Folder {
    Folder {
        id: FOLDER,
        kind: FolderKind::All,
        title: "Все чаты".into(),
        background_color_value: None,
        unread_count: count,
        created_at: TIME.into(),
        updated_at: TIME.into(),
        items: vec![FolderItem {
            id: ITEM,
            stream_id: STREAM,
            chat_type: ChatType::Group,
            pinned_at: Some(TIME.into()),
            order_index: Some(2),
            unread_count: count,
            active_unread_count: count,
            passive_unread_count: 0,
            created_at: TIME.into(),
            updated_at: TIME.into(),
        }],
    }
}

/// Читает обязательный запрос на сверку.
fn plan(storage: &Storage, owner: &OwnerContext) -> FolderRefreshPlan {
    storage.folder_refresh_plan(owner).unwrap().unwrap()
}

/// Собирает версию журнала для одного события.
fn cursor(version: u64) -> Cursor {
    Cursor {
        epoch_generation: "messenger".into(),
        epoch_version: version,
    }
}

/// Записывает изменение папок вместе с тестовой версией журнала.
fn apply(
    storage: &Storage,
    owner: &OwnerContext,
    version: u64,
    change: FolderChange,
) -> ApplyOutcome {
    storage
        .apply(
            owner,
            SOURCE_ID,
            &cursor(version),
            EventWrite {
                folder: Some(change),
                ..Default::default()
            },
        )
        .unwrap()
        .outcome
}

/// Записывает каталог известного разговора на текущей ревизии.
fn save_catalog(storage: &Storage, owner: &OwnerContext) {
    let payload = serde_json::to_string(&[Chat {
        id: STREAM,
        name: "Команда".into(),
        direct: false,
        topics: vec![],
    }])
    .unwrap();
    let revision = storage.catalog_revision(owner).unwrap();
    storage
        .save_chat_catalog(owner, &payload, revision)
        .unwrap();
}

/// Одинаковые UUID двух входов изолированы, а сохранённая отметка переживает закрытие базы.
#[test]
fn owner_snapshots_and_pending_refresh_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("workspace.sqlite");
    let storage = Storage::open(&path).unwrap();
    let alice = owner("alice");
    let bob = owner("bob");
    assert!(storage.folder_view(&alice).unwrap().folders.is_none());
    save_catalog(&storage, &alice);
    assert!(
        storage
            .save_folder_refresh(&alice, &plan(&storage, &alice), vec![folder(5)])
            .unwrap()
    );
    assert!(
        storage
            .save_folder_refresh(&bob, &plan(&storage, &bob), vec![folder(9)])
            .unwrap()
    );
    storage
        .apply_unread_baseline(&alice, &cursor(1), 6)
        .unwrap();
    apply(&storage, &alice, 2, FolderChange::Refresh(Some(FOLDER)));
    drop(storage);

    let storage = Storage::open(&path).unwrap();
    assert_eq!(storage.cursor(&alice, SOURCE_ID).unwrap(), Some(cursor(2)));
    let cached = storage.folder_view(&alice).unwrap();
    assert!(cached.needs_refresh);
    assert_eq!(cached.folders, Some(vec![folder(5)]));
    assert!(cached.catalog.is_some());
    assert_eq!(cached.unread, Some(6));
    let pending = plan(&storage, &alice);
    assert_eq!(pending.folder_id(), Some(FOLDER));
    // Пустой точечный результат подтверждает удаление только у владельца запроса.
    assert!(
        storage
            .save_folder_refresh(&alice, &pending, vec![])
            .unwrap()
    );
    assert_eq!(storage.folder_view(&alice).unwrap().folders, Some(vec![]));
    assert_eq!(
        storage.folder_view(&bob).unwrap().folders,
        Some(vec![folder(9)])
    );
    assert!(storage.folder_refresh_plan(&alice).unwrap().is_none());
}

/// Запрос закрывает прежние отметки и оставляет новые, включая событие на свой UUID.
#[test]
fn marks_during_refresh_keep_the_next_request_pending() {
    let storage = Storage::in_memory().unwrap();
    let owner = owner("alice");
    let initial = plan(&storage, &owner);
    storage
        .request_folder_refresh(&owner, Some(FOLDER))
        .unwrap();
    assert!(
        storage
            .save_folder_refresh(&owner, &initial, vec![folder(1)])
            .unwrap()
    );

    let first = plan(&storage, &owner);
    assert_eq!(first.folder_id(), Some(FOLDER));
    storage
        .request_folder_refresh(&owner, Some(OTHER_FOLDER))
        .unwrap();
    storage
        .request_folder_refresh(&owner, Some(FOLDER))
        .unwrap();
    assert!(
        storage
            .save_folder_refresh(&owner, &first, vec![folder(2)])
            .unwrap()
    );
    assert!(
        !storage
            .save_folder_refresh(&owner, &first, vec![folder(1)])
            .unwrap()
    );
    assert_eq!(
        storage.folder_view(&owner).unwrap().folders,
        Some(vec![folder(2)])
    );

    let second = plan(&storage, &owner);
    assert_eq!(second.folder_id(), Some(OTHER_FOLDER));
    assert!(
        storage
            .save_folder_refresh(&owner, &second, vec![])
            .unwrap()
    );
    assert_eq!(plan(&storage, &owner).folder_id(), Some(FOLDER));
    storage.request_folder_refresh(&owner, None).unwrap();
    let full = plan(&storage, &owner);
    assert_eq!(full.folder_id(), None);
    assert!(
        storage
            .save_folder_refresh(&owner, &full, vec![folder(3)])
            .unwrap()
    );
    assert!(storage.folder_refresh_plan(&owner).unwrap().is_none());
}

/// Удаление элемента и папки меняет снимок сразу и отклоняет уже начатые ответы.
#[test]
fn deletion_blocks_late_refresh_and_duplicate_has_no_effect() {
    let storage = Storage::in_memory().unwrap();
    let owner = owner("alice");
    assert!(
        storage
            .save_folder_refresh(&owner, &plan(&storage, &owner), vec![folder(1)])
            .unwrap()
    );
    storage.request_folder_refresh(&owner, None).unwrap();
    let in_flight = plan(&storage, &owner);
    assert_eq!(
        apply(&storage, &owner, 1, FolderChange::DeleteItem(ITEM)),
        ApplyOutcome::Applied
    );
    assert_eq!(storage.cursor(&owner, SOURCE_ID).unwrap(), Some(cursor(1)));
    assert_eq!(
        apply(&storage, &owner, 1, FolderChange::DeleteItem(ITEM)),
        ApplyOutcome::Duplicate
    );
    assert!(
        !storage
            .save_folder_refresh(&owner, &in_flight, vec![folder(1)])
            .unwrap()
    );
    let mut updated = folder(1);
    updated.items.clear();
    assert_eq!(
        storage.folder_view(&owner).unwrap().folders,
        Some(vec![updated])
    );

    let in_flight = plan(&storage, &owner);
    apply(&storage, &owner, 2, FolderChange::DeleteFolder(FOLDER));
    assert!(
        !storage
            .save_folder_refresh(&owner, &in_flight, vec![folder(1)])
            .unwrap()
    );
    assert_eq!(storage.folder_view(&owner).unwrap().folders, Some(vec![]));
}

/// Отзыв, сброс и новая авторитетная сверка защищают и папки, и каталог.
#[test]
fn revocation_and_reset_invalidate_pending_responses_for_their_owner() {
    let storage = Storage::in_memory().unwrap();
    let alice = owner("alice");
    let bob = owner("bob");
    for owner in [&alice, &bob] {
        save_catalog(&storage, owner);
        assert!(
            storage
                .save_folder_refresh(owner, &plan(&storage, owner), vec![folder(1)])
                .unwrap()
        );
    }
    storage.request_folder_refresh(&alice, None).unwrap();
    let in_flight = plan(&storage, &alice);
    let catalog_in_flight = storage.catalog_revision(&alice).unwrap();
    let removed = HistoryChange::RemoveStream(STREAM.to_string());
    storage
        .apply(
            &alice,
            SOURCE_ID,
            &cursor(1),
            EventWrite {
                history: Some(&removed),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        !storage
            .save_folder_refresh(&alice, &in_flight, vec![folder(5)])
            .unwrap()
    );
    assert!(matches!(
        storage.save_chat_catalog(&alice, "[]", catalog_in_flight),
        Err(StorageError::StaleCatalog)
    ));
    let view = storage.folder_view(&alice).unwrap();
    assert!(view.folders.unwrap()[0].items.is_empty());
    assert_eq!(view.catalog.as_deref(), Some("[]"));

    let in_flight = plan(&storage, &alice);
    let catalog_in_flight = storage.catalog_revision(&alice).unwrap();
    storage.reset_source(&alice, SOURCE_ID).unwrap();
    assert!(storage.cursor(&alice, SOURCE_ID).unwrap().is_none());
    assert!(
        !storage
            .save_folder_refresh(&alice, &in_flight, vec![folder(1)])
            .unwrap()
    );
    assert!(matches!(
        storage.save_chat_catalog(&alice, "[]", catalog_in_flight),
        Err(StorageError::StaleCatalog)
    ));
    let view = storage.folder_view(&alice).unwrap();
    assert!(view.folders.is_none() && view.catalog.is_none() && view.needs_refresh);
    assert!(storage.catalog_needs_refresh(&alice).unwrap());
    assert_eq!(plan(&storage, &alice).folder_id(), None);
    assert_eq!(
        storage.folder_view(&bob).unwrap().folders,
        Some(vec![folder(1)])
    );
    assert!(!storage.catalog_needs_refresh(&bob).unwrap());
}

/// Новая связь и возврат из архива заказывают каталог, частые события известного чата пропускаются.
#[test]
fn new_streams_refresh_catalog_without_repeating_for_known_links() {
    let storage = Storage::in_memory().unwrap();
    let owner = owner("alice");
    storage.save_chat_catalog(&owner, "[]", 0).unwrap();
    assert!(
        storage
            .save_folder_refresh(&owner, &plan(&storage, &owner), vec![folder(1)])
            .unwrap()
    );
    assert!(storage.catalog_needs_refresh(&owner).unwrap());
    let revision = storage.catalog_revision(&owner).unwrap();
    // Пустой успешный ответ оставляет связь ждать события о доступности разговора.
    storage.save_chat_catalog(&owner, "[]", revision).unwrap();
    apply(&storage, &owner, 1, FolderChange::Refresh(Some(FOLDER)));
    assert!(
        storage
            .save_folder_refresh(&owner, &plan(&storage, &owner), vec![folder(2)])
            .unwrap()
    );
    assert!(!storage.catalog_needs_refresh(&owner).unwrap());
    save_catalog(&storage, &owner);
    apply(
        &storage,
        &owner,
        2,
        FolderChange::StreamChanged {
            stream_id: STREAM,
            archived: false,
        },
    );
    assert!(storage.folder_refresh_plan(&owner).unwrap().is_none());
    apply(
        &storage,
        &owner,
        3,
        FolderChange::StreamChanged {
            stream_id: STREAM,
            archived: true,
        },
    );
    assert_eq!(
        storage.folder_view(&owner).unwrap().catalog.as_deref(),
        Some("[]")
    );
    assert!(
        storage.folder_view(&owner).unwrap().folders.unwrap()[0]
            .items
            .is_empty()
    );
    assert_eq!(plan(&storage, &owner).folder_id(), None);
    assert!(storage.catalog_needs_refresh(&owner).unwrap());
    let archive_revision = storage.catalog_revision(&owner).unwrap();
    storage
        .save_chat_catalog(&owner, "[]", archive_revision)
        .unwrap();
    assert!(!storage.catalog_needs_refresh(&owner).unwrap());
    apply(
        &storage,
        &owner,
        4,
        FolderChange::StreamChanged {
            stream_id: STREAM,
            archived: false,
        },
    );
    assert!(storage.catalog_needs_refresh(&owner).unwrap());
}

/// Если сохранение курсора упало, изменения папок откатываются и подписка молчит.
#[test]
fn failed_cursor_write_rolls_back_folder_change() {
    let storage = Storage::in_memory().unwrap();
    let owner = owner("alice");
    assert!(
        storage
            .save_folder_refresh(&owner, &plan(&storage, &owner), vec![folder(1)])
            .unwrap()
    );
    let changes = storage.folder_changes();
    storage
        .connection
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_cursor BEFORE INSERT ON sync_cursors
         BEGIN SELECT RAISE(ABORT, 'cursor unavailable'); END;",
        )
        .unwrap();
    assert!(
        storage
            .apply(
                &owner,
                SOURCE_ID,
                &cursor(1),
                EventWrite {
                    folder: Some(FolderChange::DeleteFolder(FOLDER)),
                    ..Default::default()
                }
            )
            .is_err()
    );
    assert_eq!(
        storage.folder_view(&owner).unwrap().folders,
        Some(vec![folder(1)])
    );
    assert!(storage.cursor(&owner, SOURCE_ID).unwrap().is_none());
    assert!(!changes.has_changed().unwrap());
}
