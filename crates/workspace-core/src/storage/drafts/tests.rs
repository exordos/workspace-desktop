//! Проверяет сохранение черновиков при смене версии, владельца, области и подключения к базе.

use std::path::Path;

use crate::{
    context::AccountId,
    messenger::SOURCE_ID,
    storage::{
        ApplyOutcome, Cursor, EventWrite, HistoryChange, MessagePlacement, PreparedSendBody,
    },
};

use super::*;

/// Возвращает владельца синтетической сессии для теста изоляции.
fn owner() -> OwnerContext {
    OwnerContext {
        account_id: AccountId("test-account".into()),
        server: "https://workspace.test".into(),
        organization_id: "test-organization".into(),
        project_id: "test-project".into(),
        user_id: "test-user".into(),
    }
}

/// Даёт точный адрес темы, даже если у разговора всего одна тема.
fn scope(stream: u128, topic: u128) -> DraftScope {
    DraftScope {
        stream_id: Uuid::from_u128(stream),
        topic_id: Uuid::from_u128(topic),
    }
}

/// Готовит снимок с известными версией и содержимым.
fn draft(id: u128, version: u64, markdown: &str) -> PlaintextDraft {
    PlaintextDraft {
        id: Uuid::from_u128(id),
        version,
        markdown: markdown.into(),
        replies: None,
    }
}

/// Создаёт синтетический профиль ранней версии, когда база хранила только текст сообщения.
fn write_legacy_profile(path: &Path, owner: &OwnerContext) {
    let old = Connection::open(path).unwrap();
    old.execute_batch(
        "CREATE TABLE sync_messages (
             account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
             project_id TEXT NOT NULL, user_id TEXT NOT NULL, message_id TEXT NOT NULL, text TEXT NOT NULL,
             PRIMARY KEY (account_id, server, organization_id, project_id, user_id, message_id)
         );",
    )
    .unwrap();
    old.execute(
        "INSERT INTO sync_messages VALUES (?1, ?2, ?3, ?4, ?5, 'legacy-message', 'старое сообщение')",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id
        ],
    )
    .unwrap();
}

/// Проверяет чтение старого профиля и восстановление последних версий после перезапуска.
#[test]
fn local_draft_survives_legacy_profile_migration_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profile.sqlite");
    let owner = owner();
    let scope = scope(1, 2);
    write_legacy_profile(&path, &owner);
    let primary = draft(7, 4, "  Привет 👋\nещё строка\n");
    let secondary = draft(3, 15, "другой черновик той же темы");
    {
        let storage = Storage::open(&path).unwrap();
        assert_eq!(
            storage
                .message(&owner, "legacy-message")
                .unwrap()
                .unwrap()
                .body
                .markdown(),
            Some("старое сообщение")
        );
        let access = storage.prepare_draft_access(&owner, scope).unwrap();
        assert!(
            storage
                .read_plaintext_draft(&access, None)
                .unwrap()
                .is_none()
        );
        for snapshot in [&primary, &secondary] {
            assert_eq!(
                storage.save_plaintext_draft(&access, snapshot).unwrap(),
                DraftWriteOutcome::Saved
            );
        }
    }
    let reopened = Storage::open(&path).unwrap();
    let access = reopened.prepare_draft_access(&owner, scope).unwrap();
    assert!(reopened.read_plaintext_draft(&access, None).unwrap() == Some(secondary.clone()));
    assert!(
        reopened
            .read_plaintext_draft(&access, Some(primary.id))
            .unwrap()
            == Some(primary)
    );
    assert!(
        reopened
            .read_plaintext_draft(&access, Some(secondary.id))
            .unwrap()
            == Some(secondary)
    );
}

/// Локальный каталог допускает чтение только обычного черновика известной темы владельца.
#[test]
fn local_draft_read_requires_plain_catalog_scope() {
    use crate::messenger::catalog::{Chat, ChatMetadata, Topic, TopicMetadata};

    let storage = Storage::in_memory().unwrap();
    let owner = owner();
    let scope = scope(1, 2);
    let snapshot = draft(7, 1, "локальный текст");
    let access = storage.prepare_draft_access(&owner, scope).unwrap();
    storage.save_plaintext_draft(&access, &snapshot).unwrap();
    assert!(
        storage
            .read_catalog_plaintext_draft(&owner, scope, None)
            .unwrap()
            .is_none()
    );
    let mut chat = Chat {
        id: scope.stream_id,
        name: "чат".into(),
        direct: false,
        topics: vec![Topic {
            id: scope.topic_id,
            name: "тема".into(),
            metadata: TopicMetadata::default(),
        }],
        metadata: ChatMetadata {
            encrypted: Some(false),
            ..Default::default()
        },
    };
    let save = |chat: &Chat| {
        let revision = storage.catalog_revision(&owner).unwrap();
        storage
            .save_chat_catalog(
                &owner,
                &serde_json::to_string(&[chat]).unwrap(),
                revision,
                None,
            )
            .unwrap();
    };
    save(&chat);
    assert!(
        storage
            .read_catalog_plaintext_draft(&owner, scope, None)
            .unwrap()
            == Some(Some(snapshot.clone()))
    );
    assert!(
        storage
            .read_catalog_plaintext_draft(&owner, scope, Some(Uuid::new_v4()))
            .unwrap()
            == Some(None)
    );
    assert!(
        storage
            .read_catalog_plaintext_draft(&owner, super::tests::scope(1, 3), None)
            .unwrap()
            .is_none()
    );
    chat.metadata.encrypted = Some(true);
    save(&chat);
    assert!(
        storage
            .read_catalog_plaintext_draft(&owner, scope, None)
            .unwrap()
            .is_none()
    );
    chat.metadata.encrypted = None;
    save(&chat);
    assert!(
        storage
            .read_catalog_plaintext_draft(&owner, scope, None)
            .unwrap()
            .is_none()
    );
}

/// Обычное открытие выбирает недавнюю запись, а явный UUID возвращает нужную версию.
#[test]
fn ordinary_draft_open_chooses_most_recent_record() {
    let storage = Storage::in_memory().unwrap();
    let owner = owner();
    let scope = scope(1, 2);
    let access = storage.prepare_draft_access(&owner, scope).unwrap();
    let first = draft(1, 1, "старый вариант");
    let second = draft(2, 1, "ранний ввод");
    storage.save_plaintext_draft(&access, &first).unwrap();
    storage.save_plaintext_draft(&access, &second).unwrap();
    {
        let connection = storage.connection.lock().unwrap();
        connection.execute(
            "UPDATE messenger_drafts SET updated_at = CASE draft_id WHEN ?1 THEN '2026-01-01T00:00:00.000Z' ELSE '2026-01-01T00:00:01.000Z' END",
            params![first.id.to_string()],
        ).unwrap();
    }
    assert!(storage.read_plaintext_draft(&access, None).unwrap() == Some(second));
    assert!(
        storage
            .read_plaintext_draft(&access, Some(first.id))
            .unwrap()
            == Some(first.clone())
    );
    let updated = draft(1, 2, "изменённый старый вариант");
    storage.save_plaintext_draft(&access, &updated).unwrap();
    {
        let connection = storage.connection.lock().unwrap();
        connection
            .execute(
                "UPDATE messenger_drafts SET updated_at = '2026-01-01T00:00:00.000Z'",
                [],
            )
            .unwrap();
    }
    assert!(storage.read_plaintext_draft(&access, None).unwrap() == Some(updated));
}

/// После миграции старые записи сортируются по времени, новые — по порядку записи.
#[test]
fn draft_order_migration_preserves_legacy_order_and_tracks_new_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("draft-order.sqlite");
    let owner = owner();
    let scope = scope(1, 2);
    let first = draft(1, 1, "первый");
    let second = draft(2, 1, "второй");
    {
        let storage = Storage::open(&path).unwrap();
        let access = storage.prepare_draft_access(&owner, scope).unwrap();
        storage.save_plaintext_draft(&access, &first).unwrap();
        storage.save_plaintext_draft(&access, &second).unwrap();
    }
    let old = Connection::open(&path).unwrap();
    old.execute_batch("UPDATE messenger_drafts SET updated_at = CASE WHEN sequence = 1 THEN '2026-01-01T00:00:01.000Z' ELSE '2026-01-01T00:00:00.000Z' END; ALTER TABLE messenger_drafts DROP COLUMN write_order").unwrap();
    drop(old);
    let storage = Storage::open(&path).unwrap();
    let access = storage.prepare_draft_access(&owner, scope).unwrap();
    assert!(storage.read_plaintext_draft(&access, None).unwrap() == Some(first));
    let updated = draft(2, 2, "второй изменён");
    storage.save_plaintext_draft(&access, &updated).unwrap();
    assert!(storage.read_plaintext_draft(&access, None).unwrap() == Some(updated));
}

/// Сохраняет только адрес удаления между запусками и отклоняет обычную запись после защиты.
#[test]
fn protected_draft_purge_survives_reopen_without_restoring_text() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("protected.sqlite");
    let owner = owner();
    let scope = scope(1, 2);
    let ordinary = draft(19, 7, "old ordinary text");
    let protection = DraftVersion {
        id: ordinary.id,
        version: 8,
    };
    {
        let storage = Storage::open(&path).unwrap();
        let access = storage.prepare_draft_access(&owner, scope).unwrap();
        assert_eq!(
            storage.save_plaintext_draft(&access, &ordinary).unwrap(),
            DraftWriteOutcome::Saved
        );
        assert!(
            storage
                .protect_plaintext_draft(&access, protection)
                .unwrap()
        );
        assert!(
            storage
                .read_plaintext_draft(&access, None)
                .unwrap()
                .is_none()
        );
    }

    let storage = Storage::open(&path).unwrap();
    let access = storage.prepare_draft_access(&owner, scope).unwrap();
    assert_eq!(
        storage.save_plaintext_draft(&access, &ordinary).unwrap(),
        DraftWriteOutcome::Revoked
    );
    let pending = storage.pending_protected_draft_purges(&owner).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, scope);
    assert_eq!(pending[0].1.id, ordinary.id);
    assert_eq!(pending[0].1.version, protection.version);
    let mut another = owner.clone();
    another.account_id = AccountId("other-account".into());
    assert!(
        storage
            .pending_protected_draft_purges(&another)
            .unwrap()
            .is_empty()
    );
    let old = DraftVersion {
        id: protection.id,
        version: 7,
    };
    storage
        .confirm_protected_draft_purge(&owner, scope, old)
        .unwrap();
    assert_eq!(
        storage
            .pending_protected_draft_purges(&owner)
            .unwrap()
            .len(),
        1
    );
    storage
        .confirm_protected_draft_purge(&owner, scope, protection)
        .unwrap();
    assert!(
        storage
            .protect_plaintext_draft(&access, protection)
            .unwrap()
    );
    assert!(
        storage
            .pending_protected_draft_purges(&owner)
            .unwrap()
            .is_empty()
    );
    assert!(
        storage
            .read_plaintext_draft(&access, None)
            .unwrap()
            .is_none()
    );
}

/// Показывает, что очистка и более свежий ввод побеждают позднее сохранение.
#[test]
fn local_draft_versions_are_ordered_and_identical_retries_are_idempotent() {
    let storage = Storage::in_memory().unwrap();
    let access = storage.prepare_draft_access(&owner(), scope(1, 2)).unwrap();
    let initial = draft(19, 7, "первый ввод");
    let newer = draft(19, 8, "второй ввод");
    let different = draft(19, 8, "текст не совпал при той же версии");
    let cleared = draft(19, 9, "");
    for (snapshot, result) in [
        (&initial, DraftWriteOutcome::Saved),
        (&newer, DraftWriteOutcome::Saved),
        (&initial, DraftWriteOutcome::StaleVersion),
        (&newer, DraftWriteOutcome::Unchanged),
        (&different, DraftWriteOutcome::VersionConflict),
        (&cleared, DraftWriteOutcome::Saved),
        (&newer, DraftWriteOutcome::StaleVersion),
    ] {
        assert_eq!(
            storage.save_plaintext_draft(&access, snapshot).unwrap(),
            result
        );
    }
    for invalid in [0, u64::MAX] {
        assert!(matches!(
            storage.save_plaintext_draft(&access, &draft(19, invalid, "вне диапазона")),
            Err(StorageError::InvalidDraftVersion)
        ));
    }
    assert!(storage.read_plaintext_draft(&access, None).unwrap() == Some(cleared));
}

/// Проверяет точный ключ владельца, неизменность темы и отсутствие подстановки чужой строки.
#[test]
fn local_draft_uuid_stays_in_its_scope_with_independent_owners() {
    let storage = Storage::in_memory().unwrap();
    let owner = owner();
    let first = storage.prepare_draft_access(&owner, scope(1, 2)).unwrap();
    let neighboring = storage.prepare_draft_access(&owner, scope(1, 3)).unwrap();
    let saved = draft(9, 1, "первый проект");
    let collision = draft(9, 2, "тот же UUID в другой теме");
    assert_eq!(
        storage.save_plaintext_draft(&first, &saved).unwrap(),
        DraftWriteOutcome::Saved
    );
    assert_eq!(
        storage
            .save_plaintext_draft(&neighboring, &collision)
            .unwrap(),
        DraftWriteOutcome::ScopeConflict
    );
    assert!(
        storage
            .read_plaintext_draft(&neighboring, Some(saved.id))
            .unwrap()
            .is_none()
    );

    let owners = [
        OwnerContext {
            account_id: AccountId("other".into()),
            ..owner.clone()
        },
        OwnerContext {
            project_id: "neighbor-project".into(),
            ..owner.clone()
        },
        OwnerContext {
            server: "https://other.test".into(),
            ..owner.clone()
        },
        OwnerContext {
            organization_id: "other-organization".into(),
            ..owner.clone()
        },
        OwnerContext {
            user_id: "other-user".into(),
            ..owner
        },
    ];
    for other in owners {
        let access = storage.prepare_draft_access(&other, scope(1, 2)).unwrap();
        assert!(
            storage
                .read_plaintext_draft(&access, Some(saved.id))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            storage.save_plaintext_draft(&access, &collision).unwrap(),
            DraftWriteOutcome::Saved
        );
    }
    assert!(
        storage
            .read_plaintext_draft(&first, Some(saved.id))
            .unwrap()
            == Some(saved)
    );
}

/// Применяет штатную транзакцию журнала для удаления стрима или темы.
fn revoke_at(
    storage: &Storage,
    owner: &OwnerContext,
    version: u64,
    change: &HistoryChange,
) -> ApplyOutcome {
    storage
        .apply(
            owner,
            SOURCE_ID,
            &Cursor {
                epoch_generation: "test-epoch".into(),
                epoch_version: version,
            },
            EventWrite {
                history: Some(change),
                ..EventWrite::default()
            },
        )
        .unwrap()
        .outcome
}

/// Проверяет, что запись до и после отзыва не восстанавливает текст даже из второго соединения.
#[test]
fn local_draft_revocation_invalidates_only_the_matching_scope() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profile.sqlite");
    let writer = Storage::open(&path).unwrap();
    let events = Storage::open(&path).unwrap();
    let owner = owner();
    let other = OwnerContext {
        user_id: "other-user".into(),
        ..owner.clone()
    };
    let removed_scope = scope(1, 2);
    let neighbor_scope = scope(1, 3);
    let another_stream_scope = scope(4, 5);
    let removed = writer.prepare_draft_access(&owner, removed_scope).unwrap();
    let neighbor = writer.prepare_draft_access(&owner, neighbor_scope).unwrap();
    let another_stream = writer
        .prepare_draft_access(&owner, another_stream_scope)
        .unwrap();
    let other_owner = writer.prepare_draft_access(&other, removed_scope).unwrap();
    let first = draft(21, 1, "закрытая тема");
    let delayed = draft(22, 1, "запись ещё не появилась");
    let sibling = draft(23, 1, "соседняя тема");
    let outside = draft(24, 1, "другой стрим");
    for (access, entry) in [
        (&removed, &first),
        (&neighbor, &sibling),
        (&another_stream, &outside),
        (&other_owner, &first),
    ] {
        assert_eq!(
            writer.save_plaintext_draft(access, entry).unwrap(),
            DraftWriteOutcome::Saved
        );
    }

    let remove_topic = HistoryChange::RemoveTopic(MessagePlacement {
        stream_id: removed_scope.stream_id.to_string(),
        topic_id: removed_scope.topic_id.to_string(),
    });
    assert_eq!(
        revoke_at(&events, &owner, 1, &remove_topic),
        ApplyOutcome::Applied
    );
    assert!(matches!(
        writer.read_plaintext_draft(&removed, None),
        Err(StorageError::StaleDraftAccess)
    ));
    assert!(matches!(
        writer.save_plaintext_draft(&removed, &delayed),
        Err(StorageError::StaleDraftAccess)
    ));
    assert_eq!(
        writer.save_plaintext_draft(&neighbor, &sibling).unwrap(),
        DraftWriteOutcome::Unchanged
    );
    assert!(writer.read_plaintext_draft(&other_owner, None).unwrap() == Some(first.clone()));

    let rejoined = writer.prepare_draft_access(&owner, removed_scope).unwrap();
    assert!(
        writer
            .read_plaintext_draft(&rejoined, None)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        writer.save_plaintext_draft(&rejoined, &first).unwrap(),
        DraftWriteOutcome::Revoked
    );
    assert_eq!(
        writer.save_plaintext_draft(&neighbor, &first).unwrap(),
        DraftWriteOutcome::ScopeConflict
    );
    assert_eq!(
        revoke_at(&events, &owner, 1, &remove_topic),
        ApplyOutcome::Duplicate
    );
    assert_eq!(
        writer
            .save_plaintext_draft(&rejoined, &draft(25, 1, "новый ввод"))
            .unwrap(),
        DraftWriteOutcome::Saved
    );

    let remove_stream = HistoryChange::RemoveStream(removed_scope.stream_id.to_string());
    assert_eq!(
        revoke_at(&events, &owner, 2, &remove_stream),
        ApplyOutcome::Applied
    );
    for access in [&rejoined, &neighbor] {
        assert!(matches!(
            writer.read_plaintext_draft(access, None),
            Err(StorageError::StaleDraftAccess)
        ));
    }
    assert!(writer.read_plaintext_draft(&another_stream, None).unwrap() == Some(outside));
}

/// Проверяет, что сброс журнала требует новой проверки владельца и сохраняет локальный ввод.
#[test]
fn local_draft_source_reset_preserves_input_and_invalidates_held_access() {
    let storage = Storage::in_memory().unwrap();
    let owner = owner();
    let scope = scope(1, 2);
    let access = storage.prepare_draft_access(&owner, scope).unwrap();
    let snapshot = draft(5, 1, "локальный ввод");
    storage.save_plaintext_draft(&access, &snapshot).unwrap();

    storage.reset_source(&owner, SOURCE_ID).unwrap();
    assert!(matches!(
        storage.save_plaintext_draft(&access, &draft(5, 2, "поздний ввод")),
        Err(StorageError::StaleDraftAccess)
    ));
    let checked_again = storage.prepare_draft_access(&owner, scope).unwrap();
    assert!(storage.read_plaintext_draft(&checked_again, None).unwrap() == Some(snapshot));
}

/// Создаёт локальную версию с двумя независимыми ответами и исходным вводом.
fn reply_draft(version: u64) -> PlaintextDraft {
    let mut draft = draft(41, version, "До ответов");
    draft.replies = Some(DraftReplies {
        active: Uuid::from_u128(9),
        tabs: vec![
            DraftReply {
                id: Uuid::from_u128(12),
                message_id: Uuid::from_u128(52),
                fragment: Some("фрагмент 🦊".into()),
                answer: "Второй ответ".into(),
                creation: 20,
            },
            DraftReply {
                id: Uuid::from_u128(9),
                message_id: Uuid::from_u128(50),
                fragment: None,
                answer: "Первый ответ\nすし".into(),
                creation: 1,
            },
        ],
    });
    draft
}

/// Старые версии, завершившиеся после очереди, не возвращают отправленный ответ в редактор.
#[test]
fn outbox_recovers_consumed_reply_version_without_losing_newer_input() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.sqlite3");
    let owner = owner();
    let scope = scope(15, 16);
    let message = crate::storage::PreparedSend {
        canonical_id: Uuid::from_u128(73),
        public_id: Uuid::from_u128(75),
        stream_id: scope.stream_id,
        topic_id: scope.topic_id,
        body: PreparedSendBody::Markdown("Сообщение очереди".into()),
    };
    let version = |draft: &PlaintextDraft| DraftVersion {
        id: draft.id,
        version: draft.version,
    };
    let sent = reply_draft(1);
    let unsent = reply_draft(2);
    {
        let storage = Storage::open(&path).unwrap();
        let access = storage.prepare_draft_access(&owner, scope).unwrap();
        assert!(
            storage
                .save_prepared_send_with_draft(&owner, &message, Some((&access, version(&sent))))
                .unwrap()
        );
        assert_eq!(
            storage.save_plaintext_draft(&access, &sent).unwrap(),
            DraftWriteOutcome::VersionConflict,
        );
    }
    {
        let storage = Storage::open(&path).unwrap();
        let access = storage.prepare_draft_access(&owner, scope).unwrap();
        let restored = storage
            .read_plaintext_draft(&access, None)
            .unwrap()
            .unwrap();
        assert_eq!(restored.id, sent.id);
        assert_eq!(restored.version, sent.version);
        assert!(restored.markdown.is_empty() && restored.replies.is_none());
        let queued = storage.pending_sends(&owner).unwrap();
        assert_eq!(queued, vec![message.clone()]);

        assert_eq!(
            storage.save_plaintext_draft(&access, &unsent).unwrap(),
            DraftWriteOutcome::Saved
        );
        let old_submission = crate::storage::PreparedSend {
            canonical_id: Uuid::from_u128(83),
            ..message.clone()
        };
        assert!(
            storage
                .save_prepared_send_with_draft(
                    &owner,
                    &old_submission,
                    Some((&access, version(&sent)))
                )
                .unwrap()
        );
        assert!(
            storage
                .read_plaintext_draft(&access, None)
                .unwrap()
                .is_some_and(|saved| saved == unsent)
        );
    }
    let storage = Storage::open(&path).unwrap();
    let access = storage.prepare_draft_access(&owner, scope).unwrap();
    assert!(
        storage
            .read_plaintext_draft(&access, None)
            .unwrap()
            .is_some_and(|saved| saved == unsent)
    );
}

/// При конфликте адреса снимка обе записи откатываются.
#[test]
fn outbox_rejects_reply_version_from_another_topic() {
    let storage = Storage::in_memory().unwrap();
    let owner = owner();
    let original_scope = scope(1, 2);
    let neighbor_scope = scope(1, 3);
    let original = storage
        .prepare_draft_access(&owner, original_scope)
        .unwrap();
    let wrong = storage
        .prepare_draft_access(&owner, neighbor_scope)
        .unwrap();
    let saved = reply_draft(1);
    storage.save_plaintext_draft(&original, &saved).unwrap();
    let message = crate::storage::PreparedSend {
        canonical_id: Uuid::from_u128(101),
        public_id: Uuid::from_u128(102),
        stream_id: neighbor_scope.stream_id,
        topic_id: neighbor_scope.topic_id,
        body: PreparedSendBody::Markdown("Сообщение из соседней темы".into()),
    };
    assert!(matches!(
        storage.save_prepared_send_with_draft(
            &owner,
            &message,
            Some((
                &wrong,
                DraftVersion {
                    id: saved.id,
                    version: saved.version
                }
            ))
        ),
        Err(StorageError::StaleDraftAccess),
    ));
    assert!(storage.pending_sends(&owner).unwrap().is_empty());
    assert!(
        storage
            .read_plaintext_draft(&original, None)
            .unwrap()
            .is_some_and(|draft| draft == saved)
    );
}
