//! Представление двух правых панелей. Получает снимок состояния и передаёт действия экрану.

mod channel;
mod gallery;
mod peer;

use std::{rc::Rc, sync::Arc};

use gpui::{
    App, Div, ElementId, FocusHandle, ImageSource, Pixels, Point, SharedString, Window, div,
    prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{
    directory::ProfilePresence, messenger::details::TopicSummaryBlock,
    storage::media::GalleryCounts,
};

use super::{
    Conversation,
    gallery::{Action as GalleryAction, Gallery},
};
use crate::gpui_client::{
    preferences::Language,
    ui::{Avatar, AvatarPresence, Icon, Palette, TextRole, ellipsized_label, label},
};

pub(in crate::gpui_client) use channel::member_counts;
pub(super) use gallery::title as gallery_title;

/// Данные одного из двух видов панели.
pub(super) enum PanelContent {
    Peer(PeerView),            // Карточка собеседника в личном чате.
    Channel(Box<ChannelView>), // Сведения о канале, темы и участники.
}

/// Видимость и доступность действия в текущем контексте.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Availability {
    #[default]
    Hidden, // Действие в этом контексте не отображается.
    Disabled, // Контрол отображается, но временно не принимает нажатия.
    Enabled,  // Можно передать действие контроллеру.
}

/// Изображение и присутствие, которые подготовил экран.
#[derive(Default)]
pub(super) struct AvatarView {
    pub initials: SharedString,     // Инициалы для аватара без изображения.
    pub image: Option<ImageSource>, // Готовый источник изображения, если доступен.
    pub presence: AvatarPresence,   // Индикатор присутствия на аватаре.
}

impl AvatarView {
    /// Создаёт обычный аватар UI-кита с данными экрана.
    fn avatar(&self) -> Avatar {
        Avatar {
            image: self.image.clone(),
            presence: self.presence,
            ..Avatar::from_initials(self.initials.clone())
        }
    }
}

/// Видимая строка сведений профиля.
pub(super) struct ProfileField {
    pub label: SharedString, // Подпись над значением.
    pub icon: Icon,          // Иконка слева от подписи.
    pub value: String,       // Значение для чтения и копирования.
    pub multiline: bool,     // Описание может переноситься на несколько строк.
    pub copy: Availability,  // Доступность копирования.
}

/// Доступные действия для личного чата или чата с собой.
#[derive(Default)]
pub(super) struct PeerActions {
    pub message: Availability,   // Открыть разговор с профилем.
    pub call: Availability,      // Начать звонок.
    pub share: Availability,     // Скопировать ссылку на профиль.
    pub share_copied: bool,      // Показать подтверждение копирования вместо ссылки.
    pub favorites: Availability, // Открыть чат с собой.
    pub edit: Availability,      // Открыть редактирование своего профиля.
}

/// Снимок данных, нужных для карточки личного чата.
#[derive(Default)]
pub(super) struct PeerView {
    pub name: SharedString,                  // Отображаемое имя собеседника.
    pub avatar: AvatarView,                  // Аватар, инициалы и присутствие.
    pub custom_status: Option<SharedString>, // Пользовательский статус, если есть.
    pub presence: Option<ProfilePresence>,   // Состояние для подписи и её цвета.
    pub fields: Vec<ProfileField>,           // Строки профиля в порядке показа.
    pub copied_fields: Vec<usize>,           // Номера строк с галочкой после копирования.
    pub actions: PeerActions,                // Доступность карточек действий.
    pub language: Language,                  // Язык подписей и подсказок.
}

/// Три уровня уведомлений для всего канала.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NotificationLevel {
    Default,    // Уведомлять об упоминаниях.
    Muted,      // Без уведомлений.
    Subscribed, // Все уведомления.
}

/// Выбранный уровень уведомлений и состояние операции.
pub(super) struct NotificationsView {
    pub level: NotificationLevel,    // Подтверждённый уровень.
    pub editable: bool,              // Контроллер может изменить уровень сейчас.
    pub error: Option<SharedString>, // Ошибка последнего изменения, если есть.
}

/// Одна строка списка тем.
pub(super) struct TopicView {
    pub id: Uuid,            // Идентификатор темы для навигации.
    pub focus: FocusHandle,  // Возврат на тему из контекстного меню.
    pub title: SharedString, // Отображаемое имя темы.
    pub selected: bool,      // Сейчас открыт этот разговор.
    pub muted: Option<bool>, // Действующий режим темы, если он известен.
    pub unread: u64,         // Активные непрочитанные или счётчик приглушённых.
    pub unread_muted: bool,  // Счётчик относится к приглушённым непрочитанным.
    pub mentioned: bool,     // В теме есть непрочитанное личное упоминание.
}

/// Контекст открытой темы и сведения о завершении.
pub(super) struct SelectedTopicView {
    pub title: SharedString,                       // Название открытой темы.
    pub done: bool,                                // Тема завершена.
    pub summary: Option<Arc<[TopicSummaryBlock]>>, // Контекст с нативным начертанием.
    pub expanded: bool,                            // Содержимое контекста сейчас раскрыто.
    pub summary_enabled: bool,                     // Контекст продолжает обновляться.
    pub summary_has_new_messages: bool,            // После сводки появились сообщения.
    pub summary_settings: Option<FocusHandle>,     // Право открыть настройки и вернуть фокус.
}

/// Строка участника с действием, разрешённым контроллером.
pub(super) struct MemberView {
    pub id: Uuid,                        // Идентификатор привязки к каналу.
    pub name: SharedString,              // Отображаемое имя.
    pub avatar: AvatarView,              // Аватар и индикатор присутствия.
    pub role: Option<SharedString>,      // Метка роли, если есть.
    pub secondary: Option<SharedString>, // Статус или email под именем.
    pub remove: Availability,            // Доступность удаления участника или выхода.
}

/// Снимок данных панели канала.
#[derive(Default)]
pub(super) struct ChannelView {
    pub name: SharedString,                        // Отображаемое имя канала.
    pub language: Language,                        // Язык подписей и счётчиков.
    pub avatar: AvatarView,                        // Аватар канала с резервными инициалами.
    pub color: Option<gpui::Hsla>,                 // Цвет подложки аватара, если задан.
    pub description: Option<SharedString>,         // Описание канала.
    pub member_count: usize,                       // Общее число участников.
    pub online_count: usize,                       // Число участников в сети.
    pub notifications: Option<NotificationsView>,  // Режим уведомлений, если он доступен.
    pub selected_topic: Option<SelectedTopicView>, // Контекст открытой темы, если он есть.
    pub topics_expanded: bool,                     // Видимость списка тем.
    pub topics: Vec<TopicView>,                    // Темы в порядке показа.
    pub topic_error: Option<SharedString>,         // Ошибка изменения темы.
    pub members: Vec<MemberView>,                  // Участники в порядке показа.
    pub add_members: Availability,                 // Доступность кнопки добавления участников.
    pub add_members_focus: Option<FocusHandle>,    // Возврат на кнопку из окна приглашения.
    pub member_error: Option<SharedString>,        // Ошибка изменения состава, если есть.
}

/// Действие правой панели. Экран проверяет контекст и выполняет команду.
#[derive(Clone)]
#[allow(
    dead_code,
    reason = "Часть действий появится после подключения адаптеров экрана"
)]
pub(super) enum PanelAction {
    MessagePeer, // Перейти к разговору профиля.
    CallPeer,    // Позвонить собеседнику.
    SharePeer,   // Скопировать ссылку профиля.
    Favorites,   // Перейти в избранное.
    EditPeer,    // Изменить свой профиль.
    Copy {
        field: usize,  // Номер строки, на которой показать подтверждение.
        value: String, // Видимое значение для буфера обмена.
    },
    SetNotifications(NotificationLevel), // Установить уровень канала.
    ToggleTopics,                        // Развернуть или свернуть темы.
    ToggleSummary,                       // Развернуть или свернуть контекст темы.
    OpenSummarySettings,                 // Открыть форму параметров выжимки.
    OpenTopic(Uuid),                     // Открыть тему канала.
    OpenTopicMenu {
        topic: Uuid,           // UUID темы в переданном снимке канала.
        anchor: Point<Pixels>, // Точка показа меню рядом с указателем или строкой.
    },
    AddMembers,             // Показать выбор новых участников.
    RemoveMember(Uuid),     // Удалить привязку к каналу.
    Gallery(GalleryAction), // Открыть раздел вложений или выполнить его действие.
}

type Dispatch = Rc<dyn Fn(PanelAction, &mut Window, &mut App)>;

/// Данные и фокус четырёх переходов в разделы вложений.
#[derive(Clone, Copy)]
pub(super) struct GalleryTriggers<'a> {
    pub focus: &'a [FocusHandle; 4], // Фокус каждой строки в порядке показа.
    pub counts: Option<GalleryCounts>, // Числа известной истории, если их удалось прочитать.
}

/// Рисует готовые сведения. Загрузкой данных и фокусом оболочки владеет экран.
pub(super) fn render(
    content: &PanelContent,
    palette: Palette,
    gallery_triggers: Option<GalleryTriggers<'_>>,
    dispatch: impl Fn(PanelAction, &mut Window, &mut App) + 'static,
) -> Div {
    let dispatch: Dispatch = Rc::new(dispatch);
    let gallery = |language| {
        gallery_triggers.map(|triggers| gallery::triggers(triggers, palette, language, &dispatch))
    };
    match content {
        PanelContent::Peer(peer) => peer::render(peer, gallery(peer.language), palette, &dispatch),
        PanelContent::Channel(channel) => {
            channel::render(channel, gallery(channel.language), palette, &dispatch)
        }
    }
}

/// Рисует сведения защищённого чата с ключами, участниками и разделами вложений.
pub(super) fn render_encrypted(
    channel: &ChannelView,
    actions: Div,
    keys: Div,
    palette: Palette,
    gallery_triggers: Option<GalleryTriggers<'_>>,
    dispatch: impl Fn(PanelAction, &mut Window, &mut App) + 'static,
) -> Div {
    let dispatch: Dispatch = Rc::new(dispatch);
    let language = channel.language;
    let header = div()
        .flex()
        .items_center()
        .gap_4()
        .px_2()
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(64.))
                .rounded_full()
                .bg(channel.color.unwrap_or(palette.card))
                .child(Icon::ChatLock.element(px(40.), palette.text)),
        )
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .flex_col()
                .gap_2()
                .child(
                    ellipsized_label(palette, TextRole::ProfileHeading, channel.name.clone())
                        .h(px(20.)),
                )
                .child(
                    label(palette, TextRole::Label, language.tr("e2ee.protectedChat"))
                        .text_size(px(16.))
                        .line_height(px(16.))
                        .text_color(palette.accent),
                )
                .child(ellipsized_label(
                    Palette {
                        text: palette.text_muted,
                        ..palette
                    },
                    TextRole::Body,
                    member_counts(language, channel.member_count, channel.online_count),
                )),
        );
    encrypted_scroll(
        "encrypted-info-scroll",
        div()
            .flex()
            .flex_col()
            .gap_5()
            .debug_selector(|| "conversation-encrypted-info".into())
            .child(header)
            .child(actions)
            .child(keys)
            .children(
                gallery_triggers
                    .map(|triggers| gallery::triggers(triggers, palette, language, &dispatch)),
            )
            .child(
                channel::render_members(channel, palette, &dispatch)
                    .border_t_1()
                    .border_color(palette.border)
                    .pt_5(),
            ),
    )
}

/// Прокручивает содержимое открытого экрана внутри общей рамки панели.
pub(super) fn encrypted_scroll(id: impl Into<ElementId>, content: Div) -> Div {
    div().flex().flex_col().min_h_0().flex_1().child(
        div()
            .id(id)
            .min_h_0()
            .flex_1()
            .overflow_x_hidden()
            .overflow_y_scroll()
            .child(content.py_4()),
    )
}

/// Создаёт содержимое вложенного экрана с действиями текущего разговора.
pub(super) fn render_gallery(
    conversation: &Conversation,
    gallery: &Gallery,
    palette: Palette,
    language: Language,
    dispatch: impl Fn(PanelAction, &mut Window, &mut App) + 'static,
) -> Div {
    let dispatch: Dispatch = Rc::new(dispatch);
    gallery::render(conversation, gallery, palette, language, &dispatch)
}
