//! История рабочего ChatRow с переключателями входных параметров.

use gpui::{
    App, AppContext, Application, Context, Entity, FocusHandle, Focusable, Window, div, prelude::*,
    px,
};
use gpui_storybook::{Assets, Gallery, Story, StoryContainer, create_new_window};
use workspace_desktop::gpui_client::ui::{Button, Palette, TextRole, ThemeMode, label};

use crate::chat_row::{DESCRIPTION, ID, ROW_WIDTH, Sample, fixture};

/// Язык встроенных кнопок Storybook 0.5.0. Содержимое истории остаётся русским.
#[derive(Clone, Copy, Default, strum::EnumIter)]
enum Language {
    #[default]
    English, // Английский набор строк внешней библиотеки.
}

impl From<Language> for unic_langid::LanguageIdentifier {
    /// Возвращает поддерживаемый код языка галереи.
    fn from(_: Language) -> Self {
        unic_langid::langid!("en")
    }
}

impl From<&unic_langid::LanguageIdentifier> for Language {
    /// Возвращает единственный язык этой цели разработки.
    fn from(_: &unic_langid::LanguageIdentifier) -> Self {
        Self::English
    }
}

impl es_fluent::FluentDisplay for Language {
    /// Показывает язык в меню настроек Storybook.
    fn fluent_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("English")
    }
}

/// Переключатели входных данных истории.
#[derive(Clone, Copy)]
enum Control {
    Title,    // Полное название из UI-02 или короткое название.
    Selected, // Выбранная строка или обычное состояние.
    Unread,   // Счётчик из fixture либо отсутствие счётчика.
    Theme,    // Светлая или тёмная палитра.
}

/// Образец строки, открываемый как история и интерактивная панель.
struct ChatRowStory {
    sample: Sample,         // Текущие параметры строки.
    original: Sample,       // Исходные данные из UI-02 для восстановления длинного варианта.
    row_focus: FocusHandle, // Tab и Enter самого рабочего ChatRow.
}

impl ChatRowStory {
    /// Открывает выбранную светлую строку со счётчиком из UI-02.
    fn new(cx: &mut Context<Self>) -> Self {
        let sample = fixture(ThemeMode::Light);
        Self {
            original: sample.clone(),
            sample,
            row_focus: cx.focus_handle(),
        }
    }

    /// Меняет один параметр и перерисовывает ту же строку.
    fn change(&mut self, control: Control, cx: &mut Context<Self>) {
        match control {
            Control::Title => {
                self.sample.title = if self.sample.title == self.original.title {
                    "Дизайн".into()
                } else {
                    self.original.title.clone()
                };
            }
            Control::Selected => self.sample.selected = !self.sample.selected,
            Control::Unread => {
                self.sample.unread = if self.sample.unread == 0 {
                    self.original.unread
                } else {
                    0
                }
            }
            Control::Theme => {
                self.sample.theme = if self.sample.theme == ThemeMode::Light {
                    ThemeMode::Dark
                } else {
                    ThemeMode::Light
                };
            }
        }
        cx.notify();
    }
}

impl Focusable for ChatRowStory {
    /// Возвращает клавиатурный фокус строки истории.
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.row_focus.clone()
    }
}

impl Story for ChatRowStory {
    /// Использует машинное имя, которое принимает и команда запуска.
    fn title() -> String {
        ID.into()
    }

    /// Выводит описание параметров, вариантов и действий в галерее.
    fn description() -> String {
        DESCRIPTION.into()
    }

    /// Создаёт историю на фиксированных данных без подключения рабочего клиента.
    fn new_view(_: &mut Window, cx: &mut App) -> Entity<impl Render + Focusable> {
        cx.new(Self::new)
    }
}

impl Render for ChatRowStory {
    /// Показывает интерактивные параметры, передаваемые общему ChatRow.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(self.sample.theme);
        let controls = [
            ("title", "Длинное / короткое название", Control::Title),
            ("selected", "Выбрать / снять выбор", Control::Selected),
            ("unread", "Показать / скрыть счётчик", Control::Unread),
            ("theme", "Светлая / тёмная тема", Control::Theme),
        ];
        let state = format!(
            "Выбрано: {} · непрочитано: {}",
            if self.sample.selected {
                "да"
            } else {
                "нет"
            },
            self.sample.unread
        );
        let story = cx.entity().downgrade();
        div()
            .min_w_0()
            .w_full()
            .flex()
            .flex_col()
            .p_4()
            .gap_3()
            .bg(palette.surface)
            .child(label(palette, TextRole::Body, DESCRIPTION))
            .child(div().flex().flex_wrap().gap_2().children(controls.map(
                |(id, title, control)| {
                    let story = story.clone();
                    Button::new(id, title)
                        .render(palette, None, move |_, app| {
                            let _ = story.update(app, |view, cx| view.change(control, cx));
                        })
                        .debug_selector(move || id.into())
                },
            )))
            .child(div().w(px(ROW_WIDTH)).max_w_full().child(self.sample.row(
                Some(&self.row_focus),
                move |_, app| {
                    let _ = story.update(app, |view, cx| {
                        view.sample.selected = true;
                        cx.notify();
                    });
                },
            )))
            .child(label(palette, TextRole::Caption, state))
    }
}

/// Запускает штатную галерею Storybook с одной историей по её стабильному ключу.
pub fn run() {
    Application::new().with_assets(Assets).run(|cx| {
        gpui_storybook::init(Language::English, cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        create_new_window(
            "Workspace GPUI Storybook",
            |window, cx| {
                let story = StoryContainer::panel::<ChatRowStory>(window, cx);
                Gallery::view(vec![story], Some(ID), window, cx)
            },
            cx,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyUpEvent, Keystroke, Modifiers, TestAppContext};

    /// Проверяет рабочие действия, которыми оператор меняет параметры истории и выбирает строку.
    #[gpui::test]
    fn controls_update_the_chat_row(cx: &mut TestAppContext) {
        let (view, ui) = cx.add_window_view(|_, cx| ChatRowStory::new(cx));
        ui.run_until_parked();
        for id in ["title", "selected", "unread", "theme"] {
            let control = ui.debug_bounds(id).expect("кнопка настройки");
            ui.simulate_click(control.center(), Modifiers::none());
            ui.run_until_parked();
        }
        ui.read(|app| {
            let sample = &view.read(app).sample;
            assert_eq!(sample.title, "Дизайн");
            assert!(!sample.selected);
            assert_eq!(sample.unread, 0);
            assert_eq!(sample.theme, ThemeMode::Dark);
        });
        ui.update(|window, cx| view.read(cx).focus_handle(cx).focus(window));
        ui.simulate_event(KeyUpEvent {
            keystroke: Keystroke::parse("enter").expect("клавиша выбора строки"),
        });
        ui.run_until_parked();
        ui.read(|app| assert!(view.read(app).sample.selected));
    }
}
