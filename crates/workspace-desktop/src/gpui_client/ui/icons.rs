//! Рисует иконки GPUI из ресурсов, которые уже входят в Workspace Desktop.

use std::borrow::Cow;

use gpui::{AssetSource, Hsla, Pixels, SharedString, Svg, prelude::*, svg};

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
                    _ => None,
                };
                Ok(found.map(Cow::Borrowed))
            }

            /// Перечисляет встроенные SVG под запрошенным путём.
            fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
                let prefix = path.trim_end_matches('/');
                let paths = [$(Icon::$variant.path()),+];
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
    Attach => ("attach.svg", "Выбор файла."),
    Bell => ("bell.svg", "Уведомления."),
    Check => ("check.svg", "Сервер принял сообщение."),
    Chat => ("chat_bubble_outline.svg", "Мессенджер или личный разговор."),
    ChevronDown => ("chevron-down.svg", "Раскрытый раздел."),
    ChevronRight => ("chevron-right.svg", "Свёрнутый раздел."),
    Close => ("close.svg", "Закрытие или очистка поля."),
    DoneAll => ("done_all.svg", "Подтверждённая отправка."),
    Error => ("error.svg", "Ошибка действия."),
    Folder => ("folder.svg", "Папка разговоров."),
    More => ("more_vert.svg", "Дополнительные действия."),
    Pending => ("schedule.svg", "Отправка ожидает подтверждения."),
    Search => ("search.svg", "Поиск."),
    Send => ("send.svg", "Отправка сообщения."),
    Settings => ("settings.svg", "Настройки."),
    Smile => ("smile.svg", "Эмодзи."),
    Topic => ("lists.svg", "Тема канала."),
}
