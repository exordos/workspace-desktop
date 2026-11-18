//! Стек правых панелей окна. Хранит открытые экраны, проверяет их контекст и ведёт фокус.

use std::rc::Rc;

use gpui::{
    AnyView, App, Context, Div, ElementId, Entity, EntityId, FocusHandle, KeyBinding, SharedString,
    Stateful, Window, actions, div, prelude::*, px,
};

use super::{
    NextControl, PreviousControl,
    contracts::{AccountSession, ConversationAddress},
    ui::{Button, ButtonAppearance, Icon, Palette, TextRole, label, popup::tooltip},
};

const KEY_CONTEXT: &str = "WorkspaceRightPanel";

actions!(workspace_right_panel, [Back, CloseAll]);

/// Ограничивает время жизни панели контекстом, из которого её открыли.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Scope {
    #[allow(
        dead_code,
        reason = "Панели уровня аккаунта подключаются отдельной фичей"
    )]
    Account(AccountSession), // Доступна при смене разговора внутри этого входа.
    Conversation(ConversationAddress), // Доступна только в этом открытии разговора.
}

/// Отличает экраны по контексту и назначению. Для разных объектов добавляй их ID в ElementId.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct PanelKey {
    pub scope: Scope,  // Владелец экрана и граница его жизни.
    pub id: ElementId, // Ключ экрана внутри контекста.
}

impl PanelKey {
    /// Проверяет, что экран относится к текущему входу и, при необходимости, разговору.
    fn matches(
        &self,
        account: Option<&AccountSession>,
        conversation: Option<&ConversationAddress>,
    ) -> bool {
        match &self.scope {
            Scope::Account(session) => account == Some(session),
            Scope::Conversation(address) => {
                conversation == Some(address)
                    && account.is_some_and(|session| {
                        session.owner == address.route.owner
                            && session.generation == address.generation
                    })
            }
        }
    }
}

type ConversationTitle = Rc<dyn Fn(&App) -> &'static str>;
type ConversationBack = Rc<dyn Fn(&mut App) -> bool>;

/// Связывает общую рамку с текущим шагом экрана разговора.
struct ConversationChrome {
    title: ConversationTitle, // Заголовок со сведениями или выбором участников.
    back: ConversationBack,   // Возвращает true, если обработан внутренний шаг.
}

/// Хранит один экран, пока он открыт или находится под другим экраном.
struct Entry {
    key: PanelKey,       // Ключ для повторного открытия и проверки владельца.
    title: SharedString, // Заголовок общей рамки.
    conversation_chrome: Option<ConversationChrome>, // Внутренний шаг сведений разговора.
    body: AnyView,       // Живой GPUI-вид со своим состоянием.
    opener: FocusHandle, // Элемент вне стека, к которому можно вернуться при закрытии.
    region: FocusHandle, // Область фокуса этого экрана.
    close: FocusHandle,  // Кнопка закрытия экрана и его начальный фокус.
    clear: FocusHandle,  // Кнопка очистки стека, когда открыто несколько экранов.
}

/// Единый стек окна. Содержимое нижних экранов живёт в памяти до закрытия.
pub(super) struct RightPanel {
    entries: Vec<Entry>,                         // Видна только последняя запись.
    account: Option<AccountSession>,             // Текущий вход, включая его поколение.
    conversation: Option<ConversationAddress>,   // Текущий адрес рабочего разговора.
    fallback: FocusHandle, // Корень окна для возврата фокуса после смены маршрута.
    restore: Option<(FocusHandle, FocusHandle)>, // Удалённая область и безопасный фокус.
    end: FocusHandle,      // Точка после кнопок панели для обратного обхода в узком окне.
}

impl RightPanel {
    /// Готовит пустой стек для одного рабочего окна.
    pub fn new(fallback: FocusHandle, cx: &mut Context<Self>) -> Self {
        Self {
            entries: Vec::new(),
            account: None,
            conversation: None,
            fallback,
            restore: None,
            end: cx.focus_handle(),
        }
    }

    /// Обновляет контекст окна и удаляет экраны ушедшего разговора или входа.
    pub fn set_context(
        &mut self,
        account: Option<&AccountSession>,
        conversation: Option<&ConversationAddress>,
        cx: &mut Context<Self>,
    ) {
        if self.account.as_ref() == account && self.conversation.as_ref() == conversation {
            return;
        }
        self.account = account.cloned();
        self.conversation = conversation.cloned();
        self.prune(self.fallback.clone(), cx, |key| {
            key.matches(account, conversation)
        });
    }

    /// Удаляет экраны сброшенного разговора и в том числе отменяет ожидание их ответов.
    pub fn invalidate_conversation(
        &mut self,
        address: &ConversationAddress,
        fallback: FocusHandle,
        cx: &mut Context<Self>,
    ) {
        self.prune(
            fallback,
            cx,
            |key| !matches!(&key.scope, Scope::Conversation(current) if current == address),
        );
    }

    /// Фильтрует стек и восстанавливает фокус, если его элемент исчез.
    fn prune(
        &mut self,
        fallback: FocusHandle,
        cx: &mut Context<Self>,
        keep: impl Fn(&PanelKey) -> bool,
    ) {
        let previous = self
            .entries
            .last()
            .map(|entry| (entry.region.clone(), entry.clear.clone()));
        let before = self.entries.len();
        self.entries.retain(|entry| keep(&entry.key));
        if before == self.entries.len() {
            return;
        }

        if let Some((region, clear)) = previous {
            let lost = match self.entries.last() {
                Some(entry) if entry.region == region => (self.entries.len() == 1).then_some(clear),
                _ => Some(region),
            };
            if let Some(lost) = lost {
                // Несколько событий до кадра могут убрать и экран, на который возвращали фокус.
                let restore = self.restore.get_or_insert_with(|| (lost, fallback.clone()));
                restore.1 = fallback;
            }
        }
        self.changed("invalidate", cx);
    }

    /// Проверяет, показан ли экран, например для подписи кнопки открытия.
    pub fn is_active(&self, key: &PanelKey) -> bool {
        self.entries.last().is_some_and(|entry| &entry.key == key)
    }

    /// Проверяет, что экран есть в стеке, даже если его прикрыла другая панель.
    pub fn contains_view(&self, id: EntityId) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.body.entity_id() == id)
    }

    /// Показывает экран. Существующий ключ возвращает к живому экрану и убирает записи над ним.
    pub fn open<V: Render>(
        &mut self,
        key: PanelKey,
        title: impl Into<SharedString>,
        opener: FocusHandle,
        build: impl FnOnce(&mut Context<Self>) -> Entity<V>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !key.matches(self.account.as_ref(), self.conversation.as_ref()) || self.is_active(&key) {
            return;
        }

        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            self.entries.truncate(index + 1);
        } else {
            let body = build(cx).into();
            self.entries.push(Entry {
                key,
                title: title.into(),
                conversation_chrome: None,
                body,
                opener,
                region: cx.focus_handle(),
                close: cx.focus_handle(),
                clear: cx.focus_handle(),
            });
        }
        self.restore = None;
        window.focus(&self.entries.last().expect("экран открыт").close);
        self.changed("open", cx);
    }

    /// Подключает заголовок разговора и возврат из его вложенной формы.
    pub fn configure_conversation(
        &mut self,
        key: &PanelKey,
        title: impl Fn(&App) -> &'static str + 'static,
        back: impl Fn(&mut App) -> bool + 'static,
    ) {
        if let Some(entry) = self.entries.last_mut().filter(|entry| &entry.key == key) {
            entry.conversation_chrome = Some(ConversationChrome {
                title: Rc::new(title),
                back: Rc::new(back),
            });
        }
    }

    /// Ставит фокус на закрытие только если этот экран сейчас сверху.
    pub fn focus_close(&self, key: &PanelKey, window: &mut Window) {
        if let Some(entry) = self.entries.last().filter(|entry| &entry.key == key) {
            window.focus(&entry.close);
        }
    }

    /// Возвращает из внутренней формы или снимает сам экран.
    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.last()
            && let Some(chrome) = &entry.conversation_chrome
            && (chrome.back)(cx)
        {
            window.focus(&entry.close);
            self.changed("back", cx);
            return;
        }
        self.pop(window, cx);
    }

    /// Снимает верхний экран по прямому действию его владельца.
    pub fn pop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.pop() {
            let focus = self
                .entries
                .last()
                .map_or_else(|| self.return_target(&entry, window), |entry| &entry.close);
            window.focus(focus);
            self.restore = None;
            self.changed("back", cx);
        }
    }

    /// Закрывает всю правую область и возвращает фокус к первому открытию.
    pub fn close_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(first) = self.entries.first() {
            window.focus(self.return_target(first, window));
            self.entries.clear();
            self.restore = None;
            self.changed("close_all", cx);
        }
    }

    /// Выбирает кнопку открытия, если она всё ещё присутствует в окне.
    fn return_target<'a>(&'a self, entry: &'a Entry, window: &Window) -> &'a FocusHandle {
        if self.fallback.contains(&entry.opener, window) {
            &entry.opener
        } else {
            &self.fallback
        }
    }

    /// Обновляет разметку окна и кнопки, которые показывают состояние стека.
    fn changed(&self, event: &str, cx: &mut Context<Self>) {
        log::debug!(
            "gpui_right_panel event={event} depth={}",
            self.entries.len()
        );
        cx.notify();
    }

    /// Регистрирует Escape. Команды Back и CloseAll доступны для будущих привязок.
    pub fn bind_keys(app: &mut App) {
        app.bind_keys([KeyBinding::new("escape", Back, Some(KEY_CONTEXT))]);
    }

    /// Восстанавливает фокус только если он остался в снятом экране.
    pub fn restore_focus(host: &Entity<Self>, window: &mut Window, app: &mut App) {
        if host.read(app).restore.is_some() {
            host.update(app, |host, cx| {
                if let Some((region, fallback)) = host.restore.take()
                    && region.contains_focused(window, cx)
                {
                    window.focus(host.entries.last().map_or(&fallback, |entry| &entry.close));
                }
            });
        }
    }

    /// Оборачивает верхний экран общей рамкой. В узком окне рамка лежит поверх разговора.
    pub fn frame(
        host: &Entity<Self>,
        palette: Palette,
        window: &Window,
        app: &App,
    ) -> Option<Stateful<Div>> {
        let stack = host.read(app);
        let entry = stack.entries.last()?;
        let overlay = window.viewport_size().width < px(1050.);
        let account_page = matches!(entry.key.scope, Scope::Account(_));
        let account_back = account_page
            && stack
                .entries
                .iter()
                .rev()
                .nth(1)
                .is_some_and(|previous| entry.key.scope == previous.key.scope);
        let (close_id, close_label) = if account_back {
            ("right-panel-back", "Назад")
        } else if account_page {
            ("right-panel-close", "Закрыть")
        } else {
            ("conversation-details-close", "Закрыть")
        };
        let mut close = Button::new(close_id, if account_page { close_label } else { "" });
        close.icon = (!account_back).then_some(Icon::Close);
        close.appearance = ButtonAppearance::Ghost;
        let title = entry.conversation_chrome.as_ref().map_or_else(
            || entry.title.clone(),
            |chrome| SharedString::from((chrome.title)(app)),
        );
        let text_role = if account_page {
            TextRole::Label
        } else {
            TextRole::Heading
        };
        let clear = Button::new("right-panel-close-all", "Закрыть все");
        let clear_focus = (stack.entries.len() > 1).then_some(&entry.clear);

        Some(
            div()
                .id("conversation-details-panel")
                .debug_selector(|| "conversation-details-panel".into())
                .track_focus(&entry.region)
                .key_context(KEY_CONTEXT)
                .flex()
                .flex_col()
                .w(px(315.))
                .min_h_0()
                .flex_none()
                .bg(palette.surface)
                .when(account_page, |panel| {
                    panel.rounded_md().border_1().border_color(palette.border)
                })
                .when(!account_page, |panel| panel.rounded(px(12.)).pt_2().pb_5())
                .when(overlay, |panel| {
                    panel
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_0()
                        .max_w_full()
                        .shadow_md()
                        .occlude()
                        .on_action(listener(host, |host, _: &NextControl, window, cx| {
                            host.cycle_focus(false, window, cx)
                        }))
                        .on_action(listener(host, |host, _: &PreviousControl, window, cx| {
                            host.cycle_focus(true, window, cx)
                        }))
                })
                .when(!overlay, |panel| panel.h_full().ml(px(4.)))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .justify_between()
                        .items_center()
                        .gap_3()
                        .when(account_page, |header| header.p_3())
                        .when(!account_page, |header| header.h(px(40.)).mx_2().px_2())
                        .child(label(palette, text_role, title).truncate())
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .children(clear_focus.map(|focus| {
                                    clear
                                        .render(
                                            palette,
                                            Some(focus),
                                            click(host, RightPanel::close_all),
                                        )
                                        .debug_selector(|| "right-panel-close-all".into())
                                }))
                                .child(
                                    close
                                        .render(
                                            palette,
                                            Some(&entry.close),
                                            click(host, RightPanel::back),
                                        )
                                        .debug_selector(move || close_id.into())
                                        .when(!account_page, |button| {
                                            button.tooltip(move |_, app| {
                                                tooltip(palette, close_label.into(), app)
                                            })
                                        }),
                                ),
                        ),
                )
                .child(entry.body.clone())
                .child(div().id("right-panel-end").track_focus(&stack.end)),
        )
    }

    /// Оставляет клавиатурный обход внутри перекрывающей панели.
    fn cycle_focus(&self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.last() else {
            return;
        };
        cx.stop_propagation();
        if reverse {
            window.focus_prev();
            if !entry.region.contains_focused(window, cx) {
                window.focus(&self.end);
                window.focus_prev();
            }
        } else {
            window.focus_next();
            if !entry.region.contains_focused(window, cx) {
                let first = if self.entries.len() > 1 {
                    &entry.clear
                } else {
                    &entry.close
                };
                window.focus(first);
            }
        }
    }

    /// Проверяет, что после очистки в стеке не осталось экранов.
    #[cfg(all(test, feature = "gpui-test"))]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Возвращает живой фокус кнопки закрытия для проверок окна.
    #[cfg(all(test, feature = "gpui-test"))]
    pub fn close_focus(&self, key: &PanelKey, window: &Window) -> bool {
        self.entries
            .last()
            .is_some_and(|entry| &entry.key == key && entry.close.is_focused(window))
    }
}

/// Доставляет действие рамки только живому стеку окна.
fn listener<A: gpui::Action>(
    host: &Entity<RightPanel>,
    handle: impl Fn(&mut RightPanel, &A, &mut Window, &mut Context<RightPanel>) + 'static,
) -> impl Fn(&A, &mut Window, &mut App) + 'static {
    let target = host.downgrade();
    move |action, window, app| {
        let _ = target.update(app, |host, cx| handle(host, action, window, cx));
    }
}

/// Направляет нажатие кнопки рамки в тот же обработчик, что и действие GPUI.
fn click(
    host: &Entity<RightPanel>,
    handle: fn(&mut RightPanel, &mut Window, &mut Context<RightPanel>),
) -> impl Fn(&mut Window, &mut App) + 'static {
    let target = host.downgrade();
    move |window, app| {
        let _ = target.update(app, |host, cx| handle(host, window, cx));
    }
}
