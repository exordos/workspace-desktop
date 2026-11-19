//! Показывает шаги входа в Workspace и передаёт данные рабочему потоку.
//! Сначала описаны состояние формы и переходы, затем её разметка и действия.

use gpui::{
    App, Context, Div, Entity, FocusHandle, KeyBinding, ScrollHandle, Stateful, Subscription, Task,
    Window, div, prelude::*, px,
};
use uuid::Uuid;

use super::{
    runtime::{LoginAttempt, LoginError, RuntimeClient},
    ui::{
        Button, ButtonAppearance, Icon, Palette, TextInput, TextInputChanged, TextInputSubmitted,
        TextRole, label,
    },
};

gpui::actions!(workspace_sign_in, [Back]);

const KEY_CONTEXT: &str = "WorkspaceSignIn";

/// Шаг формы и данные, которые нужны, пока он открыт.
enum Step {
    Server,      // Указываем адрес Workspace.
    Credentials, // Вводим логин и пароль для этого адреса.
    Otp,         // Вводим код только по запросу IAM.
    Project {
        attempt: LoginAttempt, // Содержит доступные проекты и отменяет попытку при удалении.
        selected: usize,       // Индекс выбранного проекта в списке этой попытки.
        focus: Vec<FocusHandle>, // Устойчивый фокус каждой строки списка.
        scroll: ScrollHandle,  // Прокрутка, которая показывает фокус при обходе клавишей Tab.
        focused: Option<usize>, // Последняя сфокусированная строка, если фокус в списке.
    },
    Finished, // Вход сохранён. Оболочка ждёт опубликованный снимок сессий.
}

/// Сведения о регистрации на выбранном сервере, которые приходят независимо от входа.
enum Registration {
    Hidden, // Сервер ещё не выбран либо доступной ссылки нет.
    Loading {
        id: Uuid,        // Номер запроса для отсечения ответа от предыдущего сервера.
        _task: Task<()>, // Удаление прекращает ожидание ответа.
    },
    Available(String), // HTTPS-адрес регистрации из публичных настроек выбранного сервера.
}

/// Состояние одной формы. Аккаунты, токены и проверка доступа остаются в авторизации.
pub(super) struct SignIn {
    runtime: RuntimeClient,                  // Рабочий поток открытого профиля.
    step: Step,                              // Видимая часть формы.
    server: Entity<TextInput>, // Адрес Workspace, после проверки хранится в нормализованном виде.
    login: Entity<TextInput>,  // Логин, сохраняется при возврате к полям учётных данных.
    password: Entity<TextInput>, // Пароль нужен до завершения проверки, включая шаг OTP.
    otp: Entity<TextInput>,    // Одноразовый код, очищается после ответа и при возврате.
    request: Option<(Uuid, Task<()>)>, // Одно ожидание ответа. Удаление отменяет ожидание.
    registration: Registration, // Ссылка на регистрацию и её отдельный запрос.
    issue: Option<LoginError>, // Категория ошибки для сообщения и рамки проблемного поля.
    focus_step: bool,          // Переместить фокус на поле нового шага в ближайшем кадре.
    _input_subscriptions: Vec<Subscription>, // Обновляют кнопки при вводе и передают Enter.
}

impl SignIn {
    /// Создаёт форму с пустым адресом или адресом Workspace для повторного входа.
    pub(super) fn new(
        runtime: RuntimeClient,
        server_hint: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Self {
        let server = cx
            .new(|cx| TextInput::new("sign-in-server", "https://workspace.example.com", false, cx));
        if let Some(value) = server_hint {
            server.update(cx, |input, cx| input.set_text(value, cx));
        }
        let login = cx.new(|cx| TextInput::new("sign-in-login", "Ваш логин или почта", false, cx));
        let password = cx.new(|cx| TextInput::new("sign-in-password", "Ваш пароль", true, cx));
        let otp = cx.new(|cx| TextInput::new("sign-in-otp", "000000", true, cx));

        let mut subscriptions = Vec::new();
        for field in [&server, &login, &password, &otp] {
            subscriptions.push(
                cx.subscribe(field, |view, field, _: &TextInputChanged, cx| {
                    view.input_changed(field, cx);
                }),
            );
            subscriptions.push(cx.subscribe(field, |view, _, _: &TextInputSubmitted, cx| {
                view.submit(cx);
            }));
        }

        Self {
            runtime,
            step: Step::Server,
            server,
            login,
            password,
            otp,
            request: None,
            registration: Registration::Hidden,
            issue: None,
            focus_step: true,
            _input_subscriptions: subscriptions,
        }
    }

    /// Привязывает Escape к возврату только внутри формы входа.
    pub(super) fn bind_keys(app: &mut App) {
        app.bind_keys([KeyBinding::new("escape", Back, Some(KEY_CONTEXT))]);
    }

    /// Принимает в OTP шесть цифр, в том числе при вставке кода с пробелами.
    fn input_changed(&self, field: Entity<TextInput>, cx: &mut Context<Self>) {
        if field == self.otp && !field.read(cx).is_composing() {
            let code = field.read(cx).text();
            let digits: String = code.chars().filter(char::is_ascii_digit).take(6).collect();
            if digits != code {
                field.update(cx, |input, cx| input.set_text(&digits, cx));
            }
        }
        cx.notify();
    }

    /// Разрешает подтверждение только после ввода данных текущего шага.
    fn can_submit(&self, cx: &Context<Self>) -> bool {
        self.request.is_none()
            && match self.step {
                Step::Server => !self.server.read(cx).text().trim().is_empty(),
                Step::Credentials => {
                    !self.login.read(cx).text().trim().is_empty()
                        && !self.password.read(cx).text().is_empty()
                }
                Step::Otp => {
                    let code = self.otp.read(cx).text();
                    code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit())
                }
                Step::Project { .. } => true,
                Step::Finished => false,
            }
    }

    /// Направляет клик основной кнопки и Enter в одну операцию текущего шага.
    fn submit(&mut self, cx: &mut Context<Self>) {
        if !self.can_submit(cx)
            || [&self.server, &self.login, &self.password, &self.otp]
                .iter()
                .any(|field| field.read(cx).is_composing())
        {
            return;
        }
        self.set_issue(None, cx);
        match &self.step {
            Step::Server => self.confirm_server(cx),
            Step::Credentials => self.request_projects(false, cx),
            Step::Otp => self.request_projects(true, cx),
            Step::Project {
                attempt, selected, ..
            } => {
                let project = &attempt.projects()[*selected];
                self.confirm_project(attempt.id(), project.id.clone(), cx);
            }
            Step::Finished => {}
        }
        cx.notify();
    }

    /// Проверяет адрес через правила IAM и сохраняет точный origin для следующих запросов.
    fn confirm_server(&mut self, cx: &mut Context<Self>) {
        let address = self.server.read(cx).text().trim();
        match self.runtime.validate_login_server(address) {
            Ok(address) => {
                self.server
                    .update(cx, |input, cx| input.set_text(&address, cx));
                self.step = Step::Credentials;
                self.focus_step = true;
                self.load_registration(address, cx);
            }
            Err(error) => {
                self.set_issue(Some(error), cx);
                self.focus_step = true;
            }
        }
    }

    /// Ищет ссылку для подтверждённого адреса, оставляя поля входа доступными.
    fn load_registration(&mut self, address: String, cx: &mut Context<Self>) {
        let runtime = self.runtime.clone();
        let id = Uuid::new_v4();
        let task = cx.spawn(async move |view, cx| {
            let url = runtime.registration_url(address).await.ok().flatten();
            let _ = view.update(cx, |view, cx| view.registration_received(id, url, cx));
        });
        self.registration = Registration::Loading { id, _task: task };
    }

    /// Показывает ссылку только для текущего запроса настроек организации.
    fn registration_received(&mut self, id: Uuid, url: Option<String>, cx: &mut Context<Self>) {
        if matches!(self.registration, Registration::Loading { id: current, .. } if current == id) {
            self.registration = url.map_or(Registration::Hidden, Registration::Available);
            cx.notify();
        }
    }

    /// Подсвечивает поля, которые пользователь может исправить после отклонения данных.
    fn set_issue(&mut self, issue: Option<LoginError>, cx: &mut Context<Self>) {
        self.issue = issue;
        let credentials = issue == Some(LoginError::InvalidCredentials);
        for (field, invalid) in [
            (&self.server, issue == Some(LoginError::InvalidServer)),
            (&self.login, credentials),
            (&self.password, credentials),
            (&self.otp, issue == Some(LoginError::InvalidOtp)),
        ] {
            field.update(cx, |input, cx| input.set_invalid(invalid, cx));
        }
    }

    /// Замораживает поля на время проверки, чтобы ответ относился к введённым данным.
    fn set_input_enabled(&self, enabled: bool, cx: &mut Context<Self>) {
        for field in [&self.login, &self.password, &self.otp] {
            field.update(cx, |input, cx| input.set_enabled(enabled, cx));
        }
    }

    /// Проверяет логин и пароль либо подтверждает уже запрошенный OTP.
    fn request_projects(&mut self, use_otp: bool, cx: &mut Context<Self>) {
        let runtime = self.runtime.clone();
        let server = self.server.read(cx).text().to_owned();
        let login = self.login.read(cx).text().trim().to_owned();
        let password = self.password.read(cx).text().to_owned();
        let otp = use_otp.then(|| self.otp.read(cx).text().to_owned());
        let op_id = Uuid::new_v4();

        self.set_input_enabled(false, cx);
        let task = cx.spawn(async move |view, cx| {
            let result = runtime.begin_login(server, login, password, otp).await;
            let _ = view.update(cx, |view, cx| view.projects_received(op_id, result, cx));
        });
        self.request = Some((op_id, task));
    }

    /// Открывает следующий шаг либо восстанавливает возможность повторить проверку.
    fn projects_received(
        &mut self,
        op_id: Uuid,
        result: Result<LoginAttempt, LoginError>,
        cx: &mut Context<Self>,
    ) {
        if !self.finish_request(op_id) {
            return;
        }
        self.set_input_enabled(true, cx);
        self.otp.update(cx, TextInput::clear);
        match result {
            Ok(attempt) => {
                self.password.update(cx, TextInput::clear);
                let focus = attempt
                    .projects()
                    .iter()
                    .map(|_| cx.focus_handle())
                    .collect();
                self.step = Step::Project {
                    attempt,
                    selected: 0,
                    focus,
                    scroll: ScrollHandle::new(),
                    focused: None,
                };
            }
            Err(LoginError::OtpRequired) => self.step = Step::Otp,
            Err(error) => {
                let can_retry =
                    matches!(error, LoginError::TemporarilyUnavailable | LoginError::Busy)
                        || (matches!(self.step, Step::Otp) && error == LoginError::InvalidOtp);
                // Для повтора этой проверки IAM снова потребует пароль.
                if !can_retry {
                    self.password.update(cx, TextInput::clear);
                    self.step = Step::Credentials;
                }
                self.set_issue(Some(error), cx);
            }
        }
        self.focus_step = true;
        cx.notify();
    }

    /// Принимает только ответ операции, которая всё ещё относится к открытому шагу.
    fn finish_request(&mut self, op_id: Uuid) -> bool {
        if !self
            .request
            .as_ref()
            .is_some_and(|(current, _)| *current == op_id)
        {
            return false;
        }
        self.request = None;
        true
    }

    /// Передаёт IAM выбор из списка той же попытки входа.
    fn confirm_project(
        &mut self,
        attempt: workspace_core::auth::LoginAttemptId,
        project_id: String,
        cx: &mut Context<Self>,
    ) {
        let runtime = self.runtime.clone();
        let op_id = Uuid::new_v4();
        let task = cx.spawn(async move |view, cx| {
            let result = runtime
                .complete_login(attempt, project_id)
                .await
                .map(|_| ());
            let _ = view.update(cx, |view, cx| view.project_confirmed(op_id, result, cx));
        });
        self.request = Some((op_id, task));
    }

    /// Показывает ошибку выбора или ждёт, пока оболочка увидит новый активный вход.
    fn project_confirmed(
        &mut self,
        op_id: Uuid,
        result: Result<(), LoginError>,
        cx: &mut Context<Self>,
    ) {
        if !self.finish_request(op_id) {
            return;
        }
        match result {
            Ok(()) => self.step = Step::Finished,
            Err(error) => {
                self.set_issue(Some(error), cx);
                if !matches!(
                    error,
                    LoginError::Busy
                        | LoginError::TemporarilyUnavailable
                        | LoginError::UnknownProject
                ) {
                    self.step = Step::Credentials;
                }
                self.focus_step = true;
            }
        }
        cx.notify();
    }

    /// Возвращает к предыдущему шагу и очищает секреты закрытого шага.
    fn back(&mut self, cx: &mut Context<Self>) {
        let previous = match self.step {
            Step::Server | Step::Finished => return,
            Step::Credentials => Step::Server,
            Step::Otp => Step::Credentials,
            // Фиксация сессии уже могла начаться, поэтому ждём её итог.
            Step::Project { .. } if self.request.is_some() => return,
            Step::Project { .. } => Step::Credentials,
        };
        self.request = None;
        self.set_input_enabled(true, cx);
        self.password.update(cx, TextInput::clear);
        self.otp.update(cx, TextInput::clear);
        if matches!(previous, Step::Server) {
            self.login.update(cx, TextInput::clear);
            self.registration = Registration::Hidden;
        }
        self.step = previous;
        self.set_issue(None, cx);
        self.focus_step = true;
        cx.notify();
    }

    /// Переносит фокус на видимое поле либо на текущий проект при смене шага.
    fn focus_current_step(&mut self, window: &mut Window, cx: &Context<Self>) {
        if !std::mem::take(&mut self.focus_step) {
            return;
        }
        match &self.step {
            Step::Server => self.server.read(cx).focus(window),
            Step::Credentials if self.login.read(cx).text().trim().is_empty() => {
                self.login.read(cx).focus(window)
            }
            Step::Credentials => self.password.read(cx).focus(window),
            Step::Otp => self.otp.read(cx).focus(window),
            Step::Project {
                selected, focus, ..
            } => window.focus(&focus[*selected]),
            Step::Finished => {}
        }
    }

    /// Прокручивает список при смене фокуса, оставляя свободную прокрутку мышью.
    fn keep_project_in_view(&mut self, window: &Window) {
        if let Step::Project {
            focus,
            focused,
            scroll,
            ..
        } = &mut self.step
        {
            let next = focus.iter().position(|handle| handle.is_focused(window));
            if next != *focused {
                *focused = next;
                if let Some(index) = next {
                    scroll.scroll_to_item(index);
                }
            }
        }
    }

    /// Возвращает подсказку и текст кнопки независимо от геометрии формы.
    fn copy(&self) -> (&'static str, &'static str) {
        let (hint, button) = match self.step {
            Step::Server => ("Укажите адрес вашей организации.", "Далее"),
            Step::Credentials => ("Введите учётные данные для входа.", "Далее"),
            Step::Otp => ("Введите шестизначный одноразовый код.", "Подтвердить"),
            Step::Project { .. } => ("Выберите проект, в который хотите войти.", "Войти"),
            Step::Finished => ("Подключаем рабочее пространство…", ""),
        };
        (
            hint,
            if self.request.is_some() {
                "Входим…"
            } else {
                button
            },
        )
    }

    /// Рисует только поля видимого шага, чтобы Tab не попадал в скрытые поля.
    fn fields(&self, palette: Palette, cx: &Context<Self>) -> Div {
        let body = div().flex().flex_col().gap_4();
        match &self.step {
            Step::Server => body.child(form_field(palette, "Адрес Workspace", self.server.clone())),
            Step::Credentials => body
                .child(
                    label(
                        palette,
                        TextRole::Caption,
                        self.server.read(cx).text().to_owned(),
                    )
                    .w_full()
                    .min_w_0()
                    .overflow_hidden(),
                )
                .child(form_field(palette, "Почта или логин", self.login.clone()))
                .child(form_field(palette, "Пароль", self.password.clone())),
            Step::Otp => body.child(form_field(palette, "Одноразовый код", self.otp.clone())),
            Step::Project {
                attempt,
                selected,
                focus,
                scroll,
                ..
            } => body.child(form_field(
                palette,
                "Проект",
                self.projects(attempt, *selected, focus, scroll, palette, cx),
            )),
            Step::Finished => body,
        }
    }

    /// Показывает доступные проекты, выделяет выбранный и ограничивает высоту списка.
    fn projects(
        &self,
        attempt: &LoginAttempt,
        selected: usize,
        focus: &[FocusHandle],
        scroll: &ScrollHandle,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let target = cx.weak_entity();
        div()
            .id("sign-in-projects")
            .max_h(px(180.))
            .overflow_y_scroll()
            .track_scroll(scroll)
            .flex()
            .flex_col()
            .gap_1()
            .children(
                attempt
                    .projects()
                    .iter()
                    .enumerate()
                    .map(|(index, project)| {
                        let selected = selected == index;
                        let target = target.clone();
                        let name = project.name.clone();
                        let mut button = Button::new(("sign-in-project", index), name.clone());
                        button.disabled = self.request.is_some();
                        button.icon = selected.then_some(Icon::Check);
                        button
                            .render(palette, Some(&focus[index]), move |_, app| {
                                let _ = target.update(app, |view, cx| {
                                    if let Step::Project { selected, .. } = &mut view.step {
                                        *selected = index;
                                        cx.notify();
                                    }
                                });
                            })
                            .flex_none()
                            .justify_start()
                            .when(selected, |row| row.bg(palette.card_selected))
                            .debug_selector(move || format!("sign-in-project-{index}"))
                            .tooltip(move |_, app| {
                                super::ui::popup::tooltip(palette, name.clone().into(), app)
                            })
                    }),
            )
    }

    /// Предлагает перейти к регистрации, если сервер сообщил адрес страницы.
    fn registration_action(&self, palette: Palette, cx: &Context<Self>) -> Option<Stateful<Div>> {
        if let (Step::Credentials, Registration::Available(url)) = (&self.step, &self.registration)
        {
            let target = cx.weak_entity();
            let url = url.clone();
            return Some(
                Button::new("sign-in-register", "Зарегистрироваться")
                    .render(palette, None, move |_, app| {
                        let _ = target.update(app, |view, cx| {
                            if matches!(
                                (&view.step, &view.registration),
                                (Step::Credentials, Registration::Available(current)) if current == &url
                            ) {
                                cx.open_url(&url);
                            }
                        });
                    })
                    .debug_selector(|| "sign-in-register".into()),
            );
        }
        None
    }

    /// Рисует подтверждение и возврат с общей механикой клавиатуры GPUI.
    fn actions(&self, caption: &'static str, palette: Palette, cx: &Context<Self>) -> Div {
        let target = cx.weak_entity();
        let back_target = target.clone();
        let mut submit = Button::new("sign-in-submit", caption);
        submit.appearance = ButtonAppearance::Primary;
        submit.disabled = !self.can_submit(cx);
        let mut back = Button::new("sign-in-back", "Назад");
        back.appearance = ButtonAppearance::Ghost;
        back.disabled = matches!(self.step, Step::Project { .. }) && self.request.is_some();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                submit
                    .render(palette, None, move |_, app| {
                        let _ = target.update(app, SignIn::submit);
                    })
                    .debug_selector(|| "sign-in-submit".into()),
            )
            .children(self.registration_action(palette, cx))
            .when(!matches!(self.step, Step::Server), |actions| {
                actions.child(
                    back.render(palette, None, move |_, app| {
                        let _ = back_target.update(app, SignIn::back);
                    })
                    .debug_selector(|| "sign-in-back".into()),
                )
            })
    }
}

impl Render for SignIn {
    /// Центрирует форму в окне и оставляет прокрутку для тесного размера.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.focus_current_step(window, cx);
        self.keep_project_in_view(window);
        let palette = Palette::for_window(window, cx);
        let (hint, submit) = self.copy();
        div()
            .id("workspace-sign-in")
            .debug_selector(|| "workspace-sign-in".into())
            .size_full()
            .min_h_0()
            .overflow_x_hidden()
            .overflow_y_scroll()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|view, _: &Back, _, cx| view.back(cx)))
            .child(
                div()
                    .w_full()
                    // В прокручиваемом слое процентная высота не даёт места для центрирования.
                    .min_h(window.viewport_size().height)
                    .p_4()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .id("sign-in-form")
                            .debug_selector(|| "sign-in-form".into())
                            .w_full()
                            .max_w(px(448.))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap_6()
                            .child(
                                div()
                                    .text_center()
                                    .child(
                                        label(palette, TextRole::Heading, "Вход в Workspace")
                                            .text_size(px(20.))
                                            .line_height(px(28.)),
                                    )
                                    .child(
                                        label(palette, TextRole::Body, hint)
                                            .text_color(palette.text_secondary),
                                    ),
                            )
                            .when(!matches!(self.step, Step::Finished), |form| {
                                form.child(self.fields(palette, cx))
                                    .children(self.issue.map(|issue| {
                                        label(palette, TextRole::Body, error_message(issue))
                                            .id("sign-in-error")
                                            .debug_selector(|| "sign-in-error".into())
                                            .p_2()
                                            .rounded(px(8.))
                                            .border_1()
                                            .border_color(palette.danger)
                                            .text_color(palette.danger)
                                    }))
                                    .child(self.actions(submit, palette, cx))
                            }),
                    ),
            )
    }
}

/// Помещает видимую подпись над полем или списком внутри формы входа.
fn form_field(palette: Palette, caption: &'static str, control: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(label(palette, TextRole::Label, caption))
        .child(control)
}

/// Переводит категории ошибок в локальные подсказки без подробностей ответа IAM.
fn error_message(error: LoginError) -> &'static str {
    match error {
        LoginError::InvalidServer => "Введите действительный HTTPS-адрес Workspace.",
        LoginError::OtpRequired => "Для входа требуется одноразовый код.",
        LoginError::InvalidOtp => "Код не подошёл. Введите новый код.",
        LoginError::InvalidCredentials => "Проверьте логин и пароль.",
        LoginError::TemporarilyUnavailable => "Сервер Workspace недоступен. Повторите попытку.",
        LoginError::AttemptUnavailable => "Время входа истекло. Введите пароль ещё раз.",
        LoginError::UnknownProject => "Проект недоступен. Выберите другой проект.",
        LoginError::NoProjects => "У этого аккаунта нет доступных проектов.",
        LoginError::InvalidResponse => "Не удалось обработать ответ сервера Workspace.",
        LoginError::Storage => "Не удалось открыть или сохранить данные входа. Повторите попытку.",
        LoginError::Busy => "Вход временно недоступен. Повторите попытку.",
        LoginError::Stopped => "Фоновая работа остановлена. Перезапустите Workspace.",
    }
}

#[cfg(all(test, feature = "gpui-test"))]
mod tests;
