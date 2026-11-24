//! Открывает настройки выжимки точной темы. Диалог держит черновик,
//! а Core читает и сохраняет подтверждённые сервером параметры.

use gpui::{
    App, Context, Div, Entity, FocusHandle, Focusable, IntoElement, MouseButton, Render,
    SharedString, Subscription, Task, WeakEntity, Window, div, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::messenger::{
    details::{ChannelSidebar, DetailsError, SidebarDetails, SidebarTopic},
    summary_settings::{
        MAX_PROMPT_CHARS, ReasoningEffort, TopicSummaryConfiguration, TopicSummaryEdit,
    },
};

use super::Conversation;
use crate::gpui_client::{
    composer::Editor,
    contracts::ConversationAddress,
    preferences::{Language, Preferences},
    right_panel::RightPanel,
    runtime::RuntimeClient,
    ui::{
        Button, ButtonAppearance, Icon, Palette, SURFACE_RADIUS, TextRole, activate, label,
        popup::{MenuInput, menu_navigation, tooltip},
    },
};

const REASONING_OPTIONS: [Option<ReasoningEffort>; 6] = [
    None,
    Some(ReasoningEffort::Off),
    Some(ReasoningEffort::Minimal),
    Some(ReasoningEffort::Low),
    Some(ReasoningEffort::Medium),
    Some(ReasoningEffort::High),
];

impl Conversation {
    /// Предоставляет точную тему, пока видна панель и подтверждена роль участника.
    fn summary_topic(&self, app: &App) -> Option<(&ChannelSidebar, &SidebarTopic)> {
        let panel = self.details.as_ref()?;
        if !self.has_access() || !panel.is_present(app) {
            return None;
        }
        let SidebarDetails::Channel(channel) = panel.snapshot.as_deref()? else {
            return None;
        };
        let id = self.address.route.scope.topic_id?;
        if !channel.can_edit_summary(id) {
            return None;
        }
        Some((channel, channel.topics.iter().find(|topic| topic.id == id)?))
    }

    /// Открывает модальное окно в области текущего разговора и сохраняет фокус кнопки.
    pub(super) fn open_summary_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((channel, topic)) = self.summary_topic(cx) else {
            return;
        };
        let subtitle: SharedString = format!("# {} · {}", topic.name, channel.name).into();
        let panel = self
            .details
            .as_ref()
            .expect("Настройки доступны только из панели");
        let opener = panel.summary_settings.clone();
        let host = panel.host.downgrade();
        let runtime = panel.runtime.clone();
        let address = self.address.clone();
        let conversation = cx.entity();
        panel.host.update(cx, |panel, cx| {
            panel.open_conversation_modal(&address, opener, window, cx, |region, cx| {
                let address = address.clone();
                cx.new(|cx| {
                    SummaryDialog::new(host, conversation, runtime, address, region, subtitle, cx)
                })
            });
        });
    }
}

/// Результат последней операции, который виден рядом с формой.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Feedback {
    Saved,  // Сервер подтвердил обновление.
    Failed, // Временный сбой чтения или записи.
    Denied, // Участник больше не может управлять параметрами.
}

/// Диалог одной темы. Данные и задачи принадлежат именно его открытию.
struct SummaryDialog {
    host: WeakEntity<RightPanel>, // Закрывает диалог и возвращает фокус.
    conversation: WeakEntity<Conversation>, // Актуальный экран, который отправил запрос.
    _changes: Subscription,       // Следит за отзывом доступа к разговору.
    runtime: RuntimeClient,       // Вызывает Core для текущего адреса.
    address: ConversationAddress, // Владелец, поколение, канал и тема.
    region: FocusHandle,          // Фокус до появления формы и во время записи.
    subtitle: SharedString,       // Названия темы и канала из проверенной карточки.
    base: Option<TopicSummaryConfiguration>, // Данные последнего успешного ответа.
    enabled: bool,                // Черновик переключателя автообновления.
    use_default_prompt: bool,     // При true сервер выбирает обычную инструкцию.
    prompt: Entity<Editor>,       // Многострочная инструкция из черновика.
    _prompt_changes: Subscription, // Пересчитывает отличия и доступность сохранения.
    reasoning: Option<ReasoningEffort>, // Черновик уровня рассуждения.
    reasoning_opener: FocusHandle, // Возврат фокуса после выбора уровня.
    reasoning_focus: FocusHandle, // Фокус открытого списка.
    reasoning_selected: Option<usize>, // Выделение списка, None пока список закрыт.
    request: Option<Task<()>>,    // Не допускает одновременного чтения и записи.
    feedback: Option<Feedback>,   // Сообщение последнего завершённого запроса.
}

impl SummaryDialog {
    /// Создаёт форму и загружает параметры конкретной темы через штатную сессию.
    fn new(
        host: WeakEntity<RightPanel>,
        conversation: Entity<Conversation>,
        runtime: RuntimeClient,
        address: ConversationAddress,
        region: FocusHandle,
        subtitle: SharedString,
        cx: &mut Context<Self>,
    ) -> Self {
        let prompt = cx.new(Editor::for_conversation);
        let prompt_changes = cx.observe(&prompt, |dialog, editor, cx| {
            if !editor.read(cx).text().is_empty() {
                dialog.use_default_prompt = false;
                if dialog.feedback == Some(Feedback::Saved)
                    && dialog
                        .base
                        .as_ref()
                        .is_some_and(|base| *base != dialog.draft(cx))
                {
                    dialog.feedback = None;
                }
            }
            cx.notify();
        });
        let changes = cx.observe(&conversation, |_, _, cx| cx.notify());
        let mut dialog = Self {
            host,
            conversation: conversation.downgrade(),
            _changes: changes,
            runtime,
            address,
            region,
            subtitle,
            base: None,
            enabled: true,
            use_default_prompt: true,
            prompt,
            _prompt_changes: prompt_changes,
            reasoning: None,
            reasoning_opener: cx.focus_handle(),
            reasoning_focus: cx.focus_handle(),
            reasoning_selected: None,
            request: None,
            feedback: None,
        };
        dialog.request(None, cx);
        dialog
    }

    /// Оставляет на экране только форму для той же темы с текущим правом управления.
    fn is_current(&self, app: &App) -> bool {
        self.conversation.upgrade().is_some_and(|conversation| {
            let conversation = conversation.read(app);
            conversation.address == self.address && conversation.summary_topic(app).is_some()
        })
    }

    /// Читает несохранённый выбор пользователя без обработки текста как Markdown.
    fn draft(&self, app: &App) -> TopicSummaryConfiguration {
        TopicSummaryConfiguration {
            enabled: self.enabled,
            prompt: (!self.use_default_prompt).then(|| self.prompt.read(app).text().to_owned()),
            reasoning: self.reasoning,
        }
    }

    /// Показывает серверный снимок или возвращает к нему после отмены правок.
    fn set_draft(&mut self, configuration: &TopicSummaryConfiguration, cx: &mut Context<Self>) {
        self.enabled = configuration.enabled;
        self.reasoning = configuration.reasoning;
        self.use_default_prompt = configuration.prompt.is_none();
        self.prompt.update(cx, |editor, cx| {
            editor.show_reply(configuration.prompt.as_deref().unwrap_or_default(), cx);
        });
    }

    /// Выполняет чтение или запись и принимает ответ только для открытой формы.
    fn request(&mut self, edit: Option<TopicSummaryEdit>, cx: &mut Context<Self>) {
        if self.request.is_some() {
            return;
        }
        self.feedback = None;
        let saving = edit.is_some();
        let op_id = Uuid::new_v4();
        let runtime = self.runtime.clone();
        let address = self.address.clone();
        log::debug!("gpui_summary_settings op_id={op_id} event=request_started saving={saving}");
        self.request = Some(cx.spawn(async move |entity, cx| {
            let reply = runtime
                .conversation(address, |services, address| async move {
                    let owner = &address.route.owner;
                    let scope = address.route.scope;
                    if let Some(edit) = edit {
                        services
                            .messenger
                            .save_topic_summary_configuration(
                                &services.sync,
                                owner,
                                address.generation,
                                scope,
                                edit,
                            )
                            .await
                    } else {
                        services
                            .messenger
                            .topic_summary_configuration(
                                &services.sync,
                                owner,
                                address.generation,
                                scope,
                            )
                            .await
                    }
                })
                .await;
            let _ = entity.update(cx, |dialog, cx| {
                dialog.request.take();
                if !dialog.is_current(cx) {
                    log::debug!("gpui_summary_settings op_id={op_id} event=stale_dialog");
                    return;
                }
                let result = reply.for_current(Some(&dialog.address));
                log::debug!(
                    "gpui_summary_settings op_id={op_id} event=request_completed success={}",
                    matches!(&result, Some(Ok(Ok(_))))
                );
                match result {
                    Some(Ok(Ok(configuration))) => {
                        dialog.base = Some(configuration.clone());
                        dialog.set_draft(&configuration, cx);
                        dialog.feedback = saving.then_some(Feedback::Saved);
                    }
                    Some(Ok(Err(DetailsError::ActionDenied | DetailsError::AccessLost))) => {
                        dialog.feedback = Some(Feedback::Denied);
                    }
                    _ => dialog.feedback = Some(Feedback::Failed),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Отправляет только валидное изменение. Пока ответ в пути, форма только для чтения.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(base) = self.base.clone() else {
            return;
        };
        let draft = self.draft(cx).normalized();
        if !self.is_current(cx)
            || self.feedback == Some(Feedback::Denied)
            || self.request.is_some()
            || draft == base
            || !draft.valid_prompt()
        {
            return;
        }
        window.focus(&self.region);
        self.request(Some(TopicSummaryEdit { base, draft }), cx);
    }

    /// Закрывает только этот диалог и возвращает фокус кнопке настроек.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.host.update(cx, |host, cx| {
            if host.modal_is::<Self>() {
                host.close_modal(window, cx);
            }
        });
    }

    /// Обрабатывает перемещение, подтверждение и закрытие списка уровней.
    fn reasoning_input(&mut self, input: MenuInput, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.reasoning_selected else {
            return;
        };
        let last = REASONING_OPTIONS.len() - 1;
        self.reasoning_selected = match input {
            MenuInput::First => Some(0),
            MenuInput::Last => Some(last),
            MenuInput::Previous => Some(selected.saturating_sub(1)),
            MenuInput::Next => Some((selected + 1).min(last)),
            MenuInput::Select => {
                self.reasoning = REASONING_OPTIONS[selected];
                self.feedback = None;
                None
            }
            MenuInput::Close => None,
        };
        if self.reasoning_selected.is_none() {
            window.focus(&self.reasoning_opener);
        }
        cx.notify();
    }

    /// Рисует выбор уровня и передаёт перемещение общим клавишам меню GPUI.
    fn reasoning_control(
        &self,
        palette: Palette,
        language: Language,
        disabled: bool,
        cx: &Context<Self>,
    ) -> Div {
        let mut opener = Button::new("summary-reasoning", effort_name(self.reasoning, language));
        opener.icon = Some(Icon::ChevronDown);
        opener.disabled = disabled;
        let open = opener
            .render(
                palette,
                Some(&self.reasoning_opener),
                click(cx, |dialog, window, cx| {
                    dialog.reasoning_selected = Some(
                        REASONING_OPTIONS
                            .iter()
                            .position(|choice| *choice == dialog.reasoning)
                            .unwrap_or_default(),
                    );
                    window.focus(&dialog.reasoning_focus);
                    cx.notify();
                }),
            )
            .w_full()
            .justify_start()
            .debug_selector(|| "topic-summary-reasoning".into());
        div()
            .relative()
            .w(px(336.))
            .max_w_full()
            .child(open)
            .children(self.reasoning_selected.map(|selected| {
                let entries = REASONING_OPTIONS
                    .into_iter()
                    .enumerate()
                    .map(|(index, choice)| {
                        Button::new(
                            ("summary-reasoning-level", index),
                            effort_name(choice, language),
                        )
                        .render(
                            palette,
                            None,
                            click(cx, move |dialog, window, cx| {
                                dialog.reasoning_selected = Some(index);
                                dialog.reasoning_input(MenuInput::Select, window, cx);
                            }),
                        )
                        .justify_start()
                        .tab_stop(false)
                        .bg(if selected == index {
                            palette.card_selected
                        } else {
                            palette.surface
                        })
                        .debug_selector(move || format!("topic-summary-reasoning-{index}"))
                    });
                let menu = div()
                    .id("topic-summary-reasoning-menu")
                    .absolute()
                    .bottom(px(36.))
                    .left_0()
                    .right_0()
                    .p_1()
                    .flex()
                    .flex_col()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(palette.border)
                    .bg(palette.surface)
                    .shadow_md()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|dialog, _, window, cx| {
                        dialog.reasoning_input(MenuInput::Close, window, cx);
                    }))
                    .children(entries);
                menu_navigation(menu, &self.reasoning_focus, cx, Self::reasoning_input)
            }))
    }

    /// Рисует переключатель, редактор инструкции и выбор уровня рассуждения.
    fn form(&self, palette: Palette, language: Language, cx: &Context<Self>) -> Div {
        let text = move |key| language.tr(key);
        let draft = self.draft(cx);
        let disabled = self.request.is_some() || self.feedback == Some(Feedback::Denied);
        let switch = div()
            .id("topic-summary-enabled")
            .debug_selector(|| "topic-summary-enabled".into())
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .px_4()
            .py_3()
            .rounded(px(12.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.card)
            .child(
                div()
                    .min_w_0()
                    .child(label(
                        palette,
                        TextRole::Label,
                        text("topicSummarySettings.topic.enabled"),
                    ))
                    .child(label(
                        palette,
                        TextRole::Caption,
                        text("topicSummarySettings.topic.enabledDescription"),
                    )),
            )
            .child(
                div()
                    .w(px(36.))
                    .h(px(20.))
                    .p(px(2.))
                    .rounded_full()
                    .flex()
                    .flex_none()
                    .when(draft.enabled, |switch| switch.justify_end())
                    .bg(if draft.enabled {
                        palette.accent
                    } else {
                        palette.border
                    })
                    .child(div().size(px(16.)).rounded_full().bg(gpui::white())),
            );
        let switch = if disabled {
            switch.opacity(0.65)
        } else {
            activate(
                switch.hover(|style| style.bg(palette.card_hover)),
                None,
                palette,
                click(cx, |dialog, _, cx| {
                    dialog.enabled = !dialog.enabled;
                    dialog.feedback = None;
                    cx.notify();
                }),
            )
        };
        let mut use_default = Button::new(
            "topic-summary-default-prompt",
            text("topicSummarySettings.topic.useDefault"),
        );
        use_default.appearance = ButtonAppearance::Ghost;
        use_default.disabled = disabled || draft.prompt.is_none();
        let use_default = use_default
            .render(
                palette,
                None,
                click(cx, |dialog, _, cx| {
                    dialog.use_default_prompt = true;
                    dialog
                        .prompt
                        .update(cx, |editor, cx| editor.show_reply("", cx));
                    dialog.feedback = None;
                    cx.notify();
                }),
            )
            .debug_selector(|| "topic-summary-default-prompt".into());

        div().flex().flex_col().gap_4()
            .child(switch)
            .child(div().flex().flex_col().gap_2()
                .child(div().flex().flex_wrap().justify_between().items_center().gap_2()
                    .child(label(palette, TextRole::Label, text("topicSummarySettings.topic.prompt")))
                    .child(use_default))
                .child(div().id("topic-summary-prompt").debug_selector(|| "topic-summary-prompt".into())
                    .relative().h(px(144.)).overflow_y_scroll().rounded(px(8.)).border_1()
                    .border_color(palette.border).bg(palette.field).px_3()
                    .when(disabled, |field| {
                        field.child(label(palette, TextRole::Body, draft.prompt.clone().unwrap_or_default()).py_2())
                    })
                    .when(!disabled, |field| {
                        field.when(self.prompt.read(cx).text().is_empty(), |field| field.child(
                            label(palette, TextRole::Body, text("topicSummarySettings.topic.promptPlaceholder"))
                                .absolute().top_2().text_color(palette.text_muted)
                        )).child(self.prompt.clone())
                        .on_mouse_down(MouseButton::Left, cx.listener(|dialog, _, window, cx| {
                            window.focus(&dialog.prompt.read(cx).focus_handle(cx));
                        }))
                    }))
                .child(div().flex().justify_between().gap_2()
                    .child(label(palette, TextRole::Caption, if draft.prompt.is_none() {
                        text("topicSummarySettings.topic.defaultPrompt")
                    } else {
                        text("topicSummarySettings.topic.customPromptHint")
                    }))
                    .children(draft.prompt.as_ref().map(|prompt| {
                        label(palette, TextRole::Caption, format!("{}/{MAX_PROMPT_CHARS}", prompt.chars().count()))
                    })))
                .when(!draft.valid_prompt(), |section| section.child(
                    label(palette, TextRole::Caption,
                        text("desktop.conversation.summarySettings.enterBetween1And16384Characters"))
                        .text_color(palette.danger)
                )))
            .child(div().flex().flex_wrap().items_center().justify_between().gap_4()
                .px_4().py_3().rounded(px(12.)).border_1().border_color(palette.border).bg(palette.card)
                .child(div().min_w(px(144.)).flex_1()
                    .child(label(palette, TextRole::Label, text("topicSummarySettings.topic.reasoning")))
                    .child(label(palette, TextRole::Caption, text("desktop.conversation.summarySettings.providerDefaultLeavesTheChoiceToTheProviderOff"))))
                .child(self.reasoning_control(palette, language, disabled, cx)))
    }

    /// Показывает результат, отмену черновика и сохранение в нижней части диалога.
    fn footer(&self, palette: Palette, language: Language, cx: &Context<Self>) -> Div {
        let text = move |key| language.tr(key);
        let draft = self.draft(cx).normalized();
        let dirty = self.base.as_ref().is_some_and(|base| *base != draft);
        let saving = self.base.is_some() && self.request.is_some();
        let denied = self.feedback == Some(Feedback::Denied);
        let mut cancel = Button::new("topic-summary-cancel", text("common.cancel"));
        cancel.disabled = !dirty || saving || denied;
        let mut save = Button::new(
            "topic-summary-save",
            if saving {
                text("topicSummarySettings.status.saving")
            } else {
                text("common.save")
            },
        );
        save.appearance = ButtonAppearance::Primary;
        save.disabled = cancel.disabled || !draft.valid_prompt();
        let feedback = match self.feedback {
            Some(Feedback::Denied) => Some((
                text("desktop.conversation.summarySettings.permissionDenied"),
                palette.danger,
            )),
            Some(Feedback::Failed) if self.base.is_some() => {
                Some((text("desktop.general.saveFailed"), palette.danger))
            }
            Some(Feedback::Saved) => Some((
                text("desktop.conversation.summarySettings.settingsSaved"),
                palette.presence_online,
            )),
            _ if dirty && !saving => Some((
                text("topicSummarySettings.status.unsaved"),
                palette.presence_away,
            )),
            _ => None,
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap_2()
            .px_8()
            .py_4()
            .border_t_1()
            .border_color(palette.border)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .children(feedback.map(|(caption, color)| {
                        label(palette, TextRole::Caption, caption).text_color(color)
                    })),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        cancel
                            .render(
                                palette,
                                None,
                                click(cx, |dialog, _, cx| {
                                    if let Some(base) = dialog.base.clone() {
                                        dialog.set_draft(&base, cx);
                                        dialog.feedback = None;
                                        cx.notify();
                                    }
                                }),
                            )
                            .min_w(px(112.)),
                    )
                    .child(
                        save.render(palette, None, click(cx, Self::save))
                            .min_w(px(144.))
                            .debug_selector(|| "topic-summary-save".into()),
                    ),
            )
    }
}

impl Render for SummaryDialog {
    /// Показывает форму в размерах оригинального диалога, с прокруткой на узком окне.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.is_current(cx) {
            self.close(window, cx);
            return div();
        }
        let language = Preferences::read_for(cx, &self.address.route.owner).language;
        let text = move |key| language.tr(key);
        let palette = Palette::for_window(window, cx);
        let viewport = window.viewport_size();
        let mut close = Button::new("topic-summary-close", "");
        close.icon = Some(Icon::Close);
        close.appearance = ButtonAppearance::Ghost;
        let content = match (self.base.is_some(), self.request.is_some(), self.feedback) {
            (true, _, _) => self.form(palette, language, cx),
            (false, true, _) => label(
                palette,
                TextRole::Body,
                text("desktop.conversation.summarySettings.loadingSettings"),
            ),
            (false, false, Some(Feedback::Denied)) => label(
                palette,
                TextRole::Body,
                text("desktop.conversation.summarySettings.permissionDenied"),
            )
            .text_color(palette.danger),
            _ => div()
                .child(label(
                    palette,
                    TextRole::Body,
                    text("desktop.conversation.summarySettings.couldNotLoadSettings"),
                ))
                .child(
                    Button::new("topic-summary-retry", text("common.retry")).render(
                        palette,
                        None,
                        click(cx, |dialog, _, cx| dialog.request(None, cx)),
                    ),
                ),
        };
        div()
            .debug_selector(|| "topic-summary-dialog".into())
            .w((viewport.width - px(32.)).clamp(px(0.), px(1120.)))
            .h((viewport.height * 0.92).min(px(680.)))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(SURFACE_RADIUS)
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .child(
                div()
                    .h(px(88.))
                    .flex()
                    .flex_none()
                    .items_start()
                    .justify_between()
                    .gap_4()
                    .px_8()
                    .pt_6()
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .child(label(
                                palette,
                                TextRole::ProfileHeading,
                                text("topicSummarySettings.title"),
                            ))
                            .child(
                                label(palette, TextRole::Caption, self.subtitle.clone())
                                    .mt_1()
                                    .truncate(),
                            ),
                    )
                    .child(
                        close.render(palette, None, click(cx, Self::close)).tooltip(
                            move |_, app| tooltip(palette, text("common.close").into(), app),
                        ),
                    ),
            )
            .child(
                div()
                    .h(px(52.))
                    .flex()
                    .flex_none()
                    .items_end()
                    .border_b_1()
                    .border_color(palette.border)
                    .px_8()
                    .child(
                        label(
                            palette,
                            TextRole::Label,
                            text("topicSummarySettings.topic.title"),
                        )
                        .px_4()
                        .py_3()
                        .border_b_2()
                        .border_color(palette.accent),
                    ),
            )
            .child(
                div()
                    .id("topic-summary-body")
                    .min_h_0()
                    .flex_1()
                    .overflow_y_scroll()
                    .px_8()
                    .py_5()
                    .child(content),
            )
            .children(
                self.base
                    .is_some()
                    .then(|| self.footer(palette, language, cx)),
            )
    }
}

/// Переводит серверный уровень в название пункта списка.
fn effort_name(value: Option<ReasoningEffort>, language: Language) -> &'static str {
    match value {
        None => language.tr("topicSummarySettings.topic.reasoningDefault"),
        Some(ReasoningEffort::Off) => language.tr("topicSummarySettings.topic.reasoningOff"),
        Some(ReasoningEffort::Minimal) => {
            language.tr("topicSummarySettings.topic.reasoningMinimal")
        }
        Some(ReasoningEffort::Low) => language.tr("topicSummarySettings.topic.reasoningLow"),
        Some(ReasoningEffort::Medium) => language.tr("topicSummarySettings.topic.reasoningMedium"),
        Some(ReasoningEffort::High) => language.tr("topicSummarySettings.topic.reasoningHigh"),
    }
}

/// Направляет действие в живой диалог через его GPUI-линзу.
fn click(
    cx: &Context<SummaryDialog>,
    handle: impl Fn(&mut SummaryDialog, &mut Window, &mut Context<SummaryDialog>) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let target = cx.entity().downgrade();
    move |window, app| {
        let _ = target.update(app, |dialog, cx| handle(dialog, window, cx));
    }
}
