//! Хранит дневные счётчики быстрых реакций отдельно для каждого владельца.

use rusqlite::{Connection, TransactionBehavior, params};

use crate::{
    context::OwnerContext,
    messenger::quick_reactions::{QuickReactionUsage, ReactionDay},
};

use super::{Storage, StorageError};

/// Создаёт таблицу для новых и уже существующих профилей.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS messenger_quick_reactions (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL,
            day INTEGER NOT NULL, emoji_name TEXT NOT NULL, count INTEGER NOT NULL CHECK (count > 0),
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id, day, emoji_name)
        );",
    )?;
    Ok(())
}

impl Storage {
    /// Читает недавние счётчики, которые можно ранжировать в окне без новых запросов к базе.
    pub(crate) fn quick_reaction_usage(
        &self,
        owner: &OwnerContext,
        first_day: i64,
    ) -> Result<QuickReactionUsage, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut statement = connection.prepare_cached(
            "SELECT day, emoji_name, count FROM messenger_quick_reactions
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND day >= ?6",
        )?;
        let days = statement
            .query_map(
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    first_day
                ],
                |row| {
                    // SQLite хранит знаковый INTEGER, а счётчик реакции всегда положительный.
                    let count: i64 = row.get(2)?;
                    Ok(ReactionDay {
                        day: row.get(0)?,
                        name: row.get(1)?,
                        count: u64::try_from(count)
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(2, count))?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(QuickReactionUsage { days })
    }

    /// Убирает дни за пределами окна и учитывает подтверждённое добавление одной транзакцией.
    pub(crate) fn record_quick_reaction(
        &self,
        owner: &OwnerContext,
        first_day: i64,
        today: i64,
        name: &str,
    ) -> Result<(), StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM messenger_quick_reactions
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND day NOT BETWEEN ?6 AND ?7",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                first_day,
                today
            ],
        )?;
        transaction.execute(
            "INSERT INTO messenger_quick_reactions
                (account_id, server, organization_id, project_id, user_id, day, emoji_name, count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)
             ON CONFLICT DO UPDATE SET count = MIN(count, 9223372036854775806) + 1",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                today,
                name
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }
}
