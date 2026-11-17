//! Хранит загруженные файлы аватаров в пределах текущего входа и ограничивает объём памяти.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::{
    auth::Service,
    context::{OwnerContext, SessionGeneration},
    sync::Sync,
};

use super::{DetailsError, MessengerSource};

const ENTRY_LIMIT: usize = 128;
const BYTE_LIMIT: usize = 64 * 1024 * 1024;

/// Форматы, которые Workspace принимает при загрузке собственного аватара.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarAvatarFormat {
    Png,  // PNG.
    Jpeg, // JPEG.
    Gif,  // GIF.
    Webp, // WebP.
}

/// Загруженный файл аватара с проверенным MIME.
#[derive(Debug)]
pub struct SidebarAvatar {
    pub format: SidebarAvatarFormat, // Формат для декодера GPUI.
    pub bytes: Vec<u8>,              // Содержимое файла в пределах серверного лимита.
}

type AvatarKey = (OwnerContext, SessionGeneration, Uuid);
type AvatarEntry = Arc<OnceCell<Arc<SidebarAvatar>>>;

/// Объединяет загрузки одного файла. Старые файлы вытесняются при достижении лимита.
#[derive(Default)]
pub(crate) struct AvatarCache {
    entries: Mutex<VecDeque<(AvatarKey, AvatarEntry)>>, // Последнее обращение находится в конце.
}

impl AvatarCache {
    /// Возвращает запись файла и отмечает, что к ней обращались последней.
    fn entry(&self, owner: &OwnerContext, generation: SessionGeneration, id: Uuid) -> AvatarEntry {
        let mut entries = self.entries.lock().expect("avatar cache");
        entries.retain(|((context, session, _), _)| context != owner || *session == generation);
        let key = (owner.clone(), generation, id);
        let entry = entries
            .iter()
            .position(|(current, _)| current == &key)
            .and_then(|position| entries.remove(position))
            .map_or_else(AvatarEntry::default, |(_, entry)| entry);
        entries.push_back((key, entry.clone()));
        entry
    }

    /// Освобождает самые старые завершённые загрузки сверх лимита записи и байтов.
    fn trim(&self) {
        let mut entries = self.entries.lock().expect("avatar cache");
        let mut bytes = entries
            .iter()
            .filter_map(|(_, entry)| entry.get())
            .map(|avatar| avatar.bytes.len())
            .sum::<usize>();
        while entries.len() > ENTRY_LIMIT || bytes > BYTE_LIMIT {
            let Some(position) = entries
                .iter()
                .position(|(_, entry)| Arc::strong_count(entry) == 1)
            else {
                break;
            };
            if let Some((_, entry)) = entries.remove(position) {
                bytes -= entry.get().map_or(0, |avatar| avatar.bytes.len());
            }
        }
    }

    /// Удаляет ссылки на загруженные изображения завершившейся сессии.
    pub(crate) fn clear_session(&self, owner: &OwnerContext, generation: SessionGeneration) {
        self.entries
            .lock()
            .expect("avatar cache")
            .retain(|((context, session, _), _)| context != owner || *session != generation);
    }
}

impl MessengerSource {
    /// Загружает аватар через Workspace API и выдаёт байты только пока действует исходная сессия.
    pub async fn sidebar_avatar(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        file_id: Uuid,
    ) -> Result<Arc<SidebarAvatar>, DetailsError> {
        let auth = sync.auth();
        let entry = auth.with_active_generation(owner, generation, || {
            sync.avatar_cache.entry(owner, generation, file_id)
        })?;
        let result = entry
            .get_or_try_init(|| async {
                let token = auth
                    .access_token_for_generation(owner, Service::Messenger, generation)
                    .await?;
                let image = self.rest.sidebar_avatar(owner, &token, file_id).await?;
                auth.with_active_generation(owner, generation, || Arc::new(image))
                    .map_err(DetailsError::from)
            })
            .await
            .cloned();
        drop(entry);
        sync.avatar_cache.trim();
        auth.with_active_generation(owner, generation, || ())?;
        result
    }
}
