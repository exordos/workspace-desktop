//! Выдаёт страницы вложений и копий, оценивает место и удаляет байты по выбранной области.

use std::{fs, io, sync::Arc};

use rusqlite::{Connection, TransactionBehavior, params, params_from_iter, types::Value};
use uuid::Uuid;

use super::{
    Disk, MediaCache, MediaCacheError, MediaKey, MediaKind, MediaMetadata, MediaVariant, bump,
    mark_pending, optional_uuid_column, owner_key, scope_key, unsigned_column, uuid_column,
};
use crate::{
    context::OwnerContext,
    storage::{HistoryScope, StorageError},
};

/// Выбирает локальные копии для оценки и удаления.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MediaFilter {
    All,                 // Все копии выбранного владельца.
    Scope(HistoryScope), // Вложения стрима или одной его темы.
    Kind(MediaKind),     // Один вид вложений.
    File(Uuid),          // Все варианты одного файла.
    Files(Vec<Uuid>),    // Все варианты набора файлов.
    Variant(MediaKey),   // Один вариант одного файла.
}

/// Сколько места занимает выбранная область и сколько можно освободить.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MediaEstimate {
    pub selected_bytes: u64,    // Полный размер уникальных выбранных копий.
    pub reclaimable_bytes: u64, // Размер копий, которые можно удалить после закрытия читателей.
    pub shared_bytes: u64,      // Размер копий, оставленных для других областей.
    pub pending_bytes: u64,     // Скрытые версии, у которых байты ещё занимают место.
}

/// Результат очистки выбранных записей.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MediaClearReport {
    pub estimate: MediaEstimate, // Сводка, рассчитанная в транзакции очистки.
    pub freed_bytes: u64,        // Байты файлов, которые действительно удалены.
    pub pending_bytes: u64,      // Байты файлов, ожидающих закрытия или повторной попытки.
}

/// Копии, скрытые одной командой очистки и готовые к физическому удалению.
pub(crate) struct MediaClearWork {
    estimate: MediaEstimate, // Сводка на момент фиксации команды.
    retired: Vec<Selected>,  // Конкретные версии, выбранные для удаления.
    _claims: ClearClaims,    // Защищает итог команды от фонового очистителя.
}

/// Даёт явной команде первой удалить свои версии и точно посчитать результат.
struct ClearClaims {
    disk: Arc<Disk>, // Одна очередь удаления для этого профиля.
    ids: Vec<Uuid>,  // Версии, за которые отвечает явная команда.
}

impl Drop for ClearClaims {
    /// Возвращает невыполненное удаление фоновой очистке.
    fn drop(&mut self) {
        let mut claims = self
            .disk
            .clear_claims
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for id in &self.ids {
            claims.remove(id);
        }
    }
}

/// Результат удаления файлов, которые уже скрыты в индексе.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MediaReapReport {
    pub freed_bytes: u64,   // Файлы успешно удалены с диска.
    pub pending_bytes: u64, // Файлы пока остались на диске.
}

/// Порядок выдачи локальных копий.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaCopySort {
    Largest,      // Сначала самые большие.
    RecentlyUsed, // Сначала недавно открывавшиеся.
}

impl MediaCopySort {
    /// Возвращает имя безопасного столбца, выбранного из перечисления.
    fn column(self) -> &'static str {
        match self {
            Self::Largest => "size",
            Self::RecentlyUsed => "used_at",
        }
    }
}

/// Фильтры и положение текущей страницы сохранённых байтов.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaCopyQuery {
    pub scope: Option<HistoryScope>, // Весь владелец либо конкретный стрим или тема.
    pub kind: Option<MediaKind>,     // Все виды либо указанный вид.
    pub sort: MediaCopySort,         // Порядок результата.
    pub after: Option<MediaCopyCursor>, // Предыдущая граница страницы.
    pub limit: usize,                // Число строк на страницу.
}

/// Стабильная граница страницы, включая уникальный ключ при совпадении размера или времени.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaCopyCursor {
    sort: MediaCopySort, // Порядок, при котором курсор был выдан.
    value: i64,          // Значение сортировки последней строки.
    id: Uuid,            // Идентификатор физической копии для разрешения совпадений.
}

/// Масштаб и метаданные одной локальной копии без системного пути.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaCopy {
    pub key: MediaKey,           // Файл и сохранённый вариант.
    pub kind: MediaKind,         // Вид содержимого для группировки.
    pub size: u64,               // Размер на диске в байтах.
    pub last_used: u64,          // Время последнего открытия в UTC, секунды с Unix epoch.
    pub metadata: MediaMetadata, // Известные сведения о файле.
}

/// Страница инвентаризации сохранённых вариантов.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaCopyPage {
    pub copies: Vec<MediaCopy>,        // Копии на текущей странице.
    pub next: Option<MediaCopyCursor>, // Следующая граница, если есть ещё записи.
}

/// Граница страницы ссылок из истории, которую уже видел Core.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaHistoryCursor {
    created_at: String, // Время сообщения для основной сортировки.
    id: i64,            // Внутренняя версия ссылки для разрешения совпадений времени.
}

/// Запись галереи с размещением и состоянием локальной копии.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaHistoryItem {
    pub scope: HistoryScope, // Стрим и тема исходного сообщения.
    pub message_id: Uuid,    // Исходное сообщение.
    pub file_id: Uuid,       // Файл, на который оно ссылается.
    pub kind: MediaKind,     // Вид прямой ссылки.
    pub created_at: String,  // Серверное время сообщения в UTC.
    pub cached: bool,        // Для этой ссылки есть доступная копия оригинала.
}

/// Страница известных вложений без загрузки байтов.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaHistoryPage {
    pub attachments: Vec<MediaHistoryItem>, // Ссылки на текущей странице.
    pub next: Option<MediaHistoryCursor>,   // Следующая граница, если есть ещё записи.
}

/// Выбранная физическая копия и условия её очистки.
struct Selected {
    id: Uuid,     // Служебный ID именно этой версии.
    size: u64,    // Число байтов в файле.
    shared: bool, // Копия нужна для другой области.
}

impl MediaCache {
    /// Возвращает для владельца страницу известных ссылок в уже подтверждённой области.
    pub(crate) fn list_history(
        &self,
        owner: &OwnerContext,
        scope: HistoryScope,
        kind: Option<MediaKind>,
        after: Option<MediaHistoryCursor>,
        limit: usize,
    ) -> Result<MediaHistoryPage, MediaCacheError> {
        let connection = self.connection()?;
        let limit = page_limit(limit);
        let mut statement = connection.prepare_cached(
            "SELECT ref.id, ref.stream, ref.topic, ref.message, ref.file, ref.kind, ref.created_at,
                 EXISTS(SELECT 1 FROM messenger_media_objects AS copy
                     WHERE copy.owner = ref.owner AND copy.file = ref.file
                       AND copy.variant = 0 AND copy.pending = 0
                       AND (copy.file_stream IS NOT NULL OR EXISTS (
                         SELECT 1 FROM messenger_media_origins AS origin
                         WHERE origin.object = copy.id AND origin.stream = ref.stream)))
             FROM messenger_media_refs AS ref
             WHERE ref.owner = ?1 AND ref.stream = ?2 AND (?3 IS NULL OR ref.topic = ?3)
               AND (?4 IS NULL OR ref.kind = ?4)
               AND (?5 IS NULL OR (ref.created_at, ref.id) < (?5, ?6))
             ORDER BY ref.created_at DESC, ref.id DESC LIMIT ?7",
        )?;
        let mut entries = statement
            .query_map(
                params![
                    owner_key(owner),
                    scope.stream_id.to_string(),
                    scope.topic_id.map(|id| id.to_string()),
                    kind.map(|kind| kind as i64),
                    after.as_ref().map(|cursor| cursor.created_at.as_str()),
                    after.as_ref().map(|cursor| cursor.id),
                    limit as i64 + 1,
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        MediaHistoryItem {
                            scope: HistoryScope {
                                stream_id: uuid_column(row, 1)?,
                                topic_id: optional_uuid_column(row, 2)?,
                            },
                            message_id: uuid_column(row, 3)?,
                            file_id: uuid_column(row, 4)?,
                            kind: MediaKind::read(row.get(5)?)?,
                            created_at: row.get(6)?,
                            cached: row.get(7)?,
                        },
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next = after_page(&mut entries, limit).map(|(id, item)| MediaHistoryCursor {
            id: *id,
            created_at: item.created_at.clone(),
        });
        Ok(MediaHistoryPage {
            attachments: entries.into_iter().map(|(_, item)| item).collect(),
            next,
        })
    }

    /// Выбирает сохранённые копии и сортирует их по индексу без обхода файловой системы.
    pub(crate) fn list(
        &self,
        owner: &OwnerContext,
        query: MediaCopyQuery,
    ) -> Result<MediaCopyPage, MediaCacheError> {
        let connection = self.connection()?;
        let sort_field = query.sort.column();
        let limit = page_limit(query.limit);
        let statement = format!(
            "SELECT object.id, object.file, object.variant, object.kind, object.size,
             object.used_at, object.name, object.mime, object.file_stream
             FROM messenger_media_objects AS object
             WHERE object.owner = ?1 AND object.pending = 0
               AND (?2 IS NULL OR EXISTS (SELECT 1 FROM messenger_media_refs AS ref
                 WHERE ref.owner = object.owner AND ref.file = object.file
                   AND ref.stream = ?2 AND (?3 IS NULL OR ref.topic = ?3)))
               AND (?4 IS NULL OR object.kind = ?4)
               AND (?5 IS NULL OR (object.{sort_field}, object.id) < (?5, ?6))
             ORDER BY object.{sort_field} DESC, object.id DESC LIMIT ?7"
        );
        let mut statement = connection.prepare_cached(&statement)?;
        let after = query.after.filter(|cursor| cursor.sort == query.sort);
        let mut entries = statement
            .query_map(
                params![
                    owner_key(owner),
                    query
                        .scope
                        .as_ref()
                        .map(|scope| scope.stream_id.to_string()),
                    query
                        .scope
                        .and_then(|scope| scope.topic_id.map(|id| id.to_string())),
                    query.kind.map(|kind| kind as i64),
                    after.as_ref().map(|cursor| cursor.value),
                    after.as_ref().map(|cursor| cursor.id.to_string()),
                    limit as i64 + 1
                ],
                |row| {
                    let variant = match row.get::<_, i64>(2)? {
                        0 => MediaVariant::Original,
                        value => return Err(rusqlite::Error::IntegralValueOutOfRange(2, value)),
                    };
                    Ok((
                        uuid_column(row, 0)?,
                        MediaCopy {
                            key: MediaKey {
                                file_id: uuid_column(row, 1)?,
                                variant,
                            },
                            kind: MediaKind::read(row.get(3)?)?,
                            size: unsigned_column(row, 4)?,
                            last_used: unsigned_column(row, 5)?,
                            metadata: MediaMetadata {
                                name: row.get(6)?,
                                mime: row.get(7)?,
                                file_stream: optional_uuid_column(row, 8)?,
                            },
                        },
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next = after_page(&mut entries, limit).map(|(id, copy)| MediaCopyCursor {
            sort: query.sort,
            value: match query.sort {
                MediaCopySort::Largest => copy.size as i64,
                MediaCopySort::RecentlyUsed => copy.last_used as i64,
            },
            id: *id,
        });
        Ok(MediaCopyPage {
            copies: entries.into_iter().map(|(_, copy)| copy).collect(),
            next,
        })
    }

    /// Оценивает объём по текущему индексу, не меняя запись и не открывая файлы.
    pub(crate) fn estimate(
        &self,
        owner: &OwnerContext,
        filter: &MediaFilter,
    ) -> Result<MediaEstimate, MediaCacheError> {
        let connection = self.connection()?;
        let owner = owner_key(owner);
        Ok(estimate(
            &select(&connection, &owner, filter, false)?,
            &select(&connection, &owner, filter, true)?,
        ))
    }

    /// Скрывает выбранные копии в SQLite, не обращаясь к файловой системе.
    pub(crate) fn begin_clear(
        &self,
        owner: &OwnerContext,
        filter: &MediaFilter,
    ) -> Result<MediaClearWork, MediaCacheError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owner = owner_key(owner);
        let selected = select(&transaction, &owner, filter, false)?;
        let estimate = estimate(&selected, &select(&transaction, &owner, filter, true)?);
        // Версия захватывает также промахи, которые ещё ничего не записали в индекс.
        match filter {
            MediaFilter::Scope(scope) => {
                bump(
                    &transaction,
                    &owner,
                    &scope_key(scope.stream_id, scope.topic_id),
                )?;
            }
            MediaFilter::File(file_id) | MediaFilter::Variant(MediaKey { file_id, .. }) => {
                bump(&transaction, &owner, &file_id.to_string())?;
            }
            MediaFilter::Files(files) => {
                for file in files {
                    bump(&transaction, &owner, &file.to_string())?;
                }
            }
            _ => bump(&transaction, &owner, "")?,
        }
        let retired = selected
            .into_iter()
            .filter(|item| !item.shared)
            .collect::<Vec<_>>();
        for item in &retired {
            mark_pending(&transaction, item.id)?;
        }
        // Резервируем версии до фиксации, пока фоновая задача ещё не может выбрать их из базы.
        let claims = ClearClaims {
            disk: self.disk.clone(),
            ids: retired.iter().map(|item| item.id).collect(),
        };
        self.disk
            .clear_claims
            .lock()
            .map_err(|_| StorageError::Poisoned)?
            .extend(&claims.ids);
        transaction.commit()?;
        Ok(MediaClearWork {
            estimate,
            retired,
            _claims: claims,
        })
    }

    /// Удаляет ранее скрытые версии и возвращает фактический результат.
    pub(crate) fn finish_clear(
        &self,
        work: MediaClearWork,
    ) -> Result<MediaClearReport, MediaCacheError> {
        let reaped = self.reap_selected(&work.retired, false)?;
        Ok(MediaClearReport {
            estimate: work.estimate,
            freed_bytes: reaped.freed_bytes,
            pending_bytes: reaped.pending_bytes,
        })
    }

    /// В тестах выполняет обе части очистки последовательно.
    #[cfg(test)]
    pub(crate) fn clear(
        &self,
        owner: &OwnerContext,
        filter: &MediaFilter,
    ) -> Result<MediaClearReport, MediaCacheError> {
        self.finish_clear(self.begin_clear(owner, filter)?)
    }

    /// Повторяет удаление физических копий после отзыва, рестарта или сбоя диска.
    pub(crate) fn reap(&self) -> Result<MediaReapReport, MediaCacheError> {
        let connection = self.connection()?;
        let pending = read_selected(
            &connection,
            "SELECT id, size, 0 FROM messenger_media_objects WHERE pending = 1",
            [],
        )?;
        drop(connection);
        self.reap_selected(&pending, true)
    }

    /// Вытесняет давно не использованные версии, пока кэш не уложится в бюджет.
    pub(crate) fn enforce_budget(&self) -> Result<MediaReapReport, MediaCacheError> {
        let mut report = self.reap()?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut current: u64 = transaction.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM messenger_media_objects",
            [],
            |row| unsigned_column(row, 0),
        )?;
        let budget = self.disk.policy.budget_bytes;
        if current <= budget {
            return Ok(report);
        }
        let candidates = read_selected(
            &transaction,
            "SELECT id, size, 0 FROM messenger_media_objects WHERE pending = 0 ORDER BY used_at, id",
            [],
        )?;
        let readers = self
            .disk
            .readers
            .lock()
            .map_err(|_| StorageError::Poisoned)?;
        let mut retired = Vec::new();
        for candidate in candidates {
            if current <= budget {
                break;
            }
            if readers.contains_key(&candidate.id) {
                continue;
            }
            mark_pending(&transaction, candidate.id)?;
            current -= candidate.size;
            retired.push(candidate);
        }
        transaction.commit()?;
        drop(readers);
        drop(connection);
        let result = self.reap_selected(&retired, true)?;
        report.freed_bytes += result.freed_bytes;
        report.pending_bytes += result.pending_bytes;
        Ok(report)
    }

    /// Удаляет только указанные физические версии, сохраняя активные дескрипторы и новые копии.
    fn reap_selected(
        &self,
        selected: &[Selected],
        skip_claimed: bool,
    ) -> Result<MediaReapReport, MediaCacheError> {
        let mut report = MediaReapReport::default();
        for item in selected {
            if skip_claimed
                && self
                    .disk
                    .clear_claims
                    .lock()
                    .map_err(|_| StorageError::Poisoned)?
                    .contains(&item.id)
            {
                report.pending_bytes += item.size;
                continue;
            }
            let is_leased = self
                .disk
                .readers
                .lock()
                .map_err(|_| StorageError::Poisoned)?
                .contains_key(&item.id);
            if is_leased {
                report.pending_bytes += item.size;
                continue;
            }
            let path = self.disk.object_path(item.id);
            let disk_size =
                fs::symlink_metadata(&path).map_or(item.size, |metadata| metadata.len());
            match fs::remove_file(path) {
                Ok(()) => report.freed_bytes += disk_size,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {
                    report.pending_bytes += item.size;
                    continue;
                }
            }
            self.connection()?.execute(
                "DELETE FROM messenger_media_objects WHERE id = ?1 AND pending = 1",
                [item.id.to_string()],
            )?;
        }
        Ok(report)
    }
}

/// Рассчитывает итог по уникальным физическим версиям.
fn estimate(selected: &[Selected], pending: &[Selected]) -> MediaEstimate {
    let mut result = MediaEstimate {
        pending_bytes: pending.iter().map(|item| item.size).sum(),
        ..MediaEstimate::default()
    };
    for item in selected {
        result.selected_bytes += item.size;
        if item.shared {
            result.shared_bytes += item.size;
        } else {
            result.reclaimable_bytes += item.size;
        }
    }
    result
}

/// Выбирает копии для фильтра, сохраняя общие байты при групповой очистке.
fn select(
    connection: &Connection,
    owner: &str,
    filter: &MediaFilter,
    pending: bool,
) -> Result<Vec<Selected>, MediaCacheError> {
    let mut bind: Vec<Value> = vec![owner.to_owned().into()];
    let (shared, predicate) = match filter {
        MediaFilter::All => ("0", ""),
        MediaFilter::Scope(scope) => {
            bind.push(scope.stream_id.to_string().into());
            bind.push(
                scope
                    .topic_id
                    .map(|id| id.to_string())
                    .map_or(Value::Null, Value::Text),
            );
            (
                "EXISTS (SELECT 1 FROM messenger_media_refs AS ref
                 WHERE ref.owner = object.owner AND ref.file = object.file
                 AND (ref.stream <> ?2 OR (?3 IS NOT NULL AND ref.topic IS NOT ?3)))",
                "AND EXISTS (SELECT 1 FROM messenger_media_refs AS ref
                 WHERE ref.owner = object.owner AND ref.file = object.file
                 AND ref.stream = ?2 AND (?3 IS NULL OR ref.topic = ?3))",
            )
        }
        MediaFilter::Kind(kind) => {
            bind.push((*kind as i64).into());
            ("0", "AND kind = ?2")
        }
        MediaFilter::File(file) => {
            bind.push(file.to_string().into());
            ("0", "AND file = ?2")
        }
        MediaFilter::Files(files) => {
            bind.push(
                serde_json::to_string(files)
                    .expect("UUID сериализуется в строку")
                    .into(),
            );
            ("0", "AND file IN (SELECT value FROM json_each(?2))")
        }
        MediaFilter::Variant(key) => {
            bind.push(key.file_id.to_string().into());
            bind.push((key.variant as i64).into());
            ("0", "AND file = ?2 AND variant = ?3")
        }
    };
    let pending = u8::from(pending);
    let sql = format!(
        "SELECT id, size, {shared} FROM messenger_media_objects AS object
         WHERE owner = ?1 AND pending = {pending} {predicate}"
    );
    Ok(read_selected(connection, &sql, params_from_iter(bind))?)
}

/// Читает размеры и идентификаторы физических версий из SQLite.
fn read_selected(
    connection: &Connection,
    sql: &str,
    bind: impl rusqlite::Params,
) -> rusqlite::Result<Vec<Selected>> {
    let mut statement = connection.prepare_cached(sql)?;
    statement
        .query_map(bind, |row| {
            Ok(Selected {
                id: uuid_column(row, 0)?,
                size: unsigned_column(row, 1)?,
                shared: row.get(2)?,
            })
        })?
        .collect()
}

/// Нормализует размер страницы до поддерживаемого диапазона.
fn page_limit(requested: usize) -> usize {
    requested.clamp(1, 200)
}

/// Убирает строку, запрошенную для определения наличия следующей страницы.
fn after_page<T>(entries: &mut Vec<T>, limit: usize) -> Option<&T> {
    if entries.len() <= limit {
        return None;
    }
    entries.truncate(limit);
    entries.last()
}
