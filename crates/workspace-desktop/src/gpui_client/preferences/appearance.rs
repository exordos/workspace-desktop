//! Настройки внешнего вида активного входа. Здесь хранятся режим темы, сортировка,
//! плотность и фон по чатам. Файлы, которые выбирает пользователь, сохраняются как JPEG.

use std::{collections::BTreeMap, sync::Arc};

use gpui::{Image, ImageFormat, ImageSource, Resource};
use serde::{Deserialize, Serialize};
use workspace_core::storage::HistoryScope;

/// Значения, которые сохраняются после закрытия панели и перезапуска клиента.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub(in crate::gpui_client) struct AppearancePreferences {
    pub theme: ThemeChoice,          // Режим освещения оформления.
    pub palette: ThemePalette,       // Набор цветов приложения.
    pub sort: SidebarSort,           // Порядок показа чатов и тем.
    pub density: ChatDensity,        // Размер строк каталога.
    pub background: ChatBackgrounds, // Общий фон и исключения отдельных чатов.
}

/// Выбор режима пользователем независимо от темы системы.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(in crate::gpui_client) enum ThemeChoice {
    Light, // Всегда светлое оформление.
    Dark,  // Всегда тёмное оформление.
    #[default]
    System, // Текущая тема ОС.
}

/// Наборы цветов, доступные в Electron.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(in crate::gpui_client) enum ThemePalette {
    OrangeWarm, // Тёплая оранжевая гамма.
    #[default]
    BlueCold, // Холодная синяя гамма и начальный выбор Electron.
    BlueMist,   // Синяя гамма с серым фоном.
    EmeraldChat, // Изумрудные акценты.
}

/// Порядок чатов и тем внутри их группы.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::gpui_client) enum SidebarSort {
    #[default]
    LastMessage, // Новые сообщения первыми.
    UnreadFirst,  // Непрочитанные первыми, затем по последнему сообщению.
    Alphabetical, // По названию чата или темы.
}

/// Варианты размера строки каталога.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(in crate::gpui_client) enum ChatDensity {
    #[default]
    Standard, // Аватар, метаданные и превью сообщения.
    Compact, // Уменьшенный аватар и одна строка без превью.
}

/// Встроенные иллюстрации Workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(in crate::gpui_client) enum BackgroundPreset {
    Mountains, // Горы.
    Sea,       // Море.
    Forest,    // Лес.
}

impl BackgroundPreset {
    /// Возвращает стабильный ключ ресурса и элементов панели.
    pub fn id(self) -> &'static str {
        match self {
            Self::Mountains => "mountains",
            Self::Sea => "sea",
            Self::Forest => "forest",
        }
    }

    /// Использует тот же рисунок, что и Electron, как локальный ресурс GPUI.
    pub fn source(self) -> ImageSource {
        ImageSource::Resource(Resource::Embedded(
            format!("chat-backgrounds/{}.svg", self.id()).into(),
        ))
    }
}

/// Фон может быть пресетом или нормализованным JPEG. None задаёт фон темы.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::gpui_client) enum Background {
    Preset(BackgroundPreset), // Встроенная иллюстрация Workspace.
    Custom(#[serde(with = "encoded_jpeg")] Arc<Image>), // Готовые JPEG-байты для GPUI.
}

impl Background {
    /// Делит один GPUI-ресурс между панелью, диалогом и историей.
    pub fn source(&self) -> ImageSource {
        match self {
            Self::Preset(preset) => preset.source(),
            Self::Custom(image) => image.clone().into(),
        }
    }

    /// Приблизительно учитывает место фона в сохранённом снимке профиля.
    fn stored_len(&self) -> usize {
        match self {
            Self::Preset(_) => 32,
            Self::Custom(image) => image.bytes.len().div_ceil(3) * 4 + 32,
        }
    }
}

/// Общий фон и явные замены. Пустая замена показывает фон темы в одном чате.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(in crate::gpui_client) struct ChatBackgrounds {
    default: Option<Background>, // Фон всего рабочего пространства этого входа.
    chats: BTreeMap<String, Option<Background>>, // Фон по UUID разговора или темы.
}

impl ChatBackgrounds {
    /// Возвращает фон открытого чата, учитывая явное исключение из общего фона.
    pub fn for_chat(&self, chat: Option<HistoryScope>) -> Option<&Background> {
        match chat.and_then(|scope| self.chats.get(&background_key(scope))) {
            Some(value) => value.as_ref(),
            None => self.default.as_ref(),
        }
    }

    /// Готовит новое значение и проверяет лимит хранения, сохраняя исходное при ошибке.
    pub fn with_background(
        &self,
        chat: Option<HistoryScope>,
        value: Option<Background>,
    ) -> Option<Self> {
        let next = match chat {
            Some(scope) => {
                let mut next = self.clone();
                next.chats.insert(background_key(scope), value);
                next
            }
            None => Self {
                default: value,
                chats: BTreeMap::new(),
            },
        };
        let size = next.default.as_ref().map_or(0, Background::stored_len)
            + next
                .chats
                .iter()
                .map(|(key, value)| {
                    key.len() + 8 + value.as_ref().map_or(0, Background::stored_len)
                })
                .sum::<usize>();
        (size <= 4_000_000).then_some(next)
    }
}

/// Строит ключ темы отдельно от канала, сохраняя привычное наследование общего фона.
fn background_key(scope: HistoryScope) -> String {
    match scope.topic_id {
        Some(topic) => format!("topic:{}:{topic}", scope.stream_id),
        None => format!("stream:{}", scope.stream_id),
    }
}

/// Преобразует байты JPEG в компактную строку для атомарной записи вместе с настройками.
mod encoded_jpeg {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserializer, Serializer, de::Error};

    /// Сохраняет локальный JPEG без пути к файлу пользователя.
    pub(super) fn serialize<S: Serializer>(
        image: &Arc<Image>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(&image.bytes))
    }

    /// Восстанавливает JPEG ограниченного размера для дальнейшего чтения GPUI.
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Arc<Image>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() > 2_000_000 {
            return Err(D::Error::custom("слишком большой файл фона"));
        }
        let bytes = STANDARD.decode(encoded).map_err(D::Error::custom)?;
        if !matches!(image::guess_format(&bytes), Ok(image::ImageFormat::Jpeg)) {
            return Err(D::Error::custom("фон должен быть JPEG"));
        }
        let mut reader =
            image::ImageReader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Jpeg);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(1600);
        limits.max_image_height = Some(1600);
        reader.limits(limits);
        let (width, height) = reader.into_dimensions().map_err(D::Error::custom)?;
        if width == 0 || height == 0 || width > 1600 || height > 1600 {
            return Err(D::Error::custom("недопустимые размеры фона"));
        }
        Ok(Arc::new(Image::from_bytes(ImageFormat::Jpeg, bytes)))
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    /// Проверяет наследование, сброс исключения и замену фона во всех чатах после чтения файла.
    #[test]
    fn chat_overrides_survive_reload_and_all_chats_resets_them() {
        let stream = Uuid::from_u128(10);
        let chat = HistoryScope {
            stream_id: stream,
            topic_id: None,
        };
        let topic = HistoryScope {
            stream_id: stream,
            topic_id: Some(Uuid::from_u128(11)),
        };
        let base = Background::Preset(BackgroundPreset::Mountains);
        let replacement = Background::Preset(BackgroundPreset::Sea);
        let saved = ChatBackgrounds::default()
            .with_background(None, Some(base.clone()))
            .unwrap()
            .with_background(Some(topic), None)
            .unwrap();
        let data = serde_json::to_string(&saved).unwrap();
        let saved: ChatBackgrounds = serde_json::from_str(&data).unwrap();
        assert_eq!(saved.for_chat(Some(chat)), Some(&base));
        assert_eq!(saved.for_chat(Some(topic)), None);
        let replaced = saved
            .with_background(None, Some(replacement.clone()))
            .unwrap();
        assert_eq!(replaced.for_chat(Some(topic)), Some(&replacement));
    }
}
