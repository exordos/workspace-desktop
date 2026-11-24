//! Открывает карточку собеседника из каталога, сохраняя видимую переписку.

use std::rc::Rc;

use gpui::{
    App, ClipboardItem, Context, Entity, FocusHandle, IntoElement, Render, Subscription, Task,
    WeakEntity, Window, div, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{directory::UserProfile, messenger::details::sidebar_profile_link};

use super::{
    Conversation,
    presentation::peer_card,
    view::{self, Availability, PanelAction, PanelContent},
};
use crate::gpui_client::{
    avatars::AvatarImages,
    contracts::{AccountSession, ConversationAddress},
    preferences::Preferences,
    right_panel::{PanelKey, RightPanel, Scope},
    runtime::RuntimeClient,
    ui::{Palette, TextRole, label},
};

/// Проверенный экраном источник открытия карточки.
pub(in crate::gpui_client) struct ContactRequest {
    pub session: AccountSession, // Вход каталога, закрывающий карточку при смене аккаунта.
    pub target: ContactTarget,   // Личка каталога или UUID из ссылки в текущем разговоре.
    pub opener: FocusHandle,     // Строка каталога для возврата фокуса.
    pub message: Option<Rc<dyn Fn(&mut App)>>, // Действие для существующей личной переписки.
    pub origin: Option<ContactOrigin>, // Строка истории, которая должна оставаться доступной.
}

/// Разрешение на показ карточки из конкретной строки разговора.
pub(in crate::gpui_client) struct ContactOrigin {
    pub view: WeakEntity<Conversation>, // Разговор, из которого начался запрос.
    pub visible: Rc<dyn Fn(&Conversation) -> bool>, // Проверяет версию сообщения и доступ к ней.
}

impl ContactRequest {
    /// Карточка каталога зависит от входа, а упоминание ещё и от исходной строки.
    fn is_visible(&self, app: &App) -> bool {
        self.origin.as_ref().is_none_or(|origin| {
            origin
                .view
                .upgrade()
                .is_some_and(|view| (origin.visible)(view.read(app)))
        })
    }
}

/// Источник профиля и контекст, в котором карточка остаётся открытой.
#[derive(Clone)]
pub(in crate::gpui_client) enum ContactTarget {
    Direct(Uuid), // Личный чат в каталоге выбранного входа.
    Mention {
        address: ConversationAddress, // Полный адрес исходного разговора.
        user_id: Uuid,                // UUID пользователя, указанный в Markdown.
        row: Uuid,                    // Источник открытия среди сообщений разговора.
    },
}

/// Публичные сведения о контакте, открытые независимо от текущей переписки.
pub(in crate::gpui_client) struct ContactPanel {
    request: ContactRequest,      // Исходный вход и действие открытия переписки.
    host: WeakEntity<RightPanel>, // Панель, которая восстанавливает прежний экран.
    profile: Option<(Uuid, UserProfile)>, // Подтверждённый UUID и публичный профиль.
    avatars: AvatarImages,        // Общий кэш фотографии собеседника.
    copied: Option<usize>,        // Номер последнего скопированного поля.
    shared: bool,                 // Показывает подтверждение копирования ссылки.
    finished: bool,               // Запрос завершён. Без профиля отображается ошибка.
    _load: Task<()>,              // Закрытие карточки освобождает ожидающий запрос.
    _source: Option<Subscription>, // Сбрасывает сведения при изменении исходной строки.
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
        let (scope, id) = match &request.target {
            ContactTarget::Direct(stream) => (
                Scope::Account(request.session.clone()),
                format!("catalog-contact-{stream}"),
            ),
            ContactTarget::Mention {
                address,
                user_id,
                row,
            } => (
                Scope::Conversation(address.clone()),
                format!("message-contact-{row}-{user_id}"),
            ),
        };
        let key = PanelKey {
            scope,
            id: gpui::SharedString::from(id).into(),
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
        let target = request.target.clone();
        let observe = request
            .origin
            .as_ref()
            .and_then(|origin| origin.view.upgrade())
            .map(|origin| {
                cx.observe(&origin, |view, _, cx| {
                    if !view.request.is_visible(cx) {
                        view.profile = None;
                        view.avatars.clear();
                        view.finished = true;
                        cx.notify();
                    }
                })
            });
        let load = cx.spawn(async move |view, cx| {
            let result = runtime
                .request(move |services| async move {
                    match target {
                        ContactTarget::Direct(stream_id) => services
                            .messenger
                            .sidebar_contact(
                                &services.sync,
                                &session.owner,
                                session.generation,
                                stream_id,
                            )
                            .await
                            .ok()
                            .map(|peer| (peer.user_id, peer.profile)),
                        ContactTarget::Mention { user_id, .. } => services
                            .messenger
                            .reference_user_profile(
                                &services.sync,
                                &session.owner,
                                session.generation,
                                user_id,
                            )
                            .await
                            .ok()
                            .flatten()
                            .map(|profile| (user_id, profile)),
                    }
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                view.profile = result
                    .ok()
                    .flatten()
                    .filter(|_| view.request.is_visible(cx));
                if let Some((user_id, _)) = &view.profile {
                    view.avatars
                        .refresh([*user_id], &runtime, &view.request.session, cx);
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
            _source: observe,
        }
    }

    /// Выполняет действие только в открытой карточке подтверждённого профиля.
    fn run(&mut self, action: PanelAction, window: &mut Window, cx: &mut Context<Self>) {
        if self.profile.is_none() || !self.request.is_visible(cx) {
            return;
        }
        match action {
            PanelAction::Copy { field, value } => {
                cx.write_to_clipboard(ClipboardItem::new_string(value));
                self.copied = Some(field);
                cx.notify();
            }
            PanelAction::SharePeer => {
                if let Some((user_id, _)) = &self.profile
                    && let Ok(url) = sidebar_profile_link(&self.request.session.owner, *user_id)
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(url.to_string()));
                    self.shared = true;
                    cx.notify();
                }
            }
            PanelAction::MessagePeer if let Some(message) = &self.request.message => {
                let _ = self.host.update(cx, |host, cx| host.pop(window, cx));
                message(cx);
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
        let body = if let Some((user_id, profile)) = self
            .profile
            .as_ref()
            .filter(|_| self.request.is_visible(cx))
        {
            let mut card = peer_card(
                *user_id,
                profile,
                self.avatars.image(*user_id, cx),
                language,
            );
            card.actions.message = if self.request.message.is_some() {
                Availability::Enabled
            } else {
                Availability::Hidden
            };
            card.actions.share =
                if sidebar_profile_link(&self.request.session.owner, *user_id).is_ok() {
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
