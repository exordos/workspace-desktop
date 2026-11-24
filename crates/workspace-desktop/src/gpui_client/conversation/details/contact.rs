//! Открывает карточку собеседника из каталога, сохраняя видимую переписку.

use std::rc::Rc;

use gpui::{
    App, ClipboardItem, Context, Entity, FocusHandle, IntoElement, Render, Task, WeakEntity,
    Window, div, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::messenger::details::{DirectSidebar, sidebar_profile_link};

use super::{
    presentation::peer_card,
    view::{self, Availability, PanelAction, PanelContent},
};
use crate::gpui_client::{
    avatars::AvatarImages,
    contracts::AccountSession,
    preferences::Preferences,
    right_panel::{PanelKey, RightPanel, Scope},
    runtime::RuntimeClient,
    ui::{Palette, TextRole, label},
};

/// Проверенный экраном источник открытия карточки.
pub(in crate::gpui_client) struct ContactRequest {
    pub session: AccountSession, // Вход каталога, закрывающий карточку при смене аккаунта.
    pub stream_id: Uuid,         // Личный чат, подтверждающий собеседника для Core.
    pub opener: FocusHandle,     // Строка каталога для возврата фокуса.
    pub message: Rc<dyn Fn(&mut App)>, // Возвращает намерение открыть личный чат каталогу.
}

/// Публичные сведения о контакте, открытые независимо от текущей переписки.
pub(in crate::gpui_client) struct ContactPanel {
    request: ContactRequest,      // Исходный вход и действие открытия переписки.
    host: WeakEntity<RightPanel>, // Панель, которая восстанавливает прежний экран.
    profile: Option<DirectSidebar>, // Подтверждённый профиль при успешной загрузке.
    avatars: AvatarImages,        // Общий кэш фотографии собеседника.
    copied: Option<usize>,        // Номер последнего скопированного поля.
    shared: bool,                 // Показывает подтверждение копирования ссылки.
    finished: bool,               // Запрос завершён. Без профиля отображается ошибка.
    _load: Task<()>,              // Закрытие карточки освобождает ожидающий запрос.
}

impl ContactPanel {
    /// Использует рамку общего правого стека и оставляет текущий маршрут.
    pub(in crate::gpui_client) fn open(
        host: &Entity<RightPanel>,
        runtime: RuntimeClient,
        request: ContactRequest,
        window: &mut Window,
        app: &mut App,
    ) {
        let key = PanelKey {
            scope: Scope::Account(request.session.clone()),
            id: gpui::SharedString::from(format!("catalog-contact-{}", request.stream_id)).into(),
        };
        let title_key = "desktop.sidebar.actions.contactInfo";
        let language = Preferences::read_for(app, &request.session.owner).language;
        host.update(app, |panel, cx| {
            let host = cx.weak_entity();
            let opener = request.opener.clone();
            panel.open(
                key.clone(),
                language.tr(title_key),
                opener,
                move |cx| cx.new(move |cx| Self::new(host, runtime, request, cx)),
                window,
                cx,
            );
            if panel.is_active(&key) {
                panel.set_translation_key(title_key);
            }
        });
    }

    /// Привязывает загрузку профиля и фото к времени жизни карточки.
    fn new(
        host: WeakEntity<RightPanel>,
        runtime: RuntimeClient,
        request: ContactRequest,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = request.session.clone();
        let stream_id = request.stream_id;
        let load = cx.spawn(async move |view, cx| {
            let result = runtime
                .request(move |services| async move {
                    services
                        .messenger
                        .sidebar_contact(
                            &services.sync,
                            &session.owner,
                            session.generation,
                            stream_id,
                        )
                        .await
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                view.profile = result.ok().and_then(Result::ok);
                if let Some(peer) = &view.profile {
                    view.avatars
                        .refresh([peer.user_id], &runtime, &view.request.session, cx);
                }
                view.finished = true;
                cx.notify();
            });
        });
        Self {
            request,
            host,
            profile: None,
            avatars: AvatarImages::default(),
            copied: None,
            shared: false,
            finished: false,
            _load: load,
        }
    }

    /// Выполняет действие только в открытой карточке подтверждённого профиля.
    fn run(&mut self, action: PanelAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            PanelAction::Copy { field, value } => {
                cx.write_to_clipboard(ClipboardItem::new_string(value));
                self.copied = Some(field);
                cx.notify();
            }
            PanelAction::SharePeer => {
                if let Some(peer) = &self.profile
                    && let Ok(url) = sidebar_profile_link(&self.request.session.owner, peer.user_id)
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(url.to_string()));
                    self.shared = true;
                    cx.notify();
                }
            }
            PanelAction::MessagePeer => {
                let _ = self.host.update(cx, |host, cx| host.pop(window, cx));
                (self.request.message)(cx);
            }
            _ => {}
        }
    }
}

impl Render for ContactPanel {
    /// Показывает ту же карточку, которую пользователь видит из заголовка лички.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read_for(cx, &self.request.session.owner).language;
        let body = if let Some(peer) = &self.profile {
            let mut card = peer_card(peer, self.avatars.image(peer.user_id, cx), language);
            card.actions.message = Availability::Enabled;
            card.actions.share =
                if sidebar_profile_link(&self.request.session.owner, peer.user_id).is_ok() {
                    Availability::Enabled
                } else {
                    Availability::Disabled
                };
            card.actions.share_copied = self.shared;
            card.copied_fields = self.copied.into_iter().collect();
            let target = cx.weak_entity();
            view::render(
                &PanelContent::Peer(card),
                palette,
                None,
                move |action, window, app| {
                    let _ = target.update(app, |view, cx| view.run(action, window, cx));
                },
            )
        } else {
            let key = if self.finished {
                "desktop.conversation.details.profileUnavailable"
            } else {
                "desktop.conversation.details.loading"
            };
            label(palette, TextRole::Body, language.tr(key))
        };
        div()
            .id("catalog-contact-panel")
            .debug_selector(|| "catalog-contact-panel".into())
            .size_full()
            .overflow_y_scroll()
            .p(px(8.))
            .child(body)
    }
}
