//! Рисует иконки GPUI из ресурсов, которые уже входят в Workspace Desktop.

use std::borrow::Cow;

use gpui::{AssetSource, Hsla, Pixels, SharedString, Svg, prelude::*, px, svg};

/// Слот иконки в компактной кнопке.
pub(super) const BUTTON_ICON_SIZE: Pixels = px(16.);
/// Слот лупы в поле поиска каталога.
pub const SEARCH_ICON_SIZE: Pixels = px(18.);

/// Путь к многоцветному логотипу, доступному через ресурсы GPUI.
pub(super) const ORGANIZATION_LOGO: &str = "organization-fallback.svg";

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
                    _ => None,
                };
                Ok(found.map(Cow::Borrowed))
            }

            /// Перечисляет встроенные SVG под запрошенным путём.
            fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
                let prefix = path.trim_end_matches('/');
                let paths = [ORGANIZATION_LOGO, $(Icon::$variant.path()),+];
                Ok(paths.into_iter().filter(|file| {
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
    Account => ("account_circle.svg", "Аккаунт или пользователь без фотографии."),
    Add => ("add.svg", "Создание элемента."),
    At => ("at.svg", "Уведомления об упоминаниях."),
    Attach => ("attach.svg", "Выбор файла."),
    Bell => ("bell.svg", "Уведомления."),
    BellOff => ("bell_off.svg", "Уведомления выключены."),
    Check => ("check.svg", "Сервер принял сообщение."),
    BusinessCenter => ("business_center.svg", "Команда или должность."),
    Calendar => ("calendar.svg", "Календарь или дата в профиле."),
    Celebration => ("celebration.svg", "День рождения."),
    Chat => ("chat_bubble_outline.svg", "Мессенджер или личный разговор."),
    Channels => ("channels.svg", "Системная папка каналов."),
    ChevronDown => ("chevron-down.svg", "Раскрытый раздел."),
    ChevronRight => ("chevron-right.svg", "Свёрнутый раздел."),
    ChevronUp => ("chevron-up.svg", "Свернуть открытый раздел."),
    Close => ("close.svg", "Закрытие или очистка поля."),
    Copy => ("copy.svg", "Копирование текста."),
    DoneAll => ("done_all.svg", "Подтверждённая отправка."),
    Error => ("error.svg", "Ошибка действия."),
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
    Info => ("info.svg", "Дополнительные сведения."),
    Links => ("links.svg", "Ссылка на профиль."),
    Mail => ("mail_outline.svg", "Адрес электронной почты."),
    PersonAdd => ("person_add.svg", "Добавление участника."),
    Phone => ("phone.svg", "Телефон или звонок."),
    Search => ("search.svg", "Поиск."),
    Send => ("send.svg", "Отправка сообщения."),
    Settings => ("settings.svg", "Настройки."),
    Smile => ("smile.svg", "Эмодзи."),
    Stylus => ("border_color.svg", "Изменение профиля."),
    Topic => ("lists.svg", "Тема канала."),
}
