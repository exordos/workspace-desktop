//! Передаёт действия ссылок из истории выбранному разговору, оболочке и правой панели.

use std::rc::Rc;

use gpui::{
    App, Context, Deferred, EventEmitter, FocusHandle, Pixels, Point, Render, SharedString,
    WeakEntity, Window, div, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::messenger::references::{
    WorkspaceReferenceTarget as Target, message_permalink,
};

use super::{
    ContactOrigin, ContactPanel, ContactRequest, ContactTarget, Conversation, GuardedText,
    OpenForwardOriginal, inline_references::MentionNames, protected_body::ProtectedInline,
};
use crate::gpui_client::{
    contracts::{ConversationAddress, ConversationReference},
    preferences::{Language, Preferences},
    right_panel::{PanelKey, RightPanel, Scope},
    sidebar::order::sort_topics,
    ui::{
        Button, ButtonAppearance, Palette, TextRole, label,
        popup::{MenuInput, menu_layer, menu_navigation, menu_surface},
        text_role,
    },
};
use crate::platform::message_time::{format_local_time, format_local_time_zone};

/// Ссылка, для которой оболочка подбирает точный адрес в текущем каталоге.
pub(in crate::gpui_client) struct OpenConversationReference {
    pub address: ConversationAddress, // Экран и вход, на которых нажали на ссылку.
    pub target: ConversationReference, // Цель Workspace из сообщения или карточки участника.
}

impl EventEmitter<OpenConversationReference> for Conversation {}

/// Открытая панель момента из конкретной строки истории.
pub(super) struct TimeReference {
    source: ReferenceSource, // Исходная ссылка или отзываемая позиция в раскрытом теле.
    anchor: Point<Pixels>,   // Точка открытия в окне.
    focus: FocusHandle,      // Удерживает Escape и Tab внутри всплывающей панели.
}

/// Подтверждает позднее действие на том же элементе обычного или защищённого сообщения.
#[derive(Clone)]
enum ReferenceSource {
    Open {
        row: Uuid,      // Строка, которая должна оставаться в текущем окне истории.
        target: Target, // Та же цель должна сохраниться после синхронизации.
    },
    Protected(ProtectedInline), // Отзываемая позиция без открытого URL или момента времени.
}

impl ReferenceSource {
    /// Возвращает исходный бабл для ключа правой панели.
    fn row(&self) -> Uuid {
        match self {
            Self::Open { row, .. } => *row,
            Self::Protected(source) => source.row,
        }
    }

    /// Повторяет проверку разрешения и версии перед действием и показом результата.
    fn is_current(&self, view: &Conversation) -> bool {
        view.has_access()
            && match self {
                Self::Open { row, target } => view.viewport.has_inline_reference(*row, target),
                Self::Protected(source) => view
                    .viewport
                    .has_protected_inline_source(source.row, &source.access),
            }
    }
}

impl Conversation {
    /// Выполняет только действие, которое всё ещё принадлежит этому открытому сообщению.
    pub(super) fn activate_inline_reference(
        &mut self,
        row: Uuid,
        target: &Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_reference(
            ReferenceSource::Open {
                row,
                target: target.clone(),
            },
            target,
            window,
            cx,
        );
    }

    /// Читает разрешённую цель из защищённого тела только на время явного нажатия.
    pub(super) fn activate_protected_reference(
        &mut self,
        source: &ProtectedInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = source.target() {
            self.activate_reference(
                ReferenceSource::Protected(source.clone()),
                &target,
                window,
                cx,
            );
        }
    }

    /// Направляет действие из проверенной ссылки во владеющий им модуль.
    fn activate_reference(
        &mut self,
        source: ReferenceSource,
        target: &Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !source.is_current(self) {
            return;
        }
        self.forward_original = None;
        match target {
            Target::User { user_id, .. } => {
                self.open_reference_contact(source, *user_id, window, cx)
            }
            Target::Message(id) => self.open_referenced_message(source, *id, cx),
            Target::Stream(id) => self.open_referenced_stream(source, *id, window, cx),
            Target::Topic {
                stream_id,
                topic_id,
            } => self.emit_conversation_reference(
                ConversationReference::Topic {
                    stream_id: *stream_id,
                    topic_id: *topic_id,
                },
                cx,
            ),
            Target::Url(url) => {
                if let Some(id) = self.inline_permalink(url) {
                    self.open_referenced_message(source, id, cx);
                } else {
                    cx.open_url(url);
                }
            }
            Target::Time(_) => {
                let focus = cx.focus_handle();
                window.focus(&focus);
                self.time_reference = Some(TimeReference {
                    source,
                    anchor: window.mouse_position(),
                    focus,
                });
                cx.notify();
            }
        }
    }

    /// Распознаёт ссылку на сообщение этого аккаунта по его штатному формату копирования.
    fn inline_permalink(&self, url: &str) -> Option<Uuid> {
        let (_, uuid) = url.rsplit_once('/')?;
        let id = Uuid::parse_str(uuid).ok()?;
        (message_permalink(&self.address.route.owner, id).as_deref() == Some(url)).then_some(id)
    }

    /// Проверяет размещение сообщения через Core, прежде чем менять открытую историю.
    fn open_referenced_message(
        &mut self,
        source: ReferenceSource,
        id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.avatar_runtime.clone() else {
            return;
        };
        let address = self.address.clone();
        self.forward_original = Some(cx.spawn(async move |entity, cx| {
            let reply = runtime
                .conversation(address, move |services, address| async move {
                    services
                        .messenger
                        .resolve_forward_original(
                            &services.sync,
                            &address.route.owner,
                            address.generation,
                            id,
                        )
                        .await
                })
                .await;
            let _ = entity.update(cx, |view, cx| {
                view.forward_original = None;
                if let Ok(Ok(scope)) = reply.value
                    && view.address == reply.address
                    && source.is_current(view)
                {
                    cx.emit(OpenForwardOriginal {
                        address: reply.address,
                        scope,
                        id,
                    });
                }
            });
        }));
    }

    /// Просит оболочку найти цель среди маршрутов того же аккаунта.
    fn emit_conversation_reference(&self, target: ConversationReference, cx: &mut Context<Self>) {
        cx.emit(OpenConversationReference {
            address: self.address.clone(),
            target,
        });
    }

    /// Предлагает выбрать тему канала. Личная переписка использует прямой маршрут.
    fn open_referenced_stream(
        &self,
        source: ReferenceSource,
        stream: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((panel, chat)) = self.details.as_ref().and_then(|panel| {
            let chat = panel
                .catalog_chats()
                .iter()
                .find(|chat| chat.id == stream && !chat.direct)?;
            Some((panel, chat))
        }) else {
            self.emit_conversation_reference(ConversationReference::Stream(stream), cx);
            return;
        };
        let preferences = Preferences::read_for(cx, &self.address.route.owner);
        let topics = sort_topics(chat, preferences.appearance.sort)
            .into_iter()
            .filter(|topic| !topic.name.trim().is_empty())
            .map(|topic| (topic.id, SharedString::from(topic.name.clone())))
            .collect();
        let key = PanelKey {
            scope: Scope::Conversation(self.address.clone()),
            id: SharedString::from(format!("stream-reference-{}-{stream}", source.row())).into(),
        };
        let title = chat.name.clone();
        let owner = self.address.route.owner.clone();
        let conversation = cx.weak_entity();
        panel.modal_host().update(cx, |host, cx| {
            let weak = cx.weak_entity();
            host.open(
                key,
                title,
                self.history_focus.clone(),
                move |cx| {
                    cx.new(move |_| StreamTopics {
                        host: weak,
                        conversation,
                        owner,
                        stream,
                        source,
                        topics,
                    })
                },
                window,
                cx,
            );
        });
    }

    /// Открывает обычную карточку контакта по UUID из ссылки и сохраняет контекст источника.
    fn open_reference_contact(
        &self,
        source: ReferenceSource,
        user_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((panel, runtime)) = self.details.as_ref().zip(self.avatar_runtime.clone()) else {
            return;
        };
        let message = panel
            .catalog_chats()
            .iter()
            .any(|chat| chat.direct && chat.metadata.direct_peer_id == Some(user_id))
            .then(|| {
                let view = cx.weak_entity();
                Rc::new(move |app: &mut App| {
                    let _ = view.update(app, |view, cx| {
                        view.emit_conversation_reference(
                            ConversationReference::Contact(user_id),
                            cx,
                        );
                    });
                }) as Rc<dyn Fn(&mut App)>
            });
        let request = ContactRequest {
            session: self.avatar_session(),
            target: ContactTarget::Mention {
                address: self.address.clone(),
                user_id,
                row: source.row(),
            },
            opener: self.history_focus.clone(),
            message,
            origin: Some(ContactOrigin {
                view: cx.weak_entity(),
                visible: Rc::new(move |view| source.is_current(view)),
            }),
        };
        ContactPanel::open(&panel.modal_host(), runtime, request, window, cx);
    }

    /// Закрывает панель времени и возвращает клавиатуру текущей истории.
    fn close_reference_time(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.time_reference.take().is_some() {
            window.focus(if self.has_access() {
                &self.history_focus
            } else {
                &self.screen_focus
            });
            cx.notify();
        }
    }

    /// Закрывает чтение времени по Escape, удерживая фокус в пределах панели.
    fn time_reference_key(
        &mut self,
        input: MenuInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(input, MenuInput::Close) {
            self.close_reference_time(window, cx);
        }
    }

    /// Показывает локальную дату и часовой пояс в доступной области окна.
    pub(super) fn render_reference_time(
        &mut self,
        window: &mut Window,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Option<[Deferred; 2]> {
        let time = self.time_reference.as_ref()?;
        if !time.source.is_current(self) {
            self.close_reference_time(window, cx);
            return None;
        }
        let locale = Preferences::read_for(cx, &self.address.route.owner).language;
        let texts = match &time.source {
            ReferenceSource::Open {
                target: Target::Time(time),
                ..
            } => {
                let title = format_local_time(time, locale.code());
                let zone = format_local_time_zone(time, locale.code());
                [
                    label(palette, TextRole::Label, title).into_any_element(),
                    label(palette, TextRole::Caption, time_zone_hint(locale, &zone))
                        .into_any_element(),
                ]
            }
            ReferenceSource::Protected(source) => [
                text_role(palette, TextRole::Label)
                    .child(GuardedText::reference(
                        source.access.clone(),
                        source.range.clone(),
                        locale,
                        MentionNames::default(),
                    ))
                    .into_any_element(),
                text_role(palette, TextRole::Caption)
                    .child(GuardedText::reference_zone(
                        source.access.clone(),
                        source.range.clone(),
                        locale,
                    ))
                    .into_any_element(),
            ],
            _ => return None,
        };
        let viewport = window.viewport_size();
        let surface = menu_surface(
            "history-reference-time",
            palette,
            viewport,
            px(280.),
            viewport.height,
        )
        .debug_selector(|| "history-reference-time-panel".into())
        .gap_1()
        .p_3()
        .on_mouse_down_out(cx.listener(|view, _, window, cx| view.close_reference_time(window, cx)))
        .children(texts);
        Some(menu_layer(
            viewport,
            time.anchor,
            menu_navigation(surface, &time.focus, cx, Self::time_reference_key),
        ))
    }
}

/// Выбор темы по ссылке канала, который существует только в исходном открытии разговора.
struct StreamTopics {
    host: WeakEntity<RightPanel>, // Панель, которая держит список и возвращает фокус.
    conversation: WeakEntity<Conversation>, // Владелец исходного сообщения и действия.
    owner: workspace_core::context::OwnerContext, // Аккаунт для локализации пустого списка.
    stream: Uuid,                 // Родитель всех доступных тем.
    source: ReferenceSource,      // При отзыве исходного доступа список закрывается для действий.
    topics: Vec<(Uuid, SharedString)>, // Названия и UUID тем из каталога этого входа.
}

impl StreamTopics {
    /// Закрывает список и передаёт точную выбранную тему штатному маршруту оболочки.
    fn choose(&self, topic: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.conversation.update(cx, |view, cx| {
            if self.source.is_current(view) {
                let _ = self.host.update(cx, |host, cx| host.pop(window, cx));
                view.emit_conversation_reference(
                    ConversationReference::Topic {
                        stream_id: Some(self.stream),
                        topic_id: topic,
                    },
                    cx,
                );
            }
        });
    }
}

impl Render for StreamTopics {
    /// Показывает доступные темы в порядке каталога, пользуясь обычными кнопками Desktop.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read_for(cx, &self.owner).language;
        let view = cx.weak_entity();
        let valid = self
            .conversation
            .upgrade()
            .is_some_and(|view| self.source.is_current(view.read(cx)));
        div()
            .id("history-stream-reference-topics")
            .size_full()
            .overflow_y_scroll()
            .p_2()
            .debug_selector(|| "history-stream-reference-topics".into())
            .flex()
            .flex_col()
            .gap_1()
            .when(!valid || self.topics.is_empty(), |root| {
                root.child(label(
                    palette,
                    TextRole::Caption,
                    language.tr("channel.noTopics"),
                ))
            })
            .children(self.topics.iter().filter(|_| valid).map(|(topic, title)| {
                let view = view.clone();
                let topic = *topic;
                let mut button = Button::new(
                    (gpui::ElementId::from(topic), "stream-reference-topic"),
                    title.clone(),
                );
                button.appearance = ButtonAppearance::Ghost;
                button
                    .render(palette, None, move |window, cx| {
                        let _ = view.update(cx, |view, cx| view.choose(topic, window, cx));
                    })
                    .debug_selector(move || format!("history-stream-reference-topic-{topic}"))
            }))
    }
}

/// Подставляет пояс в ту же строку, которую использует всплывающая подсказка.
pub(super) fn time_zone_hint(language: Language, zone: &str) -> String {
    language.tr_with("message.globalTimeHint", &[("zone", zone)])
}
