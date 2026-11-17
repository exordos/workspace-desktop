//! Содержит образец рабочего ChatRow на данных UI-02.

use gpui::{AnyElement, App, FocusHandle, Window, prelude::*};
use serde_json::Value;
use workspace_desktop::gpui_client::ui::{
    Avatar, BadgeTone, ChatRow, ChatRowPreview, Palette, ThemeMode,
};

/// Стабильное имя сцены и истории в каталоге.
pub const ID: &str = "chat-row";
/// Ширина образца совпадает с шириной рабочего сайдбара.
pub const ROW_WIDTH: f32 = 264.;
/// Описывает входные данные, действия и варианты истории.
pub const DESCRIPTION: &str = "Строка канала в каталоге Workspace. Название, выбор, счётчик и палитру можно менять. Клик и Enter выбирают строку. Длинное название оставляет место времени и счётчику.";

const FIXTURES: &str =
    include_str!("../../../../docs/plans/roadmap/gpui-ui-parity-fixtures/scenes.json");

/// Хранит параметры одного синтетического примера строки каталога.
#[derive(Clone)]
pub struct Sample {
    pub title: String,  // Название из набора UI-02 или короткий вариант для сравнения.
    pub unread: u64,    // Число непрочитанных. Ноль скрывает бейдж.
    pub selected: bool, // Активна ли строка в каталоге.
    pub theme: ThemeMode, // Режим рабочей палитры Blue Mist.
    preview: String,    // Последнее сообщение канала из UI-02.
}

/// Читает данные из того же набора, что использует сценарий Electron.
pub fn fixture(theme: ThemeMode) -> Sample {
    let data: Value = serde_json::from_str(FIXTURES).expect("валидный набор UI-02");
    let topic = &data["channel"]["topics"][0];
    let last_message = topic["messages"]
        .as_array()
        .and_then(|items| items.last())
        .expect("последнее сообщение канала UI-02");
    Sample {
        title: data["channel"]["title"]
            .as_str()
            .expect("название канала UI-02")
            .into(),
        unread: topic["unread"].as_u64().expect("счётчик темы UI-02"),
        selected: true,
        theme,
        preview: last_message["text"]
            .as_str()
            .expect("текст сообщения UI-02")
            .into(),
    }
}

impl Sample {
    /// Рисует тот же компонент, который открывает канал в рабочем каталоге.
    pub fn row(
        &self,
        focus: Option<&FocusHandle>,
        on_activate: impl Fn(&mut Window, &mut App) + 'static,
    ) -> AnyElement {
        ChatRow {
            id: ID.into(),
            avatar: Avatar::from_initials("I"),
            title: self.title.clone().into(),
            preview: Some(ChatRowPreview {
                sender: Some("Вы".into()),
                text: self.preview.clone().into(),
            }),
            time: Some("13:13".into()),
            unread: Some((self.unread, BadgeTone::Accent)),
            mention: false,
            selected: self.selected,
            disabled: false,
        }
        .render(Palette::for_mode(self.theme), focus, on_activate)
        .into_any_element()
    }
}
