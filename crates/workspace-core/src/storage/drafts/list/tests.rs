//! Проверяет локальные карточки, точное чтение и защиту удалённой версии от записи.

use super::*;
use crate::{
    context::AccountId,
    storage::{DraftReply, DraftWriteOutcome, PlaintextDraft},
};

/// Создаёт владельца тестового профиля и фиксированный маршрут редактора.
fn address() -> (OwnerContext, DraftScope) {
    (
        OwnerContext {
            account_id: AccountId("a".into()),
            server: "https://workspace.test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        },
        DraftScope {
            stream_id: Uuid::from_u128(1),
            topic_id: Uuid::from_u128(2),
        },
    )
}

/// Готовит обычный снимок или очищенный редактор без вкладок.
fn draft(number: u128, text: &str) -> PlaintextDraft {
    PlaintextDraft {
        id: Uuid::from_u128(number),
        version: 1,
        markdown: text.into(),
        replies: None,
    }
}

/// Показывает несколько черновиков одной темы и никогда не принимает содержимое другого владельца.
#[test]
fn lists_separate_versions_and_preserves_exact_opening() {
    let storage = Storage::in_memory().unwrap();
    let (owner, scope) = address();
    let access = storage.prepare_draft_access(&owner, scope).unwrap();
    let first = draft(3, "Первый\n  текст");
    let reply_id = Uuid::from_u128(4);
    let reply = PlaintextDraft {
        replies: Some(DraftReplies {
            active: reply_id,
            tabs: vec![DraftReply {
                id: reply_id,
                message_id: Uuid::from_u128(7),
                fragment: None,
                answer: "Набранный ответ".into(),
                creation: 1,
            }],
        }),
        ..draft(4, "")
    };
    for saved in [&first, &reply, &draft(5, "  \n")] {
        storage.save_plaintext_draft(&access, saved).unwrap();
    }

    let list = storage.list_plaintext_drafts(&owner).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].version.id, reply.id);
    assert_eq!(list[0].preview, "Набранный ответ");
    assert_eq!(list[1].preview, "Первый текст");
    assert!(list.iter().all(|row| row.updated_at.is_some()));
    assert!(
        storage
            .read_plaintext_draft(&access, Some(reply.id))
            .unwrap()
            == Some(reply)
    );

    let mut other = owner.clone();
    other.project_id = "other-project".into();
    assert!(storage.list_plaintext_drafts(&other).unwrap().is_empty());
}

/// Оставляет новый текст при устаревшем клике и защищает удаление от запоздавшей записи.
#[test]
fn refuses_stale_delete_and_retains_tombstone() {
    let storage = Storage::in_memory().unwrap();
    let (owner, scope) = address();
    let access = storage.prepare_draft_access(&owner, scope).unwrap();
    let mut saved = draft(3, "Первый текст");
    storage.save_plaintext_draft(&access, &saved).unwrap();
    let seen = storage.list_plaintext_drafts(&owner).unwrap()[0].version;
    saved.version += 1;
    saved.markdown.push_str(" и дополнение");
    storage.save_plaintext_draft(&access, &saved).unwrap();
    assert!(!storage.delete_plaintext_draft(&owner, scope, seen).unwrap());
    let seen = storage.list_plaintext_drafts(&owner).unwrap()[0].version;
    assert!(storage.delete_plaintext_draft(&owner, scope, seen).unwrap());
    saved.version += 1;
    assert_eq!(
        storage.save_plaintext_draft(&access, &saved).unwrap(),
        DraftWriteOutcome::Revoked
    );
    assert!(storage.list_plaintext_drafts(&owner).unwrap().is_empty());
    assert!(
        storage
            .read_plaintext_draft(&access, Some(saved.id))
            .unwrap()
            .is_none()
    );
}

/// Открывает профиль со старой схемой и сохраняет старый черновик без выдуманной даты.
#[test]
fn migrates_profile_without_draft_timestamps() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drafts.sqlite");
    let (owner, scope) = address();
    let saved = draft(3, "Старый черновик");
    {
        let storage = Storage::open(&path).unwrap();
        let access = storage.prepare_draft_access(&owner, scope).unwrap();
        storage.save_plaintext_draft(&access, &saved).unwrap();
    }
    let old = rusqlite::Connection::open(&path).unwrap();
    old.execute_batch("ALTER TABLE messenger_drafts DROP COLUMN updated_at; ALTER TABLE messenger_drafts DROP COLUMN write_order")
        .unwrap();
    drop(old);

    let storage = Storage::open(&path).unwrap();
    let drafts = storage.list_plaintext_drafts(&owner).unwrap();
    assert_eq!(drafts[0].version.id, saved.id);
    assert_eq!(drafts[0].preview, saved.markdown);
    assert!(drafts[0].updated_at.is_none());
}
