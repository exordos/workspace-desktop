//! Рисует панель профиля и ведёт переходы между её страницами.
//! Список пунктов повторяет меню Workspace, конечные страницы готовы для подключения настроек.

use gpui::{Context, FocusHandle, Stateful, WeakEntity, Window, div, prelude::*, px};

use crate::gpui_client::{
    avatars::AvatarImages,
    contracts::AccountSession,
    right_panel::{PanelKey, RightPanel, Scope},
    runtime::RuntimeClient,
    ui::{Avatar, Button, ButtonAppearance, Icon, Palette, TextRole, label},
};

/// Пункт меню и страница, которую он открывает.
#[derive(Clone, Copy)]
struct Page {
    id: &'static str,          // Идентификатор для кнопки и диагностического журнала.
    title: &'static str,       // Подпись пункта и заголовок открытой страницы.
    children: &'static [Page], // Пункты на этой странице, если они есть.
}

impl Page {
    /// Описывает конечную страницу, на которую можно перейти из меню.
    const fn leaf(id: &'static str, title: &'static str) -> Self {
        Self {
            id,
            title,
            children: &[],
        }
    }

    /// Описывает страницу, на которой виден список дочерних пунктов.
    const fn group(id: &'static str, title: &'static str, children: &'static [Page]) -> Self {
        Self {
            id,
            title,
            children,
        }
    }
}

const ROOT: Page = Page::group(
    "profile-root",
    "Профиль",
    &[
        Page::leaf("profile-external-accounts", "Внешние аккаунты"),
        Page::leaf("profile-personal-info", "Личная информация"),
        Page::leaf("profile-status", "Статус"),
        Page::group(
            "profile-settings",
            "Настройки",
            &[
                Page::leaf("profile-hotkeys", "Горячие клавиши"),
                Page::leaf("profile-chat-click", "Клик по чату"),
                Page::leaf("profile-notification-sound", "Звуки уведомлений"),
                #[cfg(target_os = "macos")]
                Page::leaf("profile-dock-bounce", "Прыжок значка в Dock"),
                Page::leaf("profile-call-window", "Открывать звонки в отдельном окне"),
                Page::leaf("profile-language", "Язык"),
                Page::leaf("profile-idle-timeout", "Автовыход"),
            ],
        ),
        Page::group(
            "profile-appearance",
            "Внешний вид",
            &[
                Page::leaf("profile-theme", "Настройки темы"),
                Page::leaf("profile-chat-sort", "Порядок стримов и топиков"),
                Page::leaf("profile-folder-layout", "Отображение папок"),
                Page::leaf("profile-chat-density", "Плотность списка чатов"),
            ],
        ),
        Page::leaf("profile-about", "Версия приложения"),
        Page::leaf("profile-diagnostics", "Диагностика"),
        Page::leaf("profile-clear-cache", "Очистить кэш"),
        Page::leaf("profile-logout", "Выйти"),
    ],
);

/// Содержимое одной страницы в общем стеке правых панелей.
pub(super) struct Profile {
    page: Page,                   // Заголовок и дочерние пункты текущей страницы.
    session: AccountSession,      // Вход, которому принадлежат все страницы профиля.
    host: WeakEntity<RightPanel>, // Общий стек окна, который принимает переходы.
    opener: FocusHandle,          // Кнопка профиля, куда можно вернуть фокус.
    avatar: AvatarImages,         // Подписка корневой страницы на фото этого входа.
}

impl Profile {
    /// Открывает профиль выбранного входа или возвращает на предыдущий экран.
    pub(super) fn toggle(
        host: &mut RightPanel,
        session: AccountSession,
        opener: FocusHandle,
        runtime: &RuntimeClient,
        window: &mut Window,
        cx: &mut Context<RightPanel>,
    ) {
        if host.is_active(&Self::key(ROOT, &session)) {
            host.back(window, cx);
        } else {
            Self::open(host, ROOT, session, opener, Some(runtime), window, cx);
        }
    }

    /// Привязывает страницу к поколению аккаунта.
    fn key(page: Page, session: &AccountSession) -> PanelKey {
        PanelKey {
            scope: Scope::Account(session.clone()),
            id: page.id.into(),
        }
    }

    /// Помещает пункт меню в общий стек как самостоятельную страницу.
    fn open(
        host: &mut RightPanel,
        page: Page,
        session: AccountSession,
        opener: FocusHandle,
        runtime: Option<&RuntimeClient>,
        window: &mut Window,
        cx: &mut Context<RightPanel>,
    ) {
        let key = Self::key(page, &session);
        let weak = cx.weak_entity();
        log::debug!("event=workspace_profile_page_opened page={}", page.id);
        host.open(
            key,
            page.title,
            opener.clone(),
            |cx| {
                cx.new(|cx| {
                    let mut avatar = AvatarImages::default();
                    if let Some(runtime) = runtime {
                        avatar.refresh(session.user_uuid(), runtime, &session, cx);
                    }
                    Self {
                        page,
                        session,
                        host: weak,
                        opener,
                        avatar,
                    }
                })
            },
            window,
            cx,
        );
    }

    /// Рисует кнопку перехода к дочерней странице.
    fn row(&self, page: Page, palette: Palette) -> Stateful<gpui::Div> {
        let host = self.host.clone();
        let session = self.session.clone();
        let opener = self.opener.clone();
        let mut button = Button::new(page.id, page.title);
        button.appearance = ButtonAppearance::Ghost;
        button
            .render(palette, None, move |window, app| {
                let _ = host.update(app, |host, cx| {
                    Self::open(
                        host,
                        page,
                        session.clone(),
                        opener.clone(),
                        None,
                        window,
                        cx,
                    );
                });
            })
            .debug_selector(move || page.id.into())
            .min_h(px(40.))
            .h_auto()
            .justify_start()
            .child(
                Icon::ChevronRight
                    .element(px(16.), palette.text_secondary)
                    .ml_auto(),
            )
    }
}

impl Render for Profile {
    /// Показывает дочерние пункты и доступные сведения текущей страницы.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let page = self.page;
        div()
            .id("profile-page")
            .debug_selector(move || format!("{}-page", page.id))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_2()
            .when(page.id == ROOT.id, |body| {
                let mut avatar = Avatar::from_initials("");
                avatar.image = self
                    .session
                    .user_uuid()
                    .and_then(|id| self.avatar.image(id, cx));
                let has_image = avatar.image.is_some();
                body.child(
                    div()
                        .p_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(avatar.render(palette).when(has_image, |photo| {
                            photo.debug_selector(|| "profile-root-avatar-photo".into())
                        }))
                        .child(
                            div()
                                .min_w_0()
                                .child(label(palette, TextRole::Label, "Текущий сервер"))
                                .child(label(
                                    palette,
                                    TextRole::Caption,
                                    self.session.owner.server.clone(),
                                )),
                        ),
                )
            })
            .when(page.id == "profile-about", |body| {
                body.child(label(palette, TextRole::Body, env!("CARGO_PKG_VERSION")).p_2())
            })
            .children(page.children.iter().map(|page| self.row(*page, palette)))
    }
}
