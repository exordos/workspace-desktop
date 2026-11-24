//! Собирает корневой сайдбар аккаунта, его страницы и действия.
//! Сначала описаны переходы, затем строки меню и их компоновка.

use gpui::{
    App, Context, Div, Entity, FocusHandle, PromptButton, PromptLevel, SharedString, Stateful,
    WeakEntity, Window, div, prelude::*, px,
};
use workspace_core::directory::{ProfilePresence, UserProfile};

use crate::gpui_client::{
    contracts::AccountSession,
    preferences::{Language, Preferences},
    right_panel::{PanelKey, RightPanel, Scope},
    runtime::RuntimeClient,
    sidebar::Sidebar,
    ui::{Icon, Palette, TextRole, activate, label},
};

use super::Client;
mod about;
mod appearance;
mod general;
mod personal;
mod status;

use about::About;
pub(super) use about::Licenses;
use appearance::Appearance;
use general::General;
pub(super) use general::Hotkeys;
use personal::Personal;
use status::Status;

/// Открывает тот же диалог горячих клавиш из сочетания помощи в рабочем окне.
pub(super) fn open_hotkey_dialog(
    host: &Entity<RightPanel>,
    session: &AccountSession,
    opener: FocusHandle,
    window: &mut Window,
    app: &mut gpui::App,
) {
    General::open_dialog(host.downgrade(), session, opener, window, app);
}

/// Действие корневого меню и подписи для панели.
#[derive(Clone, Copy)]
struct Row {
    id: &'static str,               // Идентификатор действия и открываемого экрана.
    title: &'static str,            // Ключ подписи пункта и заголовка страницы.
    subtitle: Option<&'static str>, // Ключ подписи под названием, если она есть.
    icon: Icon,                     // Значок слева.
    action: Action,                 // Экран или модальное действие, которое откроется.
}

/// Определяет поверхность, которая открывается из строки меню.
#[derive(Clone, Copy)]
enum Action {
    Personal,   // Отдельная страница личной информации внутри сайдбара.
    Status,     // Диалог изменения статуса.
    General,    // Страница общих настроек.
    Appearance, // Страница внешнего вида.
    About,      // Версия, проверка обновлений и лицензии.
    ClearCache, // Подтверждение сброса кэша и настроек.
    Logout,     // Подтверждение выхода из текущей организации.
}

const ROOT_ID: &str = "profile-root";
const ROOT_TITLE: &str = "nav.account";
const PERSONAL: Row = Row {
    id: "profile-personal-info",
    title: "settings.personalInfo",
    subtitle: None,
    icon: Icon::Account,
    action: Action::Personal,
};
const STATUS: Row = Row {
    id: "profile-status",
    title: "settings.status",
    subtitle: Some("settings.statusPlaceholder"),
    icon: Icon::StatusSmile,
    action: Action::Status,
};
const GENERAL: Row = Row {
    id: "profile-settings",
    title: "settings.settings",
    subtitle: Some("desktop.profile.generalSubtitle"),
    icon: Icon::Settings,
    action: Action::General,
};
const APPEARANCE: Row = Row {
    id: "profile-appearance",
    title: "settings.appearance",
    subtitle: Some("desktop.profile.appearanceSubtitle"),
    icon: Icon::Draw,
    action: Action::Appearance,
};
const ABOUT: Row = Row {
    id: "profile-about",
    title: "settings.appVersion",
    subtitle: None,
    icon: Icon::Info,
    action: Action::About,
};
const CLEAR_CACHE: Row = Row {
    id: "profile-clear-cache",
    title: "settings.clearCache",
    subtitle: Some("desktop.profile.clearCacheSubtitle"),
    icon: Icon::Delete,
    action: Action::ClearCache,
};
const LOGOUT: Row = Row {
    id: "profile-logout",
    title: "auth.logout",
    subtitle: None,
    icon: Icon::Logout,
    action: Action::Logout,
};

/// Корневой экран аккаунта и его живые зависимости.
pub(super) struct Profile {
    session: AccountSession,      // Вход, которому принадлежат страницы аккаунта.
    host: WeakEntity<RightPanel>, // Общий стек и оверлей окна.
    sidebar: WeakEntity<Sidebar>, // Каталог для настроек панели папок и открытия избранного.
    runtime: RuntimeClient,       // Операции над выбранным входом.
    opener: FocusHandle,          // Кнопка аккаунта вне сайдбара.
    row_focus: [FocusHandle; 7],  // Фокусы строк, к которым можно вернуться.
    account_label: String,        // Подпись текущего входа.
    client: WeakEntity<Client>,   // Рабочее окно для перехода к лицензиям.
    public_profile: Option<UserProfile>, // Статус и имя из подтверждённого ответа.
    action_error: Option<&'static str>, // Ключ безопасной подписи сбоя локальной команды.
}

/// Живые части окна, которые использует панель аккаунта.
pub(super) struct ProfileServices {
    pub runtime: RuntimeClient,       // Операции над входом.
    pub sidebar: WeakEntity<Sidebar>, // Настройка папок по этому входу.
    pub client: WeakEntity<Client>,   // Переход на главную страницу лицензий.
    pub account_label: String,        // Резервная подпись до чтения собственного профиля.
    pub details: Option<UserProfile>, // Снимок, который уже показывает шапка.
}

impl Profile {
    /// Открывает корень выбранного входа или снимает его по повторному нажатию.
    pub(super) fn toggle(
        host: &Entity<RightPanel>,
        session: AccountSession,
        opener: FocusHandle,
        services: ProfileServices,
        window: &mut Window,
        app: &mut gpui::App,
    ) {
        let weak = host.downgrade();
        host.update(app, |host, cx| {
            if host.toggle_account(&session, window, cx) {
                return;
            }
            let key = Self::key(ROOT_ID, &session);
            let title = Preferences::read(cx).language.tr(ROOT_TITLE);
            host.open(
                key,
                title,
                opener.clone(),
                |cx| {
                    cx.new(|cx| {
                        let profile = Self {
                            session,
                            host: weak,
                            sidebar: services.sidebar,
                            runtime: services.runtime,
                            opener,
                            row_focus: std::array::from_fn(|_| cx.focus_handle()),
                            account_label: services.account_label,
                            client: services.client,
                            public_profile: services.details,
                            action_error: None,
                        };
                        profile.load(cx);
                        profile
                    })
                },
                window,
                cx,
            );
            host.set_translation_key(ROOT_TITLE);
        });
    }

    /// Читает отображаемый статус и логин текущей организации.
    fn load(&self, cx: &Context<Self>) {
        let runtime = self.runtime.clone();
        let session = self.session.clone();
        cx.spawn(async move |view, cx| {
            let result = runtime
                .request(move |services| async move {
                    futures::join!(
                        services.messenger.own_profile(
                            &services.sync,
                            &session.owner,
                            session.generation
                        ),
                        services.messenger.own_identity(
                            &services.sync,
                            &session.owner,
                            session.generation
                        ),
                    )
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                if let Ok((profile, identity)) = result {
                    if view.public_profile.is_none() {
                        view.public_profile = profile.ok();
                    }
                    if let Ok(identity) = identity {
                        view.account_label = identity.email.unwrap_or(identity.username);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Показывает изменения сразу после подтверждённого ответа и запоминает ручное отсутствие.
    fn status_saved(&mut self, profile: UserProfile, away: bool, cx: &mut Context<Self>) {
        self.public_profile = Some(profile.clone());
        let _ = self.client.update(cx, |client, cx| {
            client.set_profile_details(&self.session, profile, cx)
        });
        if Preferences::update(cx, |preferences| preferences.manual_away = Some(away)).is_err() {
            log::warn!("event=workspace_manual_away_saved result=failed");
        }
        cx.notify();
    }

    /// Привязывает страницу к сеансу того же пользователя и проекта.
    fn key(id: &'static str, session: &AccountSession) -> PanelKey {
        PanelKey {
            scope: Scope::Account(session.clone()),
            id: id.into(),
        }
    }

    /// Открывает выбранную страницу или диалог над рабочим окном.
    fn open(&self, row: Row, focus: FocusHandle, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(row.action, Action::ClearCache | Action::Logout) {
            self.confirm(row, window, cx);
            return;
        }
        let weak = self.host.clone();
        let session = self.session.clone();
        let runtime = self.runtime.clone();
        let sidebar = self.sidebar.clone();
        let opener = self.opener.clone();
        let account_label = self.account_label.clone();
        let client = self.client.clone();
        let root = cx.weak_entity();
        let public_profile = self.public_profile.clone();
        let preferences = Preferences::read(cx);
        let away = preferences.manual_away;
        let title = preferences.language.tr(row.title);
        log::debug!("event=workspace_profile_action_opened action={}", row.id);
        let _ = self.host.update(cx, |host, cx| {
            let key = Self::key(row.id, &session);
            match row.action {
                Action::General => host.open(
                    key,
                    title,
                    opener,
                    |cx| cx.new(|cx| General::new(weak, session, runtime, cx)),
                    window,
                    cx,
                ),
                Action::Appearance => host.open(
                    key,
                    title,
                    opener,
                    |cx| cx.new(|cx| Appearance::new(weak, session, sidebar, cx)),
                    window,
                    cx,
                ),
                Action::Personal => host.open(
                    key,
                    title,
                    opener,
                    |cx| {
                        cx.new(|cx| {
                            Personal::new(weak, session, runtime, account_label, client, cx)
                        })
                    },
                    window,
                    cx,
                ),
                Action::About => host.open(
                    key,
                    title,
                    opener,
                    |cx| cx.new(|_| About::new(client)),
                    window,
                    cx,
                ),
                Action::Status => {
                    let mut dialog = None;
                    host.open_modal(&session.clone(), focus, window, cx, |_, cx| {
                        let view = cx.new(|cx| {
                            Status::new(root, weak, session, runtime, public_profile, away, cx)
                        });
                        dialog = Some(view.downgrade());
                        view
                    });
                    if let Some(dialog) = dialog {
                        host.set_modal_dismiss(move |app| {
                            dialog
                                .upgrade()
                                .is_none_or(|view| view.read(app).can_dismiss())
                        });
                    }
                    return;
                }
                Action::ClearCache | Action::Logout => return,
            }
            host.set_parent_opener(focus);
            host.set_translation_key(row.title);
        });
    }

    /// Просит подтверждение локальной очистки или выхода, как оригинальное окно.
    fn confirm(&self, row: Row, window: &mut Window, cx: &mut Context<Self>) {
        let language = Preferences::read(cx).language;
        let cache = matches!(row.action, Action::ClearCache);
        let question = if cache {
            language.tr("settings.clearCacheConfirm").to_owned()
        } else {
            language.tr_with(
                "auth.logoutFromOrgConfirm",
                &[("server", self.server_host())],
            )
        };
        let prompt = window.prompt(
            PromptLevel::Warning,
            &question,
            None,
            &[
                PromptButton::ok(language.tr(row.title)),
                PromptButton::cancel(language.tr("common.cancel")),
            ],
            cx,
        );
        let runtime = self.runtime.clone();
        let session = self.session.clone();
        cx.spawn_in(window, async move |view, cx| {
            if prompt.await != Ok(0) {
                return;
            }
            if cache {
                let ready = cx
                    .update(|_, app| {
                        view.update(app, |view, cx| {
                            if Preferences::reset(cx).is_ok() {
                                true
                            } else {
                                view.action_error = Some("desktop.profile.resetFailed");
                                cx.notify();
                                false
                            }
                        })
                        .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if ready {
                    let result = runtime.reset_cache().await;
                    log::info!("event=workspace_cache_reset result={result:?}");
                    let _ = cx.update(|window, app| {
                        if let Err(error) = restart_after_reset(app) {
                            log::warn!("event=workspace_cache_restart result=failed error={error}");
                            let title = language.tr("desktop.profile.restartFailed");
                            let _prompt = window.prompt(
                                PromptLevel::Critical,
                                title,
                                None,
                                &[PromptButton::ok("OK")],
                                app,
                            );
                        }
                    });
                }
            } else {
                let result = runtime.logout(session).await;
                log::info!("event=workspace_logout_completed result={result:?}");
                if result.is_err() {
                    let _ = cx.update(|_, app| {
                        let _ = view.update(app, |view, cx| {
                            view.action_error = Some("desktop.profile.logoutFailed");
                            cx.notify();
                        });
                    });
                }
            }
        })
        .detach();
    }

    /// Рисует строку меню и подключает одинаковое действие для мыши и клавиатуры.
    fn row(
        &self,
        row: Row,
        index: usize,
        divider: bool,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let focus = &self.row_focus[index];
        let on_focus = focus.clone();
        let view = cx.weak_entity();
        let danger = matches!(row.action, Action::Logout);
        let title_color = if danger { palette.danger } else { palette.text };
        let icon_color = if danger {
            palette.danger
        } else {
            palette.text_secondary
        };
        let has_chevron = !matches!(row.action, Action::Logout | Action::Status);
        let language = Preferences::read(cx).language;
        let title = language.tr(row.title);
        let subtitle = self.row_subtitle(row, language);
        activate(
            div()
                .id(row.id)
                .debug_selector(move || row.id.into())
                .relative()
                .w_full()
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .px_4()
                .py(px(6.))
                .hover(|style| style.bg(palette.card_hover))
                .when(divider, |item| item.child(hairline(palette)))
                .child(
                    div()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(icon_slot(row.icon, icon_color))
                        .child(
                            div()
                                .min_w_0()
                                .child(
                                    label(palette, TextRole::Label, title).text_color(title_color),
                                )
                                .when(!subtitle.is_empty(), |text| {
                                    text.child(
                                        label(palette, TextRole::Body, subtitle)
                                            .mt(px(2.))
                                            .text_color(palette.text_muted),
                                    )
                                }),
                        ),
                )
                .when(has_chevron, |item| {
                    item.child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_2()
                            .when(matches!(row.action, Action::About), |trail| {
                                trail.child(
                                    label(palette, TextRole::Body, env!("CARGO_PKG_VERSION"))
                                        .text_color(palette.text_secondary),
                                )
                            })
                            .child(Icon::ChevronRight.element(px(16.), palette.text_secondary)),
                    )
                }),
            Some(focus),
            palette,
            move |window, app| {
                let _ = view.update(app, |profile, cx| {
                    profile.open(row, on_focus.clone(), window, cx)
                });
            },
        )
    }

    /// Показывает адрес организации и подпись входа в одной строке меню.
    fn server(&self, palette: Palette, language: Language) -> Div {
        let server = self.server_host().to_owned();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_4()
            .py(px(6.))
            .child(icon_slot(Icon::DashboardCustomize, palette.text_secondary))
            .child(
                div()
                    .min_w_0()
                    .child(label(
                        palette,
                        TextRole::Label,
                        language.tr("auth.currentServer"),
                    ))
                    .child(
                        label(palette, TextRole::Body, server)
                            .mt(px(2.))
                            .text_color(palette.text_muted)
                            .truncate(),
                    )
                    .child(
                        label(palette, TextRole::Body, self.account_label.clone())
                            .text_color(palette.text_muted)
                            .truncate(),
                    ),
            )
    }

    /// Убирает схему и служебный порт из подписи текущего сервера.
    fn server_host(&self) -> &str {
        let origin = &self.session.owner.server;
        let host = origin
            .split_once("://")
            .map_or(origin.as_str(), |(_, host)| host);
        if let Some(ipv6) = host.strip_prefix('[').and_then(|host| host.find(']')) {
            &host[..ipv6 + 2]
        } else {
            host.split(['/', ':']).next().unwrap_or_default()
        }
    }

    /// Для статуса показывает серверный текст, эмодзи или текущее присутствие.
    fn row_subtitle(&self, row: Row, language: Language) -> SharedString {
        if matches!(row.action, Action::Status)
            && let Some(profile) = &self.public_profile
        {
            if let Some(status) = profile.status_label() {
                return status.into();
            }
            if let Some(presence) = profile.presence {
                return match presence {
                    ProfilePresence::Active => language.tr("a11y.online"),
                    ProfilePresence::Idle | ProfilePresence::DoNotDisturb => {
                        language.tr("a11y.away")
                    }
                    ProfilePresence::Offline => language.tr("presence.offline"),
                }
                .into();
            }
        }
        row.subtitle.map_or("", |key| language.tr(key)).into()
    }
}

/// Передаёт перезапуск GPUI или повторяет исходный запуск со всеми параметрами.
fn restart_after_reset(app: &mut App) -> std::io::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !args.is_empty() {
        std::process::Command::new(std::env::current_exe()?)
            .args(args)
            .spawn()?;
        app.quit();
    } else {
        app.restart();
    }
    Ok(())
}

impl Render for Profile {
    /// Рисует группы меню с отступами и разделителями, как в сайдбаре Electron.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read(cx).language;
        div()
            .id(ROOT_ID)
            .debug_selector(|| "profile-root-page".into())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .py_5()
                    .child(group(
                        language.tr("settings.sectionDescription"),
                        palette,
                        div()
                            .flex()
                            .flex_col()
                            .child(self.server(palette, language))
                            .child(self.row(PERSONAL, 0, true, palette, cx))
                            .child(self.row(STATUS, 1, true, palette, cx)),
                    ))
                    .child(group(
                        language.tr("settings.settings"),
                        palette,
                        div()
                            .flex()
                            .flex_col()
                            .child(self.row(GENERAL, 2, false, palette, cx))
                            .child(self.row(APPEARANCE, 3, true, palette, cx)),
                    ))
                    .child(group(
                        language.tr("settings.appVersion"),
                        palette,
                        div()
                            .flex()
                            .flex_col()
                            .child(self.row(ABOUT, 4, false, palette, cx))
                            .child(self.row(CLEAR_CACHE, 5, true, palette, cx))
                            .child(self.row(LOGOUT, 6, true, palette, cx))
                            .children(self.action_error.map(|error| {
                                label(palette, TextRole::Caption, language.tr(error))
                                    .text_color(palette.danger)
                                    .p_4()
                            })),
                    )),
            )
    }
}

/// Сохраняет одинаковую позицию значков в строках с разной высотой текста.
fn icon_slot(icon: Icon, color: gpui::Hsla) -> Div {
    div()
        .size(px(32.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(icon.element(px(22.), color))
}

/// Рисует секцию с текстовой подписью над её первой строкой.
fn group(title: &str, palette: Palette, rows: Div) -> Div {
    let title: SharedString = title.to_uppercase().into();
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            label(palette, TextRole::Label, title)
                .px_4()
                .text_size(px(11.))
                .line_height(px(16.))
                .text_color(palette.text_muted),
        )
        .child(rows)
}

/// Отделяет соседние строки, оставляя отступы справа и слева.
fn hairline(palette: Palette) -> Div {
    div()
        .absolute()
        .top_0()
        .left(px(16.))
        .right(px(16.))
        .h(px(1.))
        .bg(palette.border)
}
