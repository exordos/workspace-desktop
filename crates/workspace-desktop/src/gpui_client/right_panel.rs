//! Стек правых панелей окна. Хранит открытые экраны, проверяет их контекст и ведёт фокус.

use std::rc::Rc;

use gpui::{
    AnyView, App, Context, Div, ElementId, Entity, EntityId, FocusHandle, KeyBinding, SharedString,
    Stateful, Window, actions, anchored, deferred, div, point, prelude::*, px, rgba,
};

use super::{
    NextControl, PreviousControl,
    contracts::{AccountSession, ConversationAddress},
    preferences::{Language, Preferences},
    ui::{Button, ButtonAppearance, Icon, PANEL_RADIUS, Palette, TextRole, label, popup::tooltip},
};

const KEY_CONTEXT: &str = "WorkspaceRightPanel";
const MODAL_CONTEXT: &str = "WorkspaceSettingsModal";

actions!(workspace_right_panel, [Back, CloseAll, DismissModal]);

/// Ограничивает время жизни панели контекстом, из которого её открыли.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Scope {
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
type ConversationNested = Rc<dyn Fn(&App) -> bool>;
type ConversationBack = Rc<dyn Fn(&mut App) -> bool>;
type ModalDismiss = Rc<dyn Fn(&App) -> bool>;

/// Связывает общую рамку с текущим шагом экрана разговора.
struct ConversationChrome {
    title: ConversationTitle, // Заголовок со сведениями или выбором участников.
    nested: ConversationNested, // Отдельная кнопка возврата видна на вложенном экране.
    back: ConversationBack,   // Возвращает true, если обработан внутренний шаг.
}

/// Хранит один экран, пока он открыт или находится под другим экраном.
struct Entry {
    key: PanelKey,       // Ключ для повторного открытия и проверки владельца.
    title: SharedString, // Заголовок общей рамки.
    english_title: Option<&'static str>, // Перевод заголовка страницы аккаунта.
    conversation_chrome: Option<ConversationChrome>, // Внутренний шаг сведений разговора.
    body: AnyView,       // Живой GPUI-вид со своим состоянием.
    opener: FocusHandle, // Элемент вне стека, к которому можно вернуться при закрытии.
    parent_opener: Option<FocusHandle>, // Вложенный пункт, к которому возвращается кнопка «Назад».
    region: FocusHandle, // Область фокуса этого экрана.
    close: FocusHandle,  // Кнопка закрытия экрана и его начальный фокус.
    back: FocusHandle,   // Кнопка перехода к предыдущему экрану аккаунта.
    clear: FocusHandle,  // Кнопка очистки стека, когда открыто несколько экранов.
}

/// Один диалог поверх окна. Закрывается вместе со страницей, которая его открыла.
struct Modal {
    body: AnyView,                 // Диалог с собственной формой и состоянием.
    session: AccountSession,       // Поколение, с которым диалог нужно закрыть.
    region: FocusHandle,           // Родительская область клавиатурного фокуса.
    opener: FocusHandle,           // Кнопка, к которой вернётся фокус.
    end: FocusHandle,              // Точка для обратного обхода после последней кнопки.
    dismiss: Option<ModalDismiss>, // Разрешает закрытие во время работы формы.
}

/// Единый стек окна. Содержимое нижних экранов живёт в памяти до закрытия.
pub(super) struct RightPanel {
    entries: Vec<Entry>,                         // Видна только последняя запись.
    hidden_account: Vec<Entry>, // Вложенные экраны аккаунта после повторного нажатия его кнопки.
    modal: Option<Modal>,       // Модальное окно текущей страницы.
    account: Option<AccountSession>, // Текущий вход, включая его поколение.
    conversation: Option<ConversationAddress>, // Текущий адрес рабочего разговора.
    fallback: FocusHandle,      // Корень окна для возврата фокуса после смены маршрута.
    restore: Option<(FocusHandle, FocusHandle)>, // Удалённая область и безопасный фокус.
    end: FocusHandle,           // Точка после кнопок панели для обратного обхода в узком окне.
}

impl RightPanel {
    /// Готовит пустой стек для одного рабочего окна.
    pub fn new(fallback: FocusHandle, cx: &mut Context<Self>) -> Self {
        Self {
            entries: Vec::new(),
            hidden_account: Vec::new(),
            modal: None,
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
        self.hidden_account
            .retain(|entry| entry.key.matches(account, conversation));
        if self
            .modal
            .as_ref()
            .is_some_and(|modal| Some(&modal.session) != account)
            && let Some(modal) = self.modal.take()
        {
            self.restore = Some((modal.region, self.fallback.clone()));
            self.changed("modal_invalidate", cx);
        }
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

        self.close_modal(window, cx);
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            self.entries.truncate(index + 1);
        } else {
            let body = build(cx).into();
            self.entries.push(Entry {
                key,
                title: title.into(),
                english_title: None,
                conversation_chrome: None,
                body,
                opener,
                parent_opener: None,
                region: cx.focus_handle(),
                close: cx.focus_handle(),
                back: cx.focus_handle(),
                clear: cx.focus_handle(),
            });
        }
        self.restore = None;
        window.focus(&self.entries.last().expect("экран открыт").close);
        self.visible_change("open", window, cx);
    }

    /// Запоминает пункт родительской страницы для возврата после вложенного экрана.
    pub fn set_parent_opener(&mut self, opener: FocusHandle) {
        if let Some(entry) = self.entries.last_mut() {
            entry.parent_opener = Some(opener);
        }
    }

    /// Запоминает английскую подпись для смены языка на уже открытом экране.
    pub fn set_account_translation(&mut self, title: &'static str) {
        if let Some(entry) = self.entries.last_mut() {
            entry.english_title = Some(title);
        }
    }

    /// Скрывает весь текущий слой аккаунта или возвращает его на том же экране.
    pub fn toggle_account(
        &mut self,
        session: &AccountSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let account_scope = Scope::Account(session.clone());
        if self
            .entries
            .last()
            .is_some_and(|entry| entry.key.scope == account_scope)
        {
            self.close_modal(window, cx);
            let first = self
                .entries
                .iter()
                .rposition(|entry| entry.key.scope != account_scope)
                .map_or(0, |previous| previous + 1);
            self.hidden_account = self.entries.split_off(first);
            if let Some(hidden) = self.hidden_account.first() {
                let focus = self
                    .entries
                    .last()
                    .map_or_else(|| self.return_target(hidden, window), |entry| &entry.close);
                window.focus(focus);
            }
            self.visible_change("account_hide", window, cx);
            return true;
        }
        if self
            .hidden_account
            .first()
            .is_some_and(|entry| entry.key.scope == account_scope)
        {
            self.close_modal(window, cx);
            self.entries.append(&mut self.hidden_account);
            window.focus(&self.entries.last().expect("слой аккаунта открыт").close);
            self.visible_change("account_show", window, cx);
            return true;
        }
        false
    }

    /// Возвращает текущий разговор, даже когда его закрывает панель аккаунта.
    pub fn current_conversation(&self) -> Option<&ConversationAddress> {
        self.conversation.as_ref()
    }

    /// Показывает диалог поверх окна, если экран всё ещё относится к активному входу.
    pub fn open_modal<V: Render>(
        &mut self,
        session: &AccountSession,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(FocusHandle, &mut Context<Self>) -> Entity<V>,
    ) {
        if self.account.as_ref() != Some(session) || self.modal.is_some() {
            return;
        }
        let region = cx.focus_handle();
        let body = build(region.clone(), cx).into();
        window.focus(&region);
        self.modal = Some(Modal {
            body,
            session: session.clone(),
            region,
            opener,
            end: cx.focus_handle(),
            dismiss: None,
        });
        self.visible_change("modal_open", window, cx);
    }

    /// Блокирует команды под открытым модальным диалогом.
    pub fn has_modal(&self) -> bool {
        self.modal.is_some()
    }

    /// Отличает открытый диалог заданного вида от остальных модальных окон.
    pub(super) fn modal_is<V: Render>(&self) -> bool {
        self.modal
            .as_ref()
            .is_some_and(|modal| modal.body.entity_type() == std::any::TypeId::of::<V>())
    }

    /// Возвращает регион и начальную кнопку панели для клавиши F6.
    pub(super) fn keyboard_area(&self) -> Option<(FocusHandle, FocusHandle)> {
        self.entries
            .last()
            .map(|entry| (entry.region.clone(), entry.close.clone()))
    }

    /// Учитывает состояние формы при закрытии по Escape и клику по фону.
    pub fn set_modal_dismiss(&mut self, allowed: impl Fn(&App) -> bool + 'static) {
        if let Some(modal) = &mut self.modal {
            modal.dismiss = Some(Rc::new(allowed));
        }
    }

    /// Закрывает диалог по действию формы и возвращает фокус на её кнопку.
    pub fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(modal) = self.modal.take() {
            window.focus(&modal.opener);
            self.visible_change("modal_close", window, cx);
        }
    }

    /// Обрабатывает закрытие общего оверлея через Escape или клик вне формы.
    fn dismiss_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if self
            .modal
            .as_ref()
            .and_then(|modal| modal.dismiss.as_ref())
            .is_none_or(|allowed| allowed(cx))
        {
            self.close_modal(window, cx);
        }
    }

    /// Рисует затемнение и удерживает клавиатурный обход внутри диалога.
    pub fn modal_frame(
        host: &Entity<Self>,
        window: &Window,
        app: &App,
    ) -> Option<impl IntoElement + use<>> {
        let modal = host.read(app).modal.as_ref()?;
        let size = window.viewport_size();
        let layer = div()
            .relative()
            .w(size.width)
            .h(size.height)
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("profile-modal-backdrop")
                    .debug_selector(|| "profile-modal-backdrop".into())
                    .absolute()
                    .inset_0()
                    .bg(rgba(0x00000080))
                    .occlude()
                    .on_click(click_event(host, Self::dismiss_modal)),
            )
            .child(
                div()
                    .id("profile-modal-region")
                    .track_focus(&modal.region)
                    .key_context(MODAL_CONTEXT)
                    .occlude()
                    .on_action(listener(host, |host, _: &DismissModal, window, cx| {
                        host.dismiss_modal(window, cx)
                    }))
                    .on_action(listener(host, |host, _: &NextControl, window, cx| {
                        host.cycle_modal_focus(false, window, cx)
                    }))
                    .on_action(listener(host, |host, _: &PreviousControl, window, cx| {
                        host.cycle_modal_focus(true, window, cx)
                    }))
                    .child(modal.body.clone())
                    .child(div().id("profile-modal-end").track_focus(&modal.end)),
            );
        Some(div().absolute().size(px(0.)).child(
            deferred(anchored().position(point(px(0.), px(0.))).child(layer)).with_priority(5),
        ))
    }

    /// Переносит обход с края диалога к противоположному краю.
    fn cycle_modal_focus(&self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(modal) = self.modal.as_ref() else {
            return;
        };
        cx.stop_propagation();
        if reverse {
            window.focus_prev();
            if !modal.region.contains_focused(window, cx) {
                window.focus(&modal.end);
                window.focus_prev();
            }
        } else {
            window.focus_next();
            if !modal.region.contains_focused(window, cx) {
                window.focus(&modal.region);
                window.focus_next();
            }
        }
        if !modal.region.contains_focused(window, cx) {
            window.focus(&modal.region);
        }
    }

    /// Подключает заголовок разговора и возврат из его вложенной формы.
    pub fn configure_conversation(
        &mut self,
        key: &PanelKey,
        title: impl Fn(&App) -> &'static str + 'static,
        nested: impl Fn(&App) -> bool + 'static,
        back: impl Fn(&mut App) -> bool + 'static,
    ) {
        if let Some(entry) = self.entries.last_mut().filter(|entry| &entry.key == key) {
            entry.conversation_chrome = Some(ConversationChrome {
                title: Rc::new(title),
                nested: Rc::new(nested),
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
        if self.modal.is_some() {
            self.dismiss_modal(window, cx);
            return;
        }
        if let Some(entry) = self.entries.last()
            && let Some(chrome) = &entry.conversation_chrome
            && (chrome.back)(cx)
        {
            window.focus(&entry.close);
            self.visible_change("back", window, cx);
            return;
        }
        self.pop(window, cx);
    }

    /// Снимает верхний экран по прямому действию его владельца.
    pub fn pop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.pop() {
            let focus = self.entries.last().map_or_else(
                || self.return_target(&entry, window),
                |parent| entry.parent_opener.as_ref().unwrap_or(&parent.close),
            );
            window.focus(focus);
            self.restore = None;
            self.visible_change("back", window, cx);
        }
    }

    /// Закрывает весь слой аккаунта и возвращает предыдущую панель разговора.
    fn close_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while self
            .entries
            .last()
            .is_some_and(|entry| entry.parent_opener.is_some())
        {
            self.entries.pop();
        }
        self.pop(window, cx);
    }

    /// Закрывает всю правую область и возвращает фокус к первому открытию.
    pub fn close_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        self.hidden_account.clear();
        if let Some(first) = self.entries.first() {
            window.focus(self.return_target(first, window));
            self.entries.clear();
            self.restore = None;
            self.visible_change("close_all", window, cx);
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

    /// Перерисовывает окно сразу после дискретного перехода, даже если его фокус уже установлен.
    fn visible_change(&self, event: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.changed(event, cx);
        window.refresh();
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
        app.bind_keys([
            KeyBinding::new("escape", Back, Some(KEY_CONTEXT)),
            KeyBinding::new("escape", DismissModal, Some(MODAL_CONTEXT)),
        ]);
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
        let language = Preferences::read(app).language;
        let close_label = language.text("Закрыть", "Close");
        let back_label = language.text("Назад", "Back");
        let close_id = if account_page {
            "profile-close"
        } else {
            "conversation-details-close"
        };
        let mut close = Button::new(close_id, "");
        close.icon = Some(Icon::Close);
        close.appearance = ButtonAppearance::Ghost;
        let conversation_back = entry
            .conversation_chrome
            .as_ref()
            .is_some_and(|chrome| (chrome.nested)(app));
        let back_id = if conversation_back {
            "conversation-gallery-back"
        } else {
            "profile-back"
        };
        let mut back = Button::new(back_id, "");
        back.icon = Some(Icon::ChevronLeft);
        back.appearance = ButtonAppearance::Ghost;
        let title = entry.conversation_chrome.as_ref().map_or_else(
            || {
                if language == Language::En
                    && let Some(title) = entry.english_title
                {
                    title.into()
                } else {
                    entry.title.clone()
                }
            },
            |chrome| SharedString::from((chrome.title)(app)),
        );
        let clear = Button::new(
            "right-panel-close-all",
            language.text("Закрыть все", "Close all"),
        );
        let clear_focus = (!account_page && !conversation_back && stack.entries.len() > 1)
            .then_some(&entry.clear);

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
                .rounded(PANEL_RADIUS)
                .bg(palette.surface)
                .pt_2()
                .pb_5()
                .when(account_page, |panel| panel.overflow_hidden())
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
                        .gap_2()
                        .when(conversation_back, |header| header.gap_1())
                        .h(px(40.))
                        .mx_2()
                        .px_2()
                        .children((account_back || conversation_back).then(|| {
                            back.render(palette, Some(&entry.back), click(host, Self::back))
                                .when(conversation_back, |back| {
                                    back.px_0().w(px(16.)).min_w(px(16.))
                                })
                                .debug_selector(move || back_id.into())
                                .tooltip(move |_, app| tooltip(palette, back_label.into(), app))
                        }))
                        .child(
                            label(palette, TextRole::Heading, title)
                                .flex_1()
                                .min_w_0()
                                .truncate(),
                        )
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
                                            click(
                                                host,
                                                if account_page {
                                                    Self::close_account
                                                } else if conversation_back {
                                                    Self::pop
                                                } else {
                                                    Self::back
                                                },
                                            ),
                                        )
                                        .debug_selector(move || close_id.into())
                                        .tooltip(move |_, app| {
                                            tooltip(palette, close_label.into(), app)
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
                let conversation_back = entry
                    .conversation_chrome
                    .as_ref()
                    .is_some_and(|chrome| (chrome.nested)(cx));
                let first = if entry.parent_opener.is_some() || conversation_back {
                    &entry.back
                } else if self.entries.len() > 1
                    && matches!(entry.key.scope, Scope::Conversation(_))
                {
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

/// Направляет клик по фону оверлея в обработчик закрытия диалога.
fn click_event(
    host: &Entity<RightPanel>,
    handle: fn(&mut RightPanel, &mut Window, &mut Context<RightPanel>),
) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
    let activate = click(host, handle);
    move |_, window, app| activate(window, app)
}
