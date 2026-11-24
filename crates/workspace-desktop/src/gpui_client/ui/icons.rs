//! Рисует иконки GPUI из ресурсов, которые уже входят в Workspace Desktop.

use std::borrow::Cow;

use gpui::{AssetSource, Hsla, Pixels, SharedString, Svg, prelude::*, px, svg};

/// Слот иконки в компактной кнопке.
pub(super) const BUTTON_ICON_SIZE: Pixels = px(16.);
/// Слот лупы в поле поиска каталога.
pub const SEARCH_ICON_SIZE: Pixels = px(18.);

/// Путь к многоцветному логотипу, доступному через ресурсы GPUI.
pub(super) const ORGANIZATION_LOGO: &str = "organization-fallback.svg";
const STATUS_EMOJI_CATEGORIES: &str = "icons/status-emoji-categories.svg";
const BACKGROUNDS: [(&str, &[u8]); 3] = [
    (
        "chat-backgrounds/mountains.svg",
        include_bytes!("../../../assets/chat-backgrounds/mountains.svg"),
    ),
    (
        "chat-backgrounds/sea.svg",
        include_bytes!("../../../assets/chat-backgrounds/sea.svg"),
    ),
    (
        "chat-backgrounds/forest.svg",
        include_bytes!("../../../assets/chat-backgrounds/forest.svg"),
    ),
];

// Одно имя ресурса используется и при отрисовке, и при поиске в AssetSource.
macro_rules! workspace_icons {
    ($($variant:ident => ($filename:literal, $description:literal)),+ $(,)?) => {
        /// Иконка из набора Workspace с поддержкой цвета GPUI.
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum Icon {
            $(#[doc = $description] $variant,)+
        }

        impl Icon {
            /// Находит путь внутри встроенного набора Workspace.
            pub const fn path(self) -> &'static str {
                match self {
                    $(Self::$variant => concat!("icons/", $filename),)+
                }
            }

            /// Рисует иконку в квадратном слоте указанного размера.
            pub fn element(self, size: Pixels, color: Hsla) -> Svg {
                svg().path(self.path()).size(size).flex_none().text_color(color)
            }
        }

        impl AssetSource for WorkspaceAssets {
            /// Возвращает сохранённый в бинарнике SVG.
            fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
                let found: Option<&'static [u8]> = match path {
                    $(p if p == Icon::$variant.path() => Some(include_bytes!(concat!("../../../assets/icons/", $filename))),)+
                    ORGANIZATION_LOGO => Some(include_bytes!("../../../assets/organization-fallback.svg")),
                    STATUS_EMOJI_CATEGORIES => Some(include_bytes!("../../../assets/icons/status-emoji-categories.svg")),
                    _ => BACKGROUNDS.iter().find(|(name, _)| *name == path).map(|(_, bytes)| *bytes),
                };
                Ok(found.map(Cow::Borrowed))
            }

            /// Перечисляет встроенные SVG под запрошенным путём.
            fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
                let prefix = path.trim_end_matches('/');
                let paths = [ORGANIZATION_LOGO, STATUS_EMOJI_CATEGORIES, $(Icon::$variant.path()),+];
                Ok(paths.into_iter().chain(BACKGROUNDS.iter().map(|(name, _)| *name)).filter(|file| {
                    prefix.is_empty() || *file == prefix || file.strip_prefix(prefix).is_some_and(|suffix| suffix.starts_with('/'))
                }).map(Into::into).collect())
            }
        }
    };
}

/// Источник иконок для `Application::with_assets` рабочего клиента и галереи.
#[derive(Default)]
pub struct WorkspaceAssets;

workspace_icons! {
    Command => ("command.svg", "Горячие клавиши."),
    ChatBubble => ("chat_bubble.svg", "Действие при клике по чату."),
    VolumeUp => ("volume_up.svg", "Звук уведомлений."),
    DesktopWindows => ("desktop_windows.svg", "Системный режим или Dock."),
    Language => ("language.svg", "Язык интерфейса."),
    DeleteHistory => ("delete_history.svg", "Автоматический выход после неактивности."),
    Account => ("account_circle.svg", "Аккаунт или пользователь без фотографии."),
    Add => ("add.svg", "Создание элемента."),
    At => ("at.svg", "Уведомления об упоминаниях."),
    AlternateEmail => ("alternate_email.svg", "Идентификатор пользователя."),
    Attach => ("attach.svg", "Выбор файла."),
    Bell => ("bell.svg", "Уведомления."),
    BellOff => ("bell_off.svg", "Уведомления выключены."),
    Check => ("check.svg", "Сервер принял сообщение."),
    BusinessCenter => ("business_center.svg", "Команда или должность."),
    Calendar => ("calendar.svg", "Календарь или дата в профиле."),
    CalendarNext => ("calendar-next.svg", "Переход между месяцами календаря."),
    Celebration => ("celebration.svg", "День рождения."),
    Chat => ("chat_bubble_outline.svg", "Мессенджер или личный разговор."),
    ChatLock => ("chat_lock.svg", "Защищённый канал в шапке разговора."),
    Channels => ("channels.svg", "Системная папка каналов."),
    ChevronDown => ("chevron-down.svg", "Раскрытый раздел."),
    ChevronLeft => ("chevron-left.svg", "Назад к предыдущему экрану."),
    ChevronRight => ("chevron-right.svg", "Свёрнутый раздел."),
    ChevronUp => ("chevron-up.svg", "Свернуть открытый раздел."),
    Close => ("close.svg", "Закрытие или очистка поля."),
    Copy => ("copy.svg", "Копирование текста."),
    DashboardCustomize => ("dashboard_customize.svg", "Сервер текущего аккаунта."),
    Delete => ("delete.svg", "Очистка или удаление."),
    Download => ("download.svg", "Загрузка обновления."),
    Draw => ("draw.svg", "Внешний вид приложения."),
    DoneAll => ("done_all.svg", "Подтверждённая отправка."),
    Error => ("error.svg", "Ошибка действия."),
    EncryptedSpoiler => ("encrypted_spoiler.svg", "Защищённый фрагмент сообщения."),
    Envelope => ("mail.svg", "Раздел почты."),
    Folder => ("folder.svg", "Папка разговоров."),
    FolderOpen => ("folder_open.svg", "Выбранная папка."),
    Folders => ("folders.svg", "Все чаты."),
    FolderList => ("more.svg", "Быстрый выбор папки."),
    More => ("more_vert.svg", "Дополнительные действия."),
    Pending => ("schedule.svg", "Отправка ожидает подтверждения."),
    Pin => ("pin.svg", "Закреплённый разговор."),
    Profile => ("profile.svg", "Личные разговоры."),
    GlobeLocation => ("globe_location_pin.svg", "Часовой пояс."),
    Group => ("group.svg", "Тип аккаунта."),
    Handshake => ("handshake.svg", "Руководитель."),
    Home => ("home.svg", "Переход к избранному."),
    HomeFilled => ("home_filled.svg", "Цветная карточка избранного."),
    ActivityInbox => ("mail_activity.svg", "Цветная карточка входящих."),
    ActivityInboxCompact => ("mail_activity_compact.svg", "Компактный переход во входящие."),
    ActivityMarked => ("marker.svg", "Цветная карточка отмеченных сообщений."),
    ActivityMarkedCompact => ("marker_outline.svg", "Компактный переход к отмеченным сообщениям."),
    ActivityMentions => ("alternate_email.svg", "Переход к упоминаниям."),
    ActivityDrafts => ("drafts.svg", "Цветная карточка черновиков."),
    ActivityDraftsCompact => ("drafts_compact.svg", "Компактный переход к черновикам."),
    Image => ("image.svg", "Выбрать фото из галереи."),
    GalleryPhoto => ("gallery-photo.svg", "Фотографии из истории разговора."),
    GalleryVideo => ("videos.svg", "Видео из истории разговора."),
    GalleryFile => ("gallery-file.svg", "Файлы из истории разговора."),
    GalleryLink => ("gallery-link.svg", "Веб-ссылки из истории разговора."),
    Info => ("info.svg", "Дополнительные сведения."),
    Links => ("links.svg", "Ссылка на профиль."),
    Logout => ("logout_compact.svg", "Выход из аккаунта."),
    Mail => ("mail_outline.svg", "Адрес электронной почты."),
    NewWindow => ("new_window.svg", "Открытие сведений."),
    PersonAdd => ("person_add.svg", "Добавление участника."),
    Phone => ("phone.svg", "Телефон или звонок."),
    Forward => ("forward.svg", "Переслать сообщение."),
    Plus => ("plus.svg", "Открыть список реакций."),
    Reply => ("reply.svg", "Ответить на сообщение."),
    Search => ("search.svg", "Поиск."),
    SidePanel => ("side_panel.svg", "Открытие и закрытие правой панели."),
    Send => ("send.svg", "Отправка сообщения."),
    Settings => ("settings.svg", "Настройки."),
    InstantMix => ("instant_mix.svg", "Настройки защищённого чата."),
    Smile => ("smile.svg", "Эмодзи."),
    StatusSmile => ("sentiment_satisfied.svg", "Статус пользователя."),
    Stylus => ("border_color.svg", "Изменение профиля."),
    Topic => ("lists.svg", "Тема канала."),
    TopicFollow => ("topic_follow.svg", "Следить за темой."),
    NightTheme => ("bedtime.svg", "Тёмный режим оформления."),
    DayTheme => ("wb_sunny.svg", "Светлый режим оформления."),
    FolderLayout => ("folder_copy.svg", "Расположение папок."),
    StreamOrder => ("list_arrow.svg", "Порядок каналов и тем."),
    Background => ("images.svg", "Фон чата."),
    BackgroundAdd => ("plus.svg", "Добавление пользовательского фона."),
    ComposerToolbar => ("composer-bottom-panel-open.svg", "Скрытие инструментов форматирования."),
    ComposerToolbarExpand => ("composer-bottom-panel-close.svg", "Показ инструментов форматирования."),
    ComposerLink => ("composer-add-link.svg", "Вставка ссылки."),
    ComposerBold => ("composer-format-bold.svg", "Жирный текст."),
    ComposerItalic => ("composer-format-italic.svg", "Курсив."),
    ComposerStrike => ("composer-strikethrough.svg", "Зачёркнутый текст."),
    ComposerNumberedList => ("composer-list-numbered.svg", "Нумерованный список."),
    ComposerBulletedList => ("composer-list-bulleted.svg", "Маркированный список."),
    ComposerQuote => ("composer-chat-dashed.svg", "Цитата Markdown."),
    ComposerCodeBlock => ("composer-frame-source.svg", "Блок кода."),
    ComposerInlineCode => ("composer-inline-code.svg", "Код в строке."),
    ComposerSpoiler => ("composer-spoiler.svg", "Спойлер."),
}
