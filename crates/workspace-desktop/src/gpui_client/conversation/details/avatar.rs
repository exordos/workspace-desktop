//! Загружает фото открытой панели и связывает его с исходным разговором.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use futures::{StreamExt, stream};
use gpui::{Context, Image, ImageFormat, ImageSource, RenderImage, Task};
use uuid::Uuid;
use workspace_core::{
    directory::UserProfile,
    messenger::details::{SidebarAvatarFormat, SidebarDetails},
};

use super::Conversation;
use crate::gpui_client::{
    contracts::{ConversationAddress, ConversationReply},
    runtime::RuntimeClient,
};

const PARALLEL_DOWNLOADS: usize = 4;
const DECODED_BYTE_LIMIT: usize = 64 * 1024 * 1024;

/// Фотографии текущей карточки. Закрытие панели освобождает декодированные данные.
#[derive(Default)]
pub(super) struct Avatars {
    ready: HashMap<Uuid, Option<Arc<RenderImage>>>, // None означает, что остались инициалы.
    waiting: HashSet<Uuid>,                         // Идентификаторы незавершённых запросов.
    decoded_bytes: usize,                           // Объём сохранённых пикселей.
    request: Option<Task<()>>,                      // Ожидание текущего набора файлов.
}

impl Avatars {
    /// Возвращает готовый источник фотографии, если загрузка и декодирование завершились.
    pub(super) fn image(&self, profile: Option<&UserProfile>) -> Option<ImageSource> {
        let id = profile?.avatar_file_id()?;
        self.ready.get(&id)?.clone().map(ImageSource::Render)
    }

    /// Меняет очередь только при смене видимых фотографий.
    pub(super) fn refresh(
        &mut self,
        details: &SidebarDetails,
        address: ConversationAddress,
        runtime: RuntimeClient,
        cx: &mut Context<Conversation>,
    ) {
        let required: HashSet<_> = match details {
            SidebarDetails::Peer(peer) => peer.profile.avatar_file_id().into_iter().collect(),
            SidebarDetails::Channel(channel) => channel
                .members
                .iter()
                .filter_map(|member| member.profile.as_ref()?.avatar_file_id())
                .collect(),
        };
        self.ready.retain(|id, _| required.contains(id));
        self.decoded_bytes = self.ready.values().flatten().map(|image| size(image)).sum();
        let missing = required
            .into_iter()
            .filter(|id| !self.ready.contains_key(id))
            .collect::<HashSet<_>>();
        if missing == self.waiting {
            return;
        }
        self.waiting = missing.clone();
        let background = cx.background_executor().clone();
        let renderer = cx.svg_renderer();
        self.request = (!missing.is_empty()).then(|| {
            cx.spawn(async move |view, cx| {
                let mut requests = stream::iter(missing)
                    .map(|id| load(runtime.clone(), address.clone(), id))
                    .buffer_unordered(PARALLEL_DOWNLOADS);
                while let Some(reply) = requests.next().await {
                    let ConversationReply {
                        address,
                        value: (id, image),
                    } = reply;
                    let image = if let Some(image) = image {
                        let renderer = renderer.clone();
                        background
                            .spawn(async move {
                                image
                                    .to_image_data(renderer)
                                    .ok()
                                    .filter(|image| image.frame_count() != 0)
                            })
                            .await
                    } else {
                        None
                    };
                    let _ = view.update(cx, |view, cx| {
                        if view.address != address {
                            return;
                        }
                        if let Some(panel) = view.details.as_mut()
                            && panel.state.is_some()
                            && panel.avatars.waiting.remove(&id)
                        {
                            panel.avatars.remember(id, image);
                            cx.notify();
                        }
                    });
                }
            })
        });
    }

    /// Сохраняет готовое фото в пределах бюджета панели. При сбое оставляет инициалы.
    fn remember(&mut self, id: Uuid, image: Option<Arc<RenderImage>>) {
        let image = image.filter(|image| {
            let bytes = size(image);
            if bytes > DECODED_BYTE_LIMIT - self.decoded_bytes {
                return false;
            }
            self.decoded_bytes += bytes;
            true
        });
        self.ready.insert(id, image);
    }
}

/// Получает файл под тем же поколением входа, что и видимая карточка.
async fn load(
    runtime: RuntimeClient,
    address: ConversationAddress,
    id: Uuid,
) -> ConversationReply<(Uuid, Option<Image>)> {
    let reply = runtime
        .conversation(address, move |services, address| async move {
            services
                .messenger
                .sidebar_avatar(&services.sync, &address.route.owner, address.generation, id)
                .await
        })
        .await;
    ConversationReply {
        address: reply.address,
        value: (
            id,
            reply.value.ok().and_then(Result::ok).map(|avatar| {
                let format = match avatar.format {
                    SidebarAvatarFormat::Png => ImageFormat::Png,
                    SidebarAvatarFormat::Jpeg => ImageFormat::Jpeg,
                    SidebarAvatarFormat::Gif => ImageFormat::Gif,
                    SidebarAvatarFormat::Webp => ImageFormat::Webp,
                };
                Image::from_bytes(format, avatar.bytes.clone())
            }),
        ),
    }
}

/// Считает байты декодированных кадров, включая кадры GIF.
fn size(image: &RenderImage) -> usize {
    (0..image.frame_count())
        .filter_map(|frame| image.as_bytes(frame))
        .map(<[u8]>::len)
        .sum()
}
