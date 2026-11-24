//! Преобразует подтверждённые данные Messenger в видимые сведения и разрешённые кнопки.

use std::collections::HashMap;

use gpui::{App, ImageSource, SharedString, rgb};
use uuid::Uuid;
use workspace_core::{
    directory::{ProfilePresence, UserProfile},
    messenger::details::{
        ChannelSidebar, DirectSidebar, SidebarDetails, SidebarMember, SidebarTopic,
        StreamNotificationMode, StreamRole, TopicNotificationMode, sidebar_profile_link,
    },
};

use super::{
    MutationKind, Panel, State,
    view::{
        Availability, AvatarView, ChannelView, MemberView, NotificationLevel, NotificationsView,
        PanelContent, PeerActions, PeerView, ProfileField, SelectedTopicView, TopicView,
    },
};
use crate::gpui_client::{
    contracts::ConversationAddress,
    preferences::{Language, Preferences, appearance::SidebarSort},
    sidebar::order::sort_topics,
    ui::{AvatarPresence, Icon, profile_joined_date},
};

/// Показывает обычные темы перед приглушёнными, используя активный режим и превью каталога.
fn panel_topics<'a>(
    panel: &Panel,
    channel: &'a ChannelSidebar,
    address: &ConversationAddress,
    app: &App,
) -> Vec<&'a SidebarTopic> {
    let mode = Preferences::read_for(app, &address.route.owner)
        .appearance
        .sort;
    let positions: HashMap<_, _> = panel
        .catalog
        .as_deref()
        .and_then(|chats| {
            chats
                .iter()
                .find(|chat| chat.id == address.route.scope.stream_id)
        })
        .map(|chat| {
            sort_topics(chat, mode)
                .into_iter()
                .enumerate()
                .map(|(index, topic)| (topic.id, index))
                .collect()
        })
        .unwrap_or_default();
    let mut topics: Vec<_> = channel.topics.iter().filter(|topic| !topic.done).collect();
    topics.sort_by_key(|topic| {
        let muted = channel_topic_muted(channel, topic);
        let unread_first =
            mode == SidebarSort::UnreadFirst && !muted && topic.active_unread_count > 0;
        (
            muted,
            !unread_first,
            positions.get(&topic.id).copied().unwrap_or(usize::MAX),
        )
    });
    topics
}

/// Наследует режим уведомлений канала, если тема не переопределяет его явно.
fn channel_topic_muted(channel: &ChannelSidebar, topic: &SidebarTopic) -> bool {
    topic.notification_mode == TopicNotificationMode::Mute
        || (topic.notification_mode == TopicNotificationMode::Default
            && channel.notification_mode == StreamNotificationMode::Muted)
}

/// Подбирает одно из двух представлений для выбранного адреса.
pub(super) fn content(
    panel: &Panel,
    details: &SidebarDetails,
    address: &ConversationAddress,
    app: &App,
    peer_call: Option<bool>,
) -> PanelContent {
    match details {
        SidebarDetails::Peer(peer) => {
            PanelContent::Peer(peer_content(panel, peer, address, app, peer_call))
        }
        SidebarDetails::Channel(channel) => {
            PanelContent::Channel(Box::new(channel_content(panel, channel, address, app)))
        }
    }
}

/// Собирает сведения профиля и кнопки для своего и чужого профиля.
fn peer_content(
    panel: &Panel,
    peer: &DirectSidebar,
    address: &ConversationAddress,
    app: &App,
    peer_call: Option<bool>,
) -> PeerView {
    let language = Preferences::read_for(app, &address.route.owner).language;
    let share = if sidebar_profile_link(&address.route.owner, peer.user_id).is_ok() {
        Availability::Enabled
    } else {
        Availability::Disabled
    };
    let actions = if peer.is_self {
        PeerActions {
            share,
            share_copied: panel.share_feedback.is_some(),
            favorites: Availability::Enabled,
            edit: Availability::Enabled,
            ..PeerActions::default()
        }
    } else {
        PeerActions {
            message: Availability::Enabled,
            call: available(true, peer_call == Some(true)),
            share,
            share_copied: panel.share_feedback.is_some(),
            ..PeerActions::default()
        }
    };
    let mut card = peer_card(
        peer.user_id,
        &peer.profile,
        panel.avatars.image(peer.user_id, app),
        language,
    );
    card.copied_fields = panel.copy_feedback.keys().copied().collect();
    card.actions = actions;
    card
}

/// Готовит общие поля карточки собеседника до подключения действий экрана.
pub(super) fn peer_card(
    user_id: Uuid,
    profile: &UserProfile,
    image: Option<ImageSource>,
    language: Language,
) -> PeerView {
    let mut avatar = avatar(&profile.display_name, Some(profile), image);
    if profile.presence == Some(ProfilePresence::Offline) {
        avatar.presence = AvatarPresence::Offline;
    } else if profile.presence == Some(ProfilePresence::DoNotDisturb) {
        avatar.presence = AvatarPresence::Away;
    }
    PeerView {
        name: profile.display_name.clone().into(),
        avatar,
        custom_status: profile.status_label().map(Into::into),
        presence: profile.presence,
        fields: vec![
            field(
                language.tr("info.userId"),
                Icon::AlternateEmail,
                Some(user_id.to_string()),
            ),
            field(
                language.tr("common.email"),
                Icon::Envelope,
                profile.email.clone(),
            ),
            field(language.tr("info.phone"), Icon::Phone, None),
            field(
                language.tr("info.jobTitle"),
                Icon::BusinessCenter,
                None,
            ),
            field(
                language.tr("info.manager"),
                Icon::Handshake,
                None,
            ),
            field(language.tr("info.role"), Icon::Account, None),
            field(
                language.tr("info.accountType"),
                Icon::Group,
                None,
            ),
            field(
                language.tr("info.accountStatus"),
                Icon::Info,
                None,
            ),
            field(
                language.tr("info.timezone"),
                Icon::GlobeLocation,
                None,
            ),
            field(
                language.tr("info.localTime"),
                Icon::Pending,
                None,
            ),
            field(
                language.tr("info.joined"),
                Icon::Calendar,
                profile
                    .created_at
                    .as_deref()
                    .and_then(|date| profile_joined_date(date, language)),
            ),
            field(language.tr("info.birthday"), Icon::Celebration, None),
        ],
        language,
        ..PeerView::default()
    }
}

/// Готовит строку профиля и разрешает копирование для известного значения.
fn field(label: &'static str, icon: Icon, value: Option<String>) -> ProfileField {
    let copy = if value.is_some() {
        Availability::Enabled
    } else {
        Availability::Hidden
    };
    ProfileField {
        label: label.into(),
        icon,
        value: value.unwrap_or_default(),
        multiline: false,
        copy,
    }
}

/// Готовит список тем, участников и режим уведомлений канала.
pub(super) fn channel_content(
    panel: &Panel,
    channel: &ChannelSidebar,
    address: &ConversationAddress,
    app: &App,
) -> ChannelView {
    let can_change = matches!(panel.state, Some(State::Ready)) && panel.mutation.is_none();
    let current_topic = address.route.scope.topic_id;
    let language = Preferences::read_for(app, &address.route.owner).language;
    ChannelView {
        name: channel.name.clone().into(),
        language,
        avatar: avatar(&channel.name, None, None),
        color: channel
            .color
            .filter(|color| *color <= 0xFF_FF_FF)
            .map(|color| rgb(color).into()),
        description: channel.description.clone().map(Into::into),
        member_count: channel.members.len(),
        online_count: channel
            .members
            .iter()
            .filter(|member| {
                member
                    .profile
                    .as_ref()
                    .is_some_and(|profile| profile.presence == Some(ProfilePresence::Active))
            })
            .count(),
        topics_expanded: panel.topics_expanded,
        topics: panel_topics(panel, channel, address, app)
            .into_iter()
            .filter_map(|topic| {
                Some(TopicView {
                    id: topic.id,
                    focus: panel.topic_menu.focus(topic.id)?.clone(),
                    title: topic.name.clone().into(),
                    selected: current_topic == Some(topic.id),
                    muted: Some(channel_topic_muted(channel, topic)),
                    unread: u64::from(if topic.active_unread_count > 0 {
                        topic.active_unread_count
                    } else {
                        topic.passive_unread_count
                    }),
                    unread_muted: topic.active_unread_count == 0 && topic.passive_unread_count > 0,
                    mentioned: false,
                })
            })
            .collect(),
        topic_error: (panel.mutation_error == Some(MutationKind::Topic)).then(|| {
            language
                .tr("desktop.conversation.details.couldNotUpdateTheTopic")
                .into()
        }),
        selected_topic: channel
            .topics
            .iter()
            .find(|topic| !channel.encrypted && Some(topic.id) == current_topic)
            .map(|topic| SelectedTopicView {
                title: topic.name.clone().into(),
                done: topic.done,
                summary: panel.summary_content.clone(),
                expanded: panel.summary_expanded.unwrap_or(topic.summary.is_some()),
                summary_enabled: topic.summary_enabled,
                summary_has_new_messages: topic.summary_has_new_messages,
                summary_settings: channel
                    .can_edit_summary(topic.id)
                    .then(|| panel.summary_settings.clone()),
            }),
        notifications: Some(NotificationsView {
            level: match channel.notification_mode {
                StreamNotificationMode::AllMessages => NotificationLevel::Subscribed,
                StreamNotificationMode::MentionsOnly => NotificationLevel::Default,
                StreamNotificationMode::Muted => NotificationLevel::Muted,
            },
            editable: can_change,
            error: (panel.mutation_error == Some(MutationKind::Notifications)).then(|| {
                language
                    .tr("desktop.conversation.details.couldNotUpdateNotifications")
                    .into()
            }),
        }),
        members: channel
            .members
            .iter()
            .map(|member| {
                member_view(
                    member,
                    can_change,
                    panel.avatars.image(member.user_id, app),
                    language,
                )
            })
            .collect(),
        add_members: available(channel.can_add_members, can_change),
        add_members_focus: Some(panel.add_members_focus.clone()),
        member_error: (panel.mutation_error == Some(MutationKind::Remove)).then(|| {
            language
                .tr("desktop.conversation.details.couldNotRemoveTheMember")
                .into()
        }),
    }
}

/// Преобразует выбранный видимый уровень в режим Workspace.
pub(super) fn stream_mode(level: NotificationLevel) -> StreamNotificationMode {
    match level {
        NotificationLevel::Default => StreamNotificationMode::MentionsOnly,
        NotificationLevel::Muted => StreamNotificationMode::Muted,
        NotificationLevel::Subscribed => StreamNotificationMode::AllMessages,
    }
}

/// Представляет привязку даже до появления её профиля в справочнике.
fn member_view(
    member: &SidebarMember,
    can_change: bool,
    image: Option<ImageSource>,
    language: Language,
) -> MemberView {
    let profile = member.profile.as_ref();
    let name = profile
        .map(|profile| profile.display_name.clone())
        .unwrap_or_else(|| member.user_id.to_string());
    let secondary = profile.and_then(|profile| {
        presence_label(profile.presence, language)
            .map(str::to_owned)
            .or_else(|| profile.email.clone())
    });
    let role = match member.role {
        StreamRole::Guest => Some(language.tr("roles.guest")),
        StreamRole::Member => Some(language.tr("roles.member")),
        StreamRole::Moderator => Some(language.tr("roles.moderator")),
        StreamRole::Administrator => Some(language.tr("roles.admin")),
        StreamRole::Owner => Some(language.tr("roles.owner")),
        StreamRole::Unknown => None,
    };
    MemberView {
        id: member.binding_id,
        avatar: avatar(&name, profile, image),
        name: name.into(),
        role: role.map(Into::into),
        secondary: secondary.map(Into::into),
        remove: available(member.can_remove, can_change),
    }
}

/// Определяет, видит ли пользователь кнопку и принимает ли она действие.
fn available(visible: bool, enabled: bool) -> Availability {
    match (visible, enabled) {
        (false, _) => Availability::Hidden,
        (true, false) => Availability::Disabled,
        (true, true) => Availability::Enabled,
    }
}

/// Готовит заглушку и метку присутствия для аватара.
fn avatar(name: &str, profile: Option<&UserProfile>, image: Option<ImageSource>) -> AvatarView {
    let initial: SharedString = name
        .chars()
        .next()
        .map(|letter| letter.to_uppercase().collect::<String>())
        .unwrap_or_default()
        .into();
    let presence = match profile.and_then(|profile| profile.presence) {
        Some(ProfilePresence::Active) => AvatarPresence::Online,
        Some(ProfilePresence::Idle) => AvatarPresence::Away,
        Some(ProfilePresence::Offline | ProfilePresence::DoNotDisturb) => AvatarPresence::Offline,
        None => AvatarPresence::Hidden,
    };
    AvatarView {
        initials: initial,
        image,
        presence,
    }
}

/// Переводит известное серверное присутствие в подпись пользователя.
fn presence_label(presence: Option<ProfilePresence>, language: Language) -> Option<&'static str> {
    Some(match presence? {
        ProfilePresence::Active => language.tr("a11y.online"),
        ProfilePresence::Idle => language.tr("a11y.away"),
        ProfilePresence::Offline => language.tr("presence.offline"),
        ProfilePresence::DoNotDisturb => language.tr("presence.doNotDisturb"),
    })
}
