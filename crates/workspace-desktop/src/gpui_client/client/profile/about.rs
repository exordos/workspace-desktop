//! Показывает сведения о версии и ведёт на экран лицензий зависимостей клиента.

use gpui::{App, Context, Div, Entity, WeakEntity, Window, div, prelude::*, px};

use super::super::Client;
use crate::gpui_client::preferences::Preferences;
use crate::gpui_client::ui::{
    Button, ButtonAppearance, Icon, Palette, TextInput, TextInputChanged, TextRole, activate,
    ellipsized_label, label,
};

const LICENSES: &str = include_str!("../../../../assets/open-source-licenses.tsv");

/// Боковая страница версии и действий, доступных для этой сборки.
pub(super) struct About {
    client: WeakEntity<Client>, // Окно для перехода к списку лицензий.
    checked: bool,              // Пользователь запросил проверку без настроенного источника.
}

impl About {
    /// Создаёт новую страницу с актуальной версией приложения.
    pub(super) fn new(client: WeakEntity<Client>) -> Self {
        Self {
            client,
            checked: false,
        }
    }
}

impl Render for About {
    /// Рисует три карточки с информацией по версии, обновлениям и лицензиям.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read(cx).language;
        let client = self.client.clone();
        let version = format!(
            "{} {}",
            language.text("Текущая версия:", "Current version:"),
            env!("CARGO_PKG_VERSION")
        );
        let mut check = Button::new(
            "profile-update-check",
            language.text("Проверить обновления", "Check for updates"),
        );
        check.appearance = ButtonAppearance::Primary;
        let about = cx.weak_entity();
        let check = check
            .render(palette, None, move |_, app| {
                let _ = about.update(app, |view, cx| {
                    view.checked = true;
                    log::debug!("event=workspace_update_check result=unconfigured");
                    cx.notify();
                });
            })
            .w_full()
            .debug_selector(|| "profile-update-check".into());
        let licenses = card(palette)
            .id("profile-licenses")
            .debug_selector(|| "profile-licenses".into())
            .flex_row()
            .items_center()
            .justify_between()
            .gap_1()
            .hover(|style| style.bg(palette.card_hover))
            .child(
                summary(
                    palette,
                    Icon::NewWindow,
                    language.text("Open Source лицензии", "Open Source licenses"),
                    language.text("Открыть", "Open"),
                    false,
                )
                .min_w_0(),
            )
            .child(Icon::ChevronRight.element(px(18.), palette.text_muted));
        div()
            .id("profile-about-page")
            .debug_selector(|| "profile-about-page".into())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_6()
            .py_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(card(palette).child(summary(
                        palette,
                        Icon::Info,
                        language.text("Версия приложения", "App version"),
                        &version,
                        false,
                    )))
                    .child(
                        card(palette)
                            .gap_3()
                            .child(summary(
                                palette,
                                Icon::Download,
                                language.text("Обновление приложения", "App update"),
                                if self.checked {
                                    language.text(
                                        "Проверка обновлений недоступна в этой сборке",
                                        "Update checks are unavailable in this build",
                                    )
                                } else {
                                    language.text(
                                        "Проверьте, доступна ли новая версия",
                                        "Check if a new version is available",
                                    )
                                },
                                true,
                            ))
                            .child(check),
                    )
                    .child(activate(licenses, None, palette, move |window, app| {
                        let _ = client.update(app, |client, cx| client.open_licenses(window, cx));
                    })),
            )
    }
}

/// Задаёт форму карточек страницы версии без поведения клика.
fn card(palette: Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .p_4()
        .rounded(px(10.))
        .border_1()
        .border_color(palette.border)
        .bg(palette.card)
}

/// Рисует круглый знак, название и пояснение для одной карточки.
fn summary(palette: Palette, icon: Icon, title: &str, detail: &str, top: bool) -> Div {
    div()
        .flex()
        .map(|row| {
            if top {
                row.items_start()
            } else {
                row.items_center()
            }
        })
        .gap_3()
        .child(
            div()
                .size(px(36.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(palette.surface)
                .child(icon.element(px(18.), palette.accent)),
        )
        .child(
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .child(label(palette, TextRole::Label, title.to_owned()))
                .child(
                    label(palette, TextRole::Caption, detail.to_owned())
                        .mt(px(2.))
                        .line_height(px(16.))
                        .text_color(palette.text_muted),
                ),
        )
}

/// Полноэкранный список лицензий пакетов нативного клиента.
pub(in crate::gpui_client::client) struct Licenses {
    search: Entity<TextInput>,    // Запрос к имени пакета или автору.
    client: WeakEntity<Client>,   // Окно для возврата на предыдущую страницу.
    filter: Option<&'static str>, // Выбранный тип лицензии или все пакеты.
}

impl Licenses {
    /// Создаёт фильтр и подписывает таблицу на изменение поиска.
    pub(in crate::gpui_client::client) fn new(
        client: WeakEntity<Client>,
        cx: &mut Context<Self>,
    ) -> Self {
        let language = Preferences::read(cx).language;
        let search = cx.new(|cx| {
            TextInput::new(
                "licenses-search",
                language.text("Поиск пакетов…", "Search packages…"),
                false,
                cx,
            )
            .unframed()
        });
        cx.subscribe(&search, |_, _, _: &TextInputChanged, cx| cx.notify())
            .detach();
        Self {
            search,
            client,
            filter: None,
        }
    }

    /// Предлагает искать пакеты сразу после перехода из боковой панели.
    pub(in crate::gpui_client::client) fn focus_search(&self, window: &mut Window, app: &App) {
        self.search.read(app).focus(window);
    }

    /// Рисует строку компактной таблицы с прокручиваемыми данными.
    fn row(
        palette: Palette,
        [name, version, license, author]: [&'static str; 4],
        header: bool,
    ) -> Div {
        let color = Palette {
            text: palette.text_muted,
            ..palette
        };
        let role = if header {
            TextRole::Caption
        } else {
            TextRole::Body
        };
        let package = if header {
            label(color, role, name).into_any_element()
        } else {
            let url = if name == "emoji-picker-react" {
                format!("https://www.npmjs.com/package/{name}")
            } else {
                format!("https://crates.io/crates/{name}")
            };
            let accent = Palette {
                text: palette.accent,
                ..palette
            };
            activate(
                div()
                    .id(gpui::SharedString::from(format!(
                        "license-{name}-{version}"
                    )))
                    .cursor_pointer()
                    .child(ellipsized_label(accent, role, name)),
                None,
                palette,
                move |_, app| app.open_url(&url),
            )
            .into_any_element()
        };
        let license = label(palette, TextRole::Caption, license)
            .line_height(px(16.))
            .whitespace_nowrap()
            .truncate();
        div()
            .flex()
            .items_center()
            .min_h(if header { px(26.) } else { px(37.) })
            .when(!header, |row| row.border_t_1().border_color(palette.border))
            .child(
                div()
                    .w(gpui::relative(0.382))
                    .min_w_0()
                    .pr_2()
                    .child(package),
            )
            .child(
                div()
                    .w(gpui::relative(0.192))
                    .min_w_0()
                    .pr_2()
                    .child(ellipsized_label(color, role, version)),
            )
            .child(
                div()
                    .w(gpui::relative(0.146))
                    .min_w_0()
                    .pr_2()
                    .flex()
                    .child(
                        license.when(!header, |chip| chip.px_2().rounded(px(4.)).bg(palette.card)),
                    ),
            )
            .child(
                div()
                    .w(gpui::relative(0.28))
                    .min_w_0()
                    .child(ellipsized_label(color, role, author)),
            )
    }

    /// Возвращается к предыдущему рабочему экрану и его пункту фокуса.
    fn close(&self, window: &mut Window, app: &mut App) {
        let _ = self
            .client
            .update(app, |client, cx| client.close_licenses(window, cx));
    }

    /// Сортирует типы лицензий по числу пакетов, как сводка Electron.
    fn types(rows: &[[&'static str; 4]]) -> Vec<(&'static str, usize)> {
        let mut counts = std::collections::BTreeMap::new();
        for row in rows {
            *counts.entry(row[2]).or_default() += 1;
        }
        let mut counts = counts.into_iter().collect::<Vec<_>>();
        counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        counts.truncate(8);
        counts
    }

    /// Показывает одну кнопку фильтра. Повторный клик возвращает весь список.
    fn chip(
        &self,
        kind: Option<&'static str>,
        text: String,
        palette: Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let active = kind.is_some() && self.filter == kind;
        let link = cx.weak_entity();
        activate(
            div()
                .id(gpui::SharedString::from(
                    kind.unwrap_or("licenses-reset-filter"),
                ))
                .px_3()
                .py_1()
                .rounded_full()
                .bg(if active { palette.accent } else { palette.card })
                .hover(|style| style.bg(palette.card_hover))
                .child(
                    label(palette, TextRole::Caption, text)
                        .line_height(px(16.))
                        .text_color(if active {
                            palette.on_accent
                        } else {
                            palette.text_secondary
                        }),
                ),
            None,
            palette,
            move |_, app| {
                let _ = link.update(app, |view, cx| {
                    view.filter = kind.filter(|_| view.filter != kind);
                    cx.notify();
                });
            },
        )
    }
}

impl Render for Licenses {
    /// Отображает реальные пакеты и убирает шапку основного мессенджера.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read(cx).language;
        let query = self.search.read(cx).text().trim().to_lowercase();
        let rows = LICENSES
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                Some([
                    fields.next()?,
                    fields.next()?,
                    fields.next()?,
                    fields.next()?,
                ])
            })
            .collect::<Vec<_>>();
        let total = rows.len();
        let filtered = rows
            .iter()
            .filter(|row| {
                self.filter.is_none_or(|kind| row[2] == kind)
                    && (query.is_empty()
                        || row[0].to_lowercase().contains(&query)
                        || row[3].to_lowercase().contains(&query))
            })
            .collect::<Vec<_>>();
        let close = cx.weak_entity();
        let mut close_button = Button::new("profile-licenses-close", "");
        close_button.icon = Some(Icon::Close);
        close_button.appearance = ButtonAppearance::Ghost;
        let close_button = close_button
            .render(
                Palette {
                    text: palette.text_muted,
                    ..palette
                },
                None,
                move |window, app| {
                    if let Some(view) = close.upgrade() {
                        let client = view.read(app).client.clone();
                        let _ = client.update(app, |client, cx| client.close_licenses(window, cx));
                    }
                },
            )
            .size(px(36.))
            .debug_selector(|| "profile-licenses-close".into());
        div()
            .id("profile-licenses-page")
            .debug_selector(|| "profile-licenses-page".into())
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_4()
            .px_6()
            .py_4()
            .bg(palette.background)
            .on_key_down(cx.listener(|view, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    view.close(window, cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .child(
                                label(
                                    palette,
                                    TextRole::Heading,
                                    language.text("Open Source лицензии", "Open Source licenses"),
                                )
                                .text_size(px(20.))
                                .line_height(px(28.)),
                            )
                            .child(
                                label(
                                    palette,
                                    TextRole::Body,
                                    format!(
                                        "Workspace {} {total} {}",
                                        language.text("использует", "uses"),
                                        language
                                            .text("open source пакетов", "open source packages")
                                    ),
                                )
                                .mt_1()
                                .text_color(palette.text_muted),
                            ),
                    )
                    .child(close_button),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(Self::types(&rows).into_iter().map(|(kind, count)| {
                        self.chip(Some(kind), format!("{kind} ({count})"), palette, cx)
                    }))
                    .children(self.filter.map(|_| {
                        self.chip(None, language.text("Сбросить", "Reset").into(), palette, cx)
                    })),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(448.))
                    .h(px(40.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_4()
                    .rounded(px(6.))
                    .bg(palette.field)
                    .child(div().flex_1().min_w_0().child(self.search.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        label(
                            palette,
                            TextRole::Caption,
                            format!(
                                "{} {} {total} {}",
                                filtered.len(),
                                language.text("из", "of"),
                                language.text("пакетов", "packages")
                            ),
                        )
                        .text_color(palette.text_muted),
                    )
                    .child(Self::row(
                        palette,
                        [
                            language.text("Пакет", "Package"),
                            language.text("Версия", "Version"),
                            language.text("Лицензия", "License"),
                            language.text("Автор", "Author"),
                        ],
                        true,
                    ))
                    .child(
                        div()
                            .id("licenses-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .children(
                                filtered
                                    .into_iter()
                                    .map(|row| Self::row(palette, *row, false)),
                            ),
                    ),
            )
    }
}
