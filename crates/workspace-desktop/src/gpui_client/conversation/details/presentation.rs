//! Преобразует подтверждённые данные Messenger в видимые сведения и разрешённые кнопки.

use gpui::{SharedString, rgb};
use workspace_core::{
    directory::{ProfilePresence, UserProfile},
    messenger::details::{
        ChannelSidebar, DirectSidebar, SidebarDetails, SidebarMember, StreamNotificationMode,
        StreamRole, TopicNotificationMode, sidebar_profile_link,
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
    ui::{AvatarPresence, Icon},
};

/// Подбирает одно из двух представлений для выбранного адреса.
pub(super) fn content(
    panel: &Panel,
    details: &SidebarDetails,
    address: &ConversationAddress,
) -> PanelContent {
    match details {
        SidebarDetails::Peer(peer) => PanelContent::Peer(peer_content(panel, peer, address)),
        SidebarDetails::Channel(channel) => {
            PanelContent::Channel(channel_content(panel, channel, address))
        }
    }
}

/// Собирает сведения профиля и кнопки для своего и чужого профиля.
fn peer_content(panel: &Panel, peer: &DirectSidebar, address: &ConversationAddress) -> PeerView {
    let profile = &peer.profile;
    let share = if sidebar_profile_link(&address.route.owner, peer.user_id).is_ok() {
        Availability::Enabled
    } else {
        Availability::Disabled
    };
    let actions = if peer.is_self {
        PeerActions {
            share,
            share_copied: panel.share_feedback.is_some(),
            favorites: Availability::Disabled,
            edit: Availability::Disabled,
            ..PeerActions::default()
        }
    } else {
        PeerActions {
            message: Availability::Enabled,
            call: Availability::Disabled,
            share,
            share_copied: panel.share_feedback.is_some(),
            ..PeerActions::default()
        }
    };
    PeerView {
        name: profile.display_name.clone().into(),
        avatar: avatar(&profile.display_name, Some(profile)),
        custom_status: custom_status(profile).map(Into::into),
        presence: presence_label(profile.presence).map(Into::into),
        fields: vec![
            field(
                "Идентификатор пользователя",
                Icon::At,
                Some(peer.user_id.to_string()),
            ),
            field("Электронная почта", Icon::Mail, profile.email.clone()),
            field("Телефон", Icon::Phone, None),
            field("Должность", Icon::BusinessCenter, None),
            field("Руководитель", Icon::Handshake, None),
            field("Роль", Icon::Account, None),
            field("Тип аккаунта", Icon::Group, None),
            field("Статус аккаунта", Icon::Info, None),
            field("Часовой пояс", Icon::GlobeLocation, None),
            field("Местное время", Icon::Pending, None),
            field(
                "Дата присоединения",
                Icon::Calendar,
                profile.created_at.as_deref().and_then(joined_date),
            ),
            field("День рождения", Icon::Celebration, None),
        ],
        actions,
    }
}

/// Показывает дату как в профиле Workspace без исходного времени сервера.
fn joined_date(timestamp: &str) -> Option<String> {
    let date = timestamp.get(..10)?;
    let mut parts = date.split('-');
    let year = parts.next()?;
    let month = parts.next()?;
    let day = parts.next()?;
    let month_value: u8 = month.parse().ok()?;
    let day_value: u8 = day.parse().ok()?;
    let _: u16 = year.parse().ok()?;
    (year.len() == 4
        && parts.next().is_none()
        && (1..=12).contains(&month_value)
        && (1..=31).contains(&day_value))
    .then(|| format!("{day}.{month}.{year}"))
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
fn channel_content(
    panel: &Panel,
    channel: &ChannelSidebar,
    address: &ConversationAddress,
) -> ChannelView {
    let can_change = matches!(panel.state, Some(State::Ready)) && panel.mutation.is_none();
    let current_topic = address.route.scope.topic_id;
    ChannelView {
        name: channel.name.clone().into(),
        avatar: avatar(&channel.name, None),
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
        topics: channel
            .topics
            .iter()
            .filter(|topic| !topic.done)
            .map(|topic| TopicView {
                id: topic.id,
                title: topic.name.clone().into(),
                selected: current_topic == Some(topic.id),
                muted: Some(
                    topic.notification_mode == TopicNotificationMode::Mute
                        || (topic.notification_mode == TopicNotificationMode::Default
                            && channel.notification_mode == StreamNotificationMode::Muted),
                ),
                unread: 0,
                mentioned: false,
            })
            .collect(),
        selected_topic: channel
            .topics
            .iter()
            .find(|topic| Some(topic.id) == current_topic && topic.done)
            .map(|topic| SelectedTopicView {
                title: topic.name.clone().into(),
                done: true,
            }),
        notifications: Some(NotificationsView {
            level: match channel.notification_mode {
                StreamNotificationMode::AllMessages => NotificationLevel::Subscribed,
                StreamNotificationMode::MentionsOnly => NotificationLevel::Default,
                StreamNotificationMode::Muted => NotificationLevel::Muted,
            },
            editable: can_change,
            error: (panel.mutation_error == Some(MutationKind::Notifications))
                .then(|| "Не удалось изменить уведомления".into()),
        }),
        members: channel
            .members
            .iter()
            .map(|member| member_view(member, can_change))
            .collect(),
        add_members: available(channel.can_add_members, can_change),
        member_error: (panel.mutation_error == Some(MutationKind::Remove))
            .then(|| "Не удалось удалить участника".into()),
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
fn member_view(member: &SidebarMember, can_change: bool) -> MemberView {
    let profile = member.profile.as_ref();
    let name = profile
        .map(|profile| profile.display_name.clone())
        .unwrap_or_else(|| member.user_id.to_string());
    let secondary = profile.and_then(|profile| {
        presence_label(profile.presence)
            .map(str::to_owned)
            .or_else(|| profile.email.clone())
    });
    let role = match member.role {
        StreamRole::Guest => Some("Гость"),
        StreamRole::Member => Some("Участник"),
        StreamRole::Moderator => Some("Модератор"),
        StreamRole::Administrator => Some("Администратор"),
        StreamRole::Owner => Some("Владелец"),
        StreamRole::Unknown => None,
    };
    MemberView {
        id: member.binding_id,
        avatar: avatar(&name, profile),
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
pub(super) fn avatar(name: &str, profile: Option<&UserProfile>) -> AvatarView {
    let initial: SharedString = name
        .chars()
        .next()
        .map(|letter| letter.to_uppercase().collect::<String>())
        .unwrap_or_default()
        .into();
    let presence = match profile.and_then(|profile| profile.presence) {
        Some(ProfilePresence::Active) => AvatarPresence::Online,
        Some(ProfilePresence::Idle) => AvatarPresence::Away,
        _ => AvatarPresence::Hidden,
    };
    AvatarView {
        initials: initial,
        image: None,
        presence,
    }
}

/// Собирает статус, когда пользователь указал текст, эмодзи или оба.
fn custom_status(profile: &UserProfile) -> Option<String> {
    match (&profile.status_emoji, &profile.status_text) {
        (Some(emoji), Some(text)) => Some(format!("{emoji} {text}")),
        (Some(emoji), None) => Some(emoji.clone()),
        (None, Some(text)) => Some(text.clone()),
        (None, None) => None,
    }
}

/// Переводит известное серверное присутствие в подпись пользователя.
fn presence_label(presence: Option<ProfilePresence>) -> Option<&'static str> {
    Some(match presence? {
        ProfilePresence::Active => "В сети",
        ProfilePresence::Idle => "Отошёл",
        ProfilePresence::Offline => "Не в сети",
        ProfilePresence::DoNotDisturb => "Не беспокоить",
    })
}
