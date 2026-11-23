//! Хранит версии доступности исходных чатов для открытых защищённых фрагментов.

use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, RwLock, Weak},
};

use uuid::Uuid;

use crate::context::OwnerContext;

type Versions<K> = HashMap<OwnerContext, HashMap<K, Weak<()>>>;

pub(super) type FragmentOrigins = CommittedOrigins<Uuid>;
pub(crate) type FragmentOriginsPermit = OriginsPermit<Uuid>;

/// Версии действующего профиля. Событие отзыва меняет их вместе с локальной записью.
pub(super) struct CommittedOrigins<K>(Arc<RwLock<Versions<K>>>); // Живая таблица разрешений владельца.

/// Привязывает работу к доступу во всех нужных исходных чатах.
pub(crate) struct OriginsPermit<K> {
    registry: Weak<RwLock<Versions<K>>>, // Текущие версии профиля без удержания синхронизации.
    owner: OwnerContext,                 // Владелец каждого исходного чата.
    scopes: Vec<(K, Arc<()>)>,           // Область и уникальная версия до следующего отзыва.
}

impl<K> Default for CommittedOrigins<K> {
    /// Начинает с пустых разрешений профиля.
    fn default() -> Self {
        Self(Arc::default())
    }
}

impl<K: Clone + Eq + Hash> CommittedOrigins<K> {
    /// Запоминает точные исходные области под текущим входом.
    pub(super) fn capture(&self, owner: &OwnerContext, streams: &[K]) -> OriginsPermit<K> {
        let mut registry = self.0.write().unwrap();
        registry.retain(|_, streams| {
            streams.retain(|_, version| version.strong_count() != 0);
            !streams.is_empty()
        });
        let current = registry.entry(owner.clone()).or_default();
        let scopes = streams
            .iter()
            .cloned()
            .map(|stream| {
                let version = current.entry(stream.clone()).or_default();
                let token = version.upgrade().unwrap_or_else(|| {
                    let token = Arc::new(());
                    *version = Arc::downgrade(&token);
                    token
                });
                (stream, token)
            })
            .collect();
        OriginsPermit {
            registry: Arc::downgrade(&self.0),
            owner: owner.clone(),
            scopes,
        }
    }

    /// Согласует отдельные затронутые разрешения с завершившейся записью.
    pub(super) fn commit_matching<T, E>(
        &self,
        owner: &OwnerContext,
        write: impl FnOnce() -> Result<T, E>,
        mut revoked: impl FnMut(&K, &T) -> bool,
    ) -> Result<T, E> {
        let mut registry = self.0.write().unwrap();
        let result = write()?;
        if let Some(versions) = registry.get_mut(owner) {
            versions.retain(|key, version| version.strong_count() != 0 && !revoked(key, &result));
        }
        Ok(result)
    }
}

impl FragmentOrigins {
    /// Снимает исходный стрим фрагмента только после записи отзыва.
    pub(super) fn commit<T, E>(
        &self,
        owner: &OwnerContext,
        stream: Option<Uuid>,
        write: impl FnOnce() -> Result<T, E>,
        revoked: impl Fn(&T) -> bool,
    ) -> Result<T, E> {
        self.commit_matching(owner, write, |candidate, result| {
            revoked(result) && stream.is_none_or(|stream| *candidate == stream)
        })
    }
}

impl FragmentOriginsPermit {
    /// Возвращает исходные чаты, которые связывают чтение с этим разрешением.
    pub(crate) fn streams(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.scopes.iter().map(|(stream, _)| *stream)
    }
}

impl<K: Eq + Hash> OriginsPermit<K> {
    /// Выполняет чтение до следующего отзыва любого исходного чата.
    pub(crate) fn with_current<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        let registry = self.registry.upgrade()?;
        let versions = registry.read().unwrap();
        let current = versions.get(&self.owner)?;
        self.scopes
            .iter()
            .all(|(stream, version)| {
                current
                    .get(stream)
                    .is_some_and(|present| present.ptr_eq(&Arc::downgrade(version)))
            })
            .then(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::AccountId;

    /// Отзывает доступ ко всем зависимым чтениям, оставляя соседние чаты и владельцев.
    #[test]
    fn one_origin_revokes_mixed_edit_without_affecting_unrelated_message() {
        let origins = FragmentOrigins::default();
        let owner = OwnerContext {
            account_id: AccountId("self".into()),
            server: "https://workspace.test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        };
        let other = OwnerContext {
            account_id: AccountId("other".into()),
            ..owner.clone()
        };
        let native = Uuid::new_v4();
        let linked = Uuid::new_v4();
        let mixed = origins.capture(&owner, &[native, linked]);
        let ordinary = origins.capture(&owner, &[native]);
        let same_elsewhere = origins.capture(&other, &[linked]);
        origins
            .commit(
                &owner,
                Some(linked),
                || Ok::<_, ()>(false),
                |applied| *applied,
            )
            .unwrap();
        assert!(mixed.with_current(|| ()).is_some());
        origins
            .commit(
                &owner,
                Some(linked),
                || Ok::<_, ()>(true),
                |applied| *applied,
            )
            .unwrap();
        assert!(mixed.with_current(|| ()).is_none());
        assert!(ordinary.with_current(|| ()).is_some());
        assert!(same_elsewhere.with_current(|| ()).is_some());
        assert!(
            origins
                .capture(&owner, &[linked])
                .with_current(|| ())
                .is_some()
        );
        assert!(mixed.with_current(|| ()).is_none());
    }
}
