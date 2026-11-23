//! Проверяет индекс, публикацию, очистку и восстановление кэша на временном профиле.

use std::{
    fs,
    io::{Read, Write},
    sync::Arc,
};

use tempfile::TempDir;
use uuid::Uuid;

use super::*;
use crate::context::AccountId;

/// Создаёт отдельный контекст, в котором один UUID файла может повторяться.
fn owner(account: &str) -> OwnerContext {
    OwnerContext {
        account_id: AccountId(account.into()),
        server: "https://example.invalid".into(),
        organization_id: "org".into(),
        project_id: "project".into(),
        user_id: account.into(),
    }
}

/// Возвращает простой ключ известного оригинала.
fn key(id: u128) -> MediaKey {
    MediaKey {
        file_id: Uuid::from_u128(id),
        variant: MediaVariant::Original,
    }
}

/// Строит подтверждённую область и сообщение для тестовой проекции.
fn source(stream: u128, topic: u128, message: u128) -> MediaSource {
    MediaSource {
        scope: HistoryScope {
            stream_id: Uuid::from_u128(stream),
            topic_id: Some(Uuid::from_u128(topic)),
        },
        message_id: Uuid::from_u128(message),
    }
}

/// Открывает кэш таким же способом, как при запуске приложения.
fn open_profile(profile: &TempDir, budget_bytes: u64) -> MediaCache {
    let storage = Arc::new(Storage::open(profile.path().join("messenger.sqlite3")).unwrap());
    MediaCache::new(
        storage,
        profile.path().join("media"),
        MediaCachePolicy {
            budget_bytes,
            max_object_bytes: 20,
        },
    )
    .unwrap()
}

/// Записывает прямую ссылку тем же методом, что использует история.
fn add(cache: &MediaCache, owner: &OwnerContext, source: MediaSource, key: MediaKey) {
    replace_message_refs(
        &cache.connection().unwrap(),
        owner,
        source.message_id,
        source.scope,
        "2026-11-18T12:00:00.000000Z",
        &[MediaLink {
            file_id: key.file_id,
            kind: MediaKind::Image,
            name: Some("example from message.png".into()),
        }],
    )
    .unwrap();
}

/// Публикует законченную копию после подготовки запроса.
fn put(
    cache: &MediaCache,
    owner: &OwnerContext,
    source: MediaSource,
    key: MediaKey,
    bytes: &[u8],
    now: u64,
) {
    let fence = cache.prepare(owner, source, key).unwrap();
    let mut staged = cache.stage().unwrap();
    staged.write_all(bytes).unwrap();
    assert_eq!(staged.size(), bytes.len() as u64);
    staged.seal().unwrap();
    cache
        .publish(
            fence,
            staged,
            MediaMetadata {
                name: Some("example.png".into()),
                mime: Some("image/png".into()),
                file_stream: None,
            },
            now,
        )
        .unwrap();
}

/// Читает готовый оригинал через тот же управляемый читатель, который использует мессенджер.
fn get(cache: &MediaCache, owner: &OwnerContext, source: MediaSource, key: MediaKey) -> Vec<u8> {
    let mut reader = cache.open(owner, source, key, 5).unwrap().unwrap();
    assert_eq!(reader.metadata().mime.as_deref(), Some("image/png"));
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(reader.size(), bytes.len() as u64);
    bytes
}

/// Очистка темы сохраняет общую копию, а отложенное удаление не трогает новую.
#[test]
fn clears_shared_copy_once_without_deleting_new_incarnation() {
    let profile = TempDir::new().unwrap();
    let cache = open_profile(&profile, 100);
    let account = owner("a");
    let first = source(10, 1, 5);
    let second = source(10, 2, 6);
    let file = key(7);
    add(&cache, &account, first, file);
    add(&cache, &account, second, file);
    put(&cache, &account, first, file, b"old", 1);
    let report = cache
        .clear(&account, &MediaFilter::Scope(first.scope))
        .unwrap();
    assert_eq!(report.estimate.shared_bytes, 3);
    assert_eq!(report.freed_bytes, 0);
    assert_eq!(get(&cache, &account, second, file), b"old");

    let mut active = cache.open(&account, first, file, 2).unwrap().unwrap();
    let report = cache
        .clear(&account, &MediaFilter::File(file.file_id))
        .unwrap();
    assert_eq!(report.estimate.reclaimable_bytes, 3);
    assert_eq!(report.freed_bytes, 0);
    assert_eq!(report.pending_bytes, 3);
    assert_eq!(
        cache
            .estimate(&account, &MediaFilter::All)
            .unwrap()
            .pending_bytes,
        3
    );
    assert_eq!(
        active.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    put(&cache, &account, second, file, b"newer", 4);
    drop(active);
    assert_eq!(cache.reap().unwrap().freed_bytes, 3);
    assert_eq!(get(&cache, &account, second, file), b"newer");
    let explicit = cache.begin_clear(&account, &MediaFilter::All).unwrap();
    assert_eq!(cache.reap().unwrap().freed_bytes, 0);
    assert_eq!(cache.finish_clear(explicit).unwrap().freed_bytes, 5);
}

/// Изоляция сохраняется на диске, а неизвестный файловый стрим требует подтверждения.
#[test]
fn restricts_unknown_origin_to_downloaded_streams_and_reopens() {
    let profile = TempDir::new().unwrap();
    let first = source(10, 1, 5);
    let second = source(20, 2, 6);
    let account = owner("a");
    let other = owner("b");
    let file = key(7);
    {
        let cache = open_profile(&profile, 100);
        add(&cache, &account, first, file);
        add(&cache, &account, second, file);
        add(&cache, &other, first, file);
        put(&cache, &account, first, file, b"data", 1);
        assert!(cache.open(&account, second, file, 2).unwrap().is_none());
        assert!(cache.open(&other, first, file, 2).unwrap().is_none());
        put(&cache, &account, second, file, b"data", 3);
        assert_eq!(
            cache
                .estimate(&account, &MediaFilter::All)
                .unwrap()
                .selected_bytes,
            4
        );
    }
    let cache = open_profile(&profile, 100);
    assert_eq!(get(&cache, &account, second, file), b"data");
    revoke_scope(
        &cache.connection().unwrap(),
        &account,
        HistoryScope {
            stream_id: first.scope.stream_id,
            topic_id: None,
        },
    )
    .unwrap();
    assert!(cache.open(&account, second, file, 6).unwrap().is_none());
    assert_eq!(cache.reap().unwrap().freed_bytes, 4);
}

/// Групповая очистка блокирует уже начатую загрузку даже до появления файла в индексе.
#[test]
fn fences_late_publication_after_clear() {
    let profile = TempDir::new().unwrap();
    let cache = open_profile(&profile, 100);
    let account = owner("a");
    let source = source(10, 1, 5);
    let file = key(7);
    add(&cache, &account, source, file);
    let fence = cache.prepare(&account, source, file).unwrap();
    let adjacent = MediaSource {
        scope: HistoryScope {
            topic_id: Some(Uuid::from_u128(99)),
            ..source.scope
        },
        message_id: Uuid::from_u128(99),
    };
    add(&cache, &account, adjacent, file);
    let unaffected = cache.prepare(&account, adjacent, file).unwrap();
    assert!(cache.is_current(&fence).unwrap());
    let mut staged = cache.stage().unwrap();
    staged.write_all(b"late").unwrap();
    assert_eq!(
        cache
            .clear(&account, &MediaFilter::Scope(source.scope))
            .unwrap()
            .freed_bytes,
        0
    );
    assert!(!cache.is_current(&fence).unwrap());
    assert!(cache.is_current(&unaffected).unwrap());
    assert!(matches!(
        cache.publish(fence, staged, MediaMetadata::default(), 10),
        Err(MediaCacheError::Stale)
    ));
    assert!(cache.open(&account, source, file, 11).unwrap().is_none());
    assert_eq!(
        fs::read_dir(profile.path().join("media")).unwrap().count(),
        0
    );
}

/// Курсоры выдерживают одинаковые размеры и даты, а очистка байтов сохраняет историю.
#[test]
fn pages_history_and_copies_and_evicts_oldest_objects() {
    let profile = TempDir::new().unwrap();
    let cache = open_profile(&profile, 10);
    let account = owner("a");
    let first = source(10, 1, 5);
    let second = source(10, 1, 6);
    let third = source(10, 1, 9);
    for (index, src) in [first, second, third].into_iter().enumerate() {
        let file = key(50 + index as u128);
        add(&cache, &account, src, file);
        put(&cache, &account, src, file, b"data", index as u64);
    }
    assert_eq!(cache.enforce_budget().unwrap().freed_bytes, 4);
    assert!(cache.open(&account, first, key(50), 7).unwrap().is_none());
    let query = MediaCopyQuery {
        scope: Some(first.scope),
        kind: Some(MediaKind::Image),
        sort: MediaCopySort::Largest,
        limit: 1,
        after: None,
    };
    let one = cache.list(&account, query.clone()).unwrap();
    let two = cache
        .list(
            &account,
            MediaCopyQuery {
                after: one.next,
                ..query
            },
        )
        .unwrap();
    assert_ne!(one.copies[0].key, two.copies[0].key);
    assert!(two.next.is_none());
    let newest = cache
        .list_history(&account, first.scope, 0, None, None, 2)
        .unwrap();
    let older = cache
        .list_history(&account, first.scope, 0, None, newest.next, 2)
        .unwrap();
    assert_eq!(newest.attachments.len(), 2);
    assert_eq!(older.attachments[0].message_id, first.message_id);
    assert!(!older.attachments[0].cached);
    assert_eq!(
        cache
            .clear(
                &account,
                &MediaFilter::Files(vec![key(51).file_id, key(52).file_id])
            )
            .unwrap()
            .freed_bytes,
        8
    );
    assert!(
        cache
            .list_history(&account, first.scope, 0, None, None, 3)
            .unwrap()
            .attachments
            .iter()
            .all(|entry| !entry.cached)
    );
}

/// Следующий запуск чистит незавершённый файл, потерянную копию и файл без записи.
#[test]
fn recovers_missing_and_orphaned_files() {
    let profile = TempDir::new().unwrap();
    let account = owner("a");
    let source = source(10, 1, 5);
    let file = key(7);
    let directory = profile.path().join("media");
    {
        let cache = open_profile(&profile, 100);
        add(&cache, &account, source, file);
        put(&cache, &account, source, file, b"data", 1);
        let path = fs::read_dir(&directory)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::remove_file(path).unwrap();
        for suffix in ["part", "bin"] {
            fs::write(
                directory.join(format!("{}.{suffix}", Uuid::new_v4())),
                b"partial",
            )
            .unwrap();
        }
    }
    let cache = open_profile(&profile, 100);
    assert!(cache.open(&account, source, file, 2).unwrap().is_none());
    assert_eq!(
        cache
            .estimate(&account, &MediaFilter::All)
            .unwrap()
            .selected_bytes,
        0
    );
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
}

/// Сбой SQLite после переименования сразу убирает байты, которые не удалось опубликовать.
#[test]
fn removes_unpublished_file_after_index_failure() {
    let profile = TempDir::new().unwrap();
    let cache = open_profile(&profile, 100);
    let account = owner("a");
    let source = source(10, 1, 5);
    let file = key(7);
    add(&cache, &account, source, file);
    cache
        .connection()
        .unwrap()
        .execute_batch(
            "CREATE TEMP TRIGGER fail_origin_write BEFORE INSERT ON messenger_media_origins
         BEGIN SELECT RAISE(ABORT, 'forced index write failure'); END;",
        )
        .unwrap();
    let fence = cache.prepare(&account, source, file).unwrap();
    let mut staged = cache.stage().unwrap();
    staged.write_all(b"partial transaction").unwrap();
    assert!(matches!(
        cache.publish(fence, staged, MediaMetadata::default(), 1),
        Err(MediaCacheError::Storage(StorageError::Sqlite(_)))
    ));
    assert!(cache.open(&account, source, file, 2).unwrap().is_none());
    assert_eq!(
        fs::read_dir(profile.path().join("media")).unwrap().count(),
        0
    );
}
