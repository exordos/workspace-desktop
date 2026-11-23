//! Представление двух правых панелей. Получает снимок состояния и передаёт действия экрану.

mod channel;
mod gallery;
mod peer;

use std::rc::Rc;

use gpui::{App, Div, FocusHandle, ImageSource, SharedString, Window};
use uuid::Uuid;

use super::{
    Conversation,
    gallery::{Action as GalleryAction, Gallery},
};
use crate::gpui_client::ui::{Avatar, AvatarPresence, Icon, Palette};

pub(super) use gallery::title as gallery_title;

/// Данные одного из двух видов панели.
pub(super) enum PanelContent {
    Peer(PeerView),       // Карточка собеседника в личном чате.
    Channel(ChannelView), // Сведения о канале, темы и участники.
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
    pub presence: Option<SharedString>,      // Подпись «В сети», «Отошёл» и другие варианты.
    pub fields: Vec<ProfileField>,           // Строки профиля в порядке показа.
    pub actions: PeerActions,                // Доступность карточек действий.
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
    pub title: SharedString, // Отображаемое имя темы.
    pub selected: bool,      // Сейчас открыт этот разговор.
    pub muted: Option<bool>, // Действующий режим темы, если он известен.
    pub unread: u64,         // Активные непрочитанные или счётчик приглушённых.
    pub unread_muted: bool,  // Счётчик относится к приглушённым непрочитанным.
    pub mentioned: bool,     // В теме есть непрочитанное личное упоминание.
}

/// Контекст открытой темы и сведения о завершении.
pub(super) struct SelectedTopicView {
    pub title: SharedString,           // Название открытой темы.
    pub done: bool,                    // Тема завершена.
    pub summary: Option<SharedString>, // Готовый контекст из данных темы.
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
    pub avatar: AvatarView,                        // Аватар канала с резервными инициалами.
    pub color: Option<gpui::Hsla>,                 // Цвет подложки аватара, если задан.
    pub description: Option<SharedString>,         // Описание канала.
    pub member_count: usize,                       // Общее число участников.
    pub online_count: usize,                       // Число участников в сети.
    pub notifications: Option<NotificationsView>,  // Режим уведомлений, если он доступен.
    pub selected_topic: Option<SelectedTopicView>, // Контекст открытой темы, если он есть.
    pub topics_expanded: bool,                     // Видимость списка тем.
    pub topics: Vec<TopicView>,                    // Темы в порядке показа.
    pub members: Vec<MemberView>,                  // Участники в порядке показа.
    pub add_members: Availability,                 // Доступность кнопки добавления участников.
    pub member_error: Option<SharedString>,        // Ошибка изменения состава, если есть.
}

/// Действие правой панели. Экран проверяет контекст и выполняет команду.
#[derive(Clone)]
#[allow(
    dead_code,
    reason = "Часть действий появится после подключения адаптеров экрана"
)]
pub(super) enum PanelAction {
    MessagePeer,                         // Перейти к разговору профиля.
    CallPeer,                            // Позвонить собеседнику.
    SharePeer,                           // Скопировать ссылку профиля.
    Favorites,                           // Перейти в избранное.
    EditPeer,                            // Изменить свой профиль.
    Copy(String),                        // Скопировать видимое значение.
    SetNotifications(NotificationLevel), // Установить уровень канала.
    ToggleTopics,                        // Развернуть или свернуть темы.
    OpenTopic(Uuid),                     // Открыть тему канала.
    AddMembers,                          // Показать выбор новых участников.
    RemoveMember(Uuid),                  // Удалить привязку к каналу.
    Gallery(GalleryAction),              // Открыть раздел вложений или выполнить его действие.
}

type Dispatch = Rc<dyn Fn(PanelAction, &mut Window, &mut App)>;

/// Рисует готовые сведения. Загрузкой данных и фокусом оболочки владеет экран.
pub(super) fn render(
    content: &PanelContent,
    palette: Palette,
    gallery_triggers: Option<&[FocusHandle; 4]>,
    dispatch: impl Fn(PanelAction, &mut Window, &mut App) + 'static,
) -> Div {
    let dispatch: Dispatch = Rc::new(dispatch);
    let gallery = gallery_triggers.map(|triggers| gallery::triggers(triggers, palette, &dispatch));
    match content {
        PanelContent::Peer(peer) => peer::render(peer, gallery, palette, &dispatch),
        PanelContent::Channel(channel) => channel::render(channel, gallery, palette, &dispatch),
    }
}

/// Создаёт содержимое вложенного экрана с действиями текущего разговора.
pub(super) fn render_gallery(
    conversation: &Conversation,
    gallery: &Gallery,
    palette: Palette,
    dispatch: impl Fn(PanelAction, &mut Window, &mut App) + 'static,
) -> Div {
    let dispatch: Dispatch = Rc::new(dispatch);
    gallery::render(conversation, gallery, palette, &dispatch)
}
