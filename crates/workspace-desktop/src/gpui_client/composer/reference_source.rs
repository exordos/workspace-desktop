//! Подключает к подсказкам каталог и справочник в границах открытого разговора.

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{Context, Task};
use uuid::Uuid;
use workspace_core::messenger::{catalog::Chat, details::SidebarDetails};

use super::Composer;

const RETRY_DELAY: Duration = Duration::from_secs(3);

/// Следит за единственным запросом подсказок в текущем разговоре.
#[derive(Default)]
pub(super) struct ReferenceDirectory {
    request: Option<(Uuid, Task<()>)>, // Только результат последней загрузки может обновить поле.
    retry_after: Option<Instant>,      // Следующий ввод может дать повторить неудачный запрос.
    current: bool,                     // Последняя загрузка подтвердила справочник и состав.
}

impl Composer {
    /// Использует тот же снимок доступных чатов, из которого оболочка строит список.
    pub(in crate::gpui_client) fn set_reference_chats(
        &mut self,
        chats: Option<Arc<[Chat]>>,
        cx: &mut Context<Self>,
    ) {
        self.editor
            .update(cx, |editor, cx| editor.set_reference_chats(chats, cx));
    }

    /// Делает состав участников неизвестным до следующего подтверждения сервера.
    pub(in crate::gpui_client) fn refresh_mention_members(
        &mut self,
        revoked: bool,
        cx: &mut Context<Self>,
    ) {
        self.reference_directory = ReferenceDirectory::default();
        self.editor.update(cx, |editor, cx| {
            editor.invalidate_reference_people(revoked, cx)
        });
        if !revoked {
            self.load_mention_directory(cx);
        }
    }

    /// Загружает справочник и состав только после начала запроса @, не передавая текст редактора.
    pub(super) fn load_mention_directory(&mut self, cx: &mut Context<Self>) {
        if self.reference_directory.current
            || self.reference_directory.request.is_some()
            || self
                .reference_directory
                .retry_after
                .is_some_and(|retry| Instant::now() < retry)
            || self.plain_send_topic().is_none()
            || !self.editor.read(cx).is_mention_query()
        {
            return;
        }
        let Some(runtime) = self.draft_runtime.clone() else {
            return;
        };
        let address = self.address.clone();
        let id = Uuid::new_v4();
        let task = cx.spawn(async move |entity, cx| {
            let reply = runtime
                .conversation(address, |services, address| async move {
                    let owner = &address.route.owner;
                    let generation = address.generation;
                    futures::join!(
                        services
                            .messenger
                            .sidebar_directory(&services.sync, owner, generation),
                        services.messenger.sidebar_details(
                            &services.sync,
                            owner,
                            generation,
                            address.route.scope.stream_id,
                        )
                    )
                })
                .await;
            let _ = entity.update(cx, |composer, cx| {
                if composer.address != reply.address
                    || composer
                        .reference_directory
                        .request
                        .as_ref()
                        .map(|(id, _)| *id)
                        != Some(id)
                {
                    return;
                }
                composer.reference_directory.request.take();
                if composer.plain_send_topic().is_none() {
                    return;
                }
                let Ok((directory, details)) = reply.value else {
                    composer.reference_directory.retry_after = Some(Instant::now() + RETRY_DELAY);
                    return;
                };
                composer.reference_directory.current = directory.is_ok() && details.is_ok();
                if !composer.reference_directory.current {
                    composer.reference_directory.retry_after = Some(Instant::now() + RETRY_DELAY);
                }
                let mut people = directory.unwrap_or_default();
                let mut known: HashSet<_> = people.iter().map(|(id, _)| *id).collect();
                let members = match details.as_deref() {
                    Ok(SidebarDetails::Channel(channel)) => Some(
                        channel
                            .members
                            .iter()
                            .map(|member| {
                                if let Some(profile) = &member.profile
                                    && known.insert(member.user_id)
                                {
                                    people.push((member.user_id, profile.clone()));
                                }
                                member.user_id
                            })
                            .collect(),
                    ),
                    Ok(SidebarDetails::Peer(peer)) => {
                        if known.insert(peer.user_id) {
                            people.push((peer.user_id, peer.profile.clone()));
                        }
                        None
                    }
                    Err(_) => None,
                };
                composer.editor.update(cx, |editor, cx| {
                    editor.set_reference_people(people, members, cx)
                });
            });
        });
        self.reference_directory.request = Some((id, task));
    }
}
