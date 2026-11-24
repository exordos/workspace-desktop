//! Управляет созданием, редактированием и подтверждением удаления папки.
//! Здесь описаны данные формы и переходы между вводом, ожиданием и результатом.

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, KeyBinding, Subscription, Window,
};
use uuid::Uuid;

use crate::gpui_client::{
    contracts::{FolderCommand, FolderCommandError},
    preferences::Preferences,
    ui::{TextInput, TextInputChanged, TextInputSubmitted},
};

mod view;

#[cfg(all(test, feature = "gpui-test"))]
mod tests;

gpui::actions!(workspace_folder_dialog, [Dismiss, Next, Previous]);

const KEY_CONTEXT: &str = "WorkspaceFolderDialog";
const MAX_TITLE_LENGTH: usize = 64;
// Значения серверного ARGB из палитры FolderFormModal.
const COLORS: [(u32, &str); 8] = [
    (0xff2563eb, "desktop.sidebar.folders.colors.blue"),
    (0xff4f46e5, "desktop.sidebar.folders.colors.indigo"),
    (0xff0d9488, "desktop.sidebar.folders.colors.teal"),
    (0xff059669, "desktop.sidebar.folders.colors.green"),
    (0xfff59e0b, "desktop.sidebar.folders.colors.amber"),
    (0xffdc2626, "desktop.sidebar.folders.colors.red"),
    (0xff6b7280, "desktop.sidebar.folders.colors.gray"),
    (0xff334155, "desktop.sidebar.folders.colors.graphite"),
];

/// Определяет действие и начальные данные открытого диалога.
enum Mode {
    Create(Uuid), // UUID новой папки. Повтор отправляет его снова.
    Edit {
        id: Uuid,                    // Папка, открытая для изменения.
        original_title: String,      // Название на момент открытия.
        original_color: Option<u32>, // Цвет на момент открытия, если он есть.
    },
    Delete {
        id: Uuid,                          // Папка, которую требуется удалить.
        title: String,                     // Название для явного подтверждения.
        after_delete: Option<FocusHandle>, // «Все чаты» после исчезновения исходной кнопки.
    },
}

/// Состояние запроса, которому принадлежит открытый диалог.
enum Status {
    Editing,                   // Форма готова к вводу и первой отправке.
    Pending,                   // Команда выполняется. Повтор и закрытие заблокированы.
    Error(FolderCommandError), // Операция завершилась, форма сохраняет текущие значения.
}

impl Status {
    /// Разрешает менять поля, пока известно, что сервер ещё их не применил.
    fn fields_enabled(&self) -> bool {
        matches!(
            self,
            Self::Editing
                | Self::Error(FolderCommandError::Rejected | FolderCommandError::WorkerUnavailable)
        )
    }
}

/// Локальные данные одной открытой формы.
struct OpenDialog {
    mode: Mode,         // Сценарий и исходные данные.
    status: Status,     // Состояние фоновой команды.
    color: Option<u32>, // Выбранный цвет, None сохраняет отсутствие акцента.
}

impl OpenDialog {
    /// Разрешает повтор неизвестного результата создания с прежним UUID и полями.
    fn can_retry_create(&self) -> bool {
        matches!(
            (&self.mode, &self.status),
            (Mode::Create(_), Status::Error(FolderCommandError::Unknown))
        )
    }
}

/// Показывает модальный диалог. Core выполняет команды и подтверждает результат.
pub(in crate::gpui_client) struct FolderDialog {
    open: Option<OpenDialog>, // Текущая форма. None скрывает перекрытие.
    name: Entity<TextInput>,  // Ввод названия для создания и редактирования.
    region: FocusHandle,      // Область диалога и фокус на время ожидания.
    close: FocusHandle,       // Кнопка закрытия в заголовке.
    cancel: FocusHandle,      // Кнопка отмены и начальный фокус удаления.
    submit: FocusHandle,      // Кнопка подтверждения.
    return_focus: Option<FocusHandle>, // Контрол, который открыл диалог.
    _input_events: [Subscription; 2], // Правки текста и Enter после системного ввода.
}

impl EventEmitter<FolderCommand> for FolderDialog {}

impl FolderDialog {
    /// Создаёт закрытый диалог и подписывается на события поля названия.
    pub(in crate::gpui_client) fn new(cx: &mut Context<Self>) -> Self {
        let placeholder = Preferences::read(cx).language.tr("folder.folderName");
        let name = cx.new(|cx| TextInput::new("folder-dialog-name", placeholder, false, cx));
        let changed = cx.subscribe(&name, |dialog, _, _: &TextInputChanged, cx| {
            dialog.changed(cx);
        });
        let submitted = cx.subscribe(&name, |dialog, _, _: &TextInputSubmitted, cx| {
            dialog.submit(cx);
        });
        Self {
            open: None,
            name,
            region: cx.focus_handle(),
            close: cx.focus_handle(),
            cancel: cx.focus_handle(),
            submit: cx.focus_handle(),
            return_focus: None,
            _input_events: [changed, submitted],
        }
    }

    /// Обрабатывает Escape и Tab только внутри диалога.
    pub(in crate::gpui_client) fn bind_keys(app: &mut App) {
        app.bind_keys([
            KeyBinding::new("escape", Dismiss, Some(KEY_CONTEXT)),
            KeyBinding::new("tab", Next, Some(KEY_CONTEXT)),
            KeyBinding::new("shift-tab", Previous, Some(KEY_CONTEXT)),
        ]);
    }

    /// Сообщает оболочке, что в окне открыт модальный диалог.
    pub(in crate::gpui_client) fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Открывает пустое название и выбирает первый цвет палитры.
    pub(in crate::gpui_client) fn open_create(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin(
            Mode::Create(Uuid::new_v4()),
            "",
            Some(COLORS[0].0),
            window,
            cx,
        );
    }

    /// Открывает форму с текущими названием и цветом пользовательской папки.
    pub(in crate::gpui_client) fn open_edit(
        &mut self,
        id: Uuid,
        title: String,
        color: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin(
            Mode::Edit {
                id,
                original_title: title.clone(),
                original_color: color,
            },
            &title,
            color,
            window,
            cx,
        );
    }

    /// Запрашивает явное подтверждение. Фокус по умолчанию остаётся на отмене.
    pub(in crate::gpui_client) fn open_delete(
        &mut self,
        id: Uuid,
        title: String,
        after_delete: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin(
            Mode::Delete {
                id,
                title,
                after_delete,
            },
            "",
            None,
            window,
            cx,
        );
    }

    /// Очищает форму и старый фокус при смене аккаунта или выходе.
    pub(in crate::gpui_client) fn reset(&mut self, cx: &mut Context<Self>) {
        self.return_focus = None;
        self.close(cx);
    }

    /// Закрывает подтверждённую операцию или показывает причину и сохраняет ввод.
    pub(in crate::gpui_client) fn finish(
        &mut self,
        result: Result<(), FolderCommandError>,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self
            .open
            .as_mut()
            .filter(|dialog| matches!(dialog.status, Status::Pending))
        else {
            return;
        };
        if matches!(result, Ok(()) | Err(FolderCommandError::Refresh))
            && let Mode::Delete {
                after_delete: Some(focus),
                ..
            } = &dialog.mode
        {
            self.return_focus = Some(focus.clone());
        }
        match result {
            Ok(()) => self.close(cx),
            Err(error) => {
                dialog.status = Status::Error(error);
                let editable = dialog.status.fields_enabled();
                self.name
                    .update(cx, |name, cx| name.set_enabled(editable, cx));
                cx.notify();
            }
        }
    }

    /// Запоминает триггер и готовит начальный ввод и фокус для нового диалога.
    fn begin(
        &mut self,
        mode: Mode,
        name: &str,
        color: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_open() {
            return;
        }
        self.return_focus = window.focused(cx);
        self.name.update(cx, |input, cx| {
            input.set_enabled(true, cx);
            input.set_text(name, cx);
        });
        self.open = Some(OpenDialog {
            mode,
            status: Status::Editing,
            color,
        });
        self.focus_initial(window, cx);
        cx.notify();
    }

    /// Убирает значения формы и оставляет возврат фокуса следующему кадру.
    fn close(&mut self, cx: &mut Context<Self>) {
        if self.open.take().is_some() {
            self.name.update(cx, |name, cx| name.clear(cx));
            cx.notify();
        }
    }

    /// Закрывает диалог до или после запроса, отклоняя повторный клик во время ожидания.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self
            .open
            .as_ref()
            .is_some_and(|dialog| !matches!(dialog.status, Status::Pending))
        {
            self.close(cx);
        }
    }

    /// Обновляет проверку длины названия и снимает старую ошибку после правки.
    fn changed(&mut self, cx: &mut Context<Self>) {
        let invalid = self.name.read(cx).text().trim().chars().count() > MAX_TITLE_LENGTH;
        self.name
            .update(cx, |name, cx| name.set_invalid(invalid, cx));
        if let Some(dialog) = &mut self.open
            && dialog.status.fields_enabled()
        {
            dialog.status = Status::Editing;
        }
        cx.notify();
    }

    /// Меняет цвет и убирает ошибку, которую пользователь решил исправить.
    fn choose_color(&mut self, color: u32, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.open
            && dialog.status.fields_enabled()
        {
            dialog.color = Some(color);
            dialog.status = Status::Editing;
            cx.notify();
        }
    }

    /// Проверяет, есть ли допустимое действие для открытой формы.
    fn can_submit(&self, cx: &Context<Self>) -> bool {
        let Some(dialog) = &self.open else {
            return false;
        };
        if !dialog.status.fields_enabled() && !dialog.can_retry_create() {
            return false;
        }
        let name = self.name.read(cx).text().trim();
        let name_valid = (1..=MAX_TITLE_LENGTH).contains(&name.chars().count());
        match &dialog.mode {
            Mode::Create(_) => name_valid,
            Mode::Edit {
                original_title,
                original_color,
                ..
            } => name_valid && (name != original_title.trim() || dialog.color != *original_color),
            Mode::Delete { .. } => true,
        }
    }

    /// Отправляет ровно одну команду с текущим значением формы или явным удалением.
    fn submit(&mut self, cx: &mut Context<Self>) {
        if !self.can_submit(cx) {
            return;
        }
        let dialog = self.open.as_mut().expect("проверена открытая форма");
        let title = self.name.read(cx).text().trim();
        let command = match &dialog.mode {
            Mode::Create(id) => FolderCommand::Create {
                id: *id,
                title: title.into(),
                color: dialog.color,
            },
            Mode::Edit {
                id,
                original_title,
                original_color,
            } => FolderCommand::Update {
                id: *id,
                title: (title != original_title.trim()).then(|| title.to_owned()),
                color: (dialog.color != *original_color).then_some(dialog.color),
            },
            Mode::Delete { id, .. } => FolderCommand::Delete(*id),
        };
        dialog.status = Status::Pending;
        self.name.update(cx, |name, cx| name.set_enabled(false, cx));
        cx.emit(command);
        cx.notify();
    }

    /// Ставит фокус в поле ввода или на безопасную отмену.
    fn focus_initial(&self, window: &mut Window, cx: &Context<Self>) {
        if let Some(dialog) = &self.open {
            if matches!(dialog.status, Status::Pending) {
                window.focus(&self.region);
            } else if matches!(dialog.mode, Mode::Delete { .. }) || !dialog.status.fields_enabled()
            {
                window.focus(&self.cancel);
            } else {
                self.name.read(cx).focus(window);
            }
        }
    }

    /// Обходит только видимые и доступные контролы открытого диалога.
    fn cycle_focus(&self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if self
            .open
            .as_ref()
            .is_some_and(|dialog| matches!(dialog.status, Status::Pending))
        {
            return;
        }
        if reverse {
            window.focus_prev();
        } else {
            window.focus_next();
        }
        if !self.region.contains_focused(window, cx) {
            let focus = if !reverse {
                &self.close
            } else if self.can_submit(cx) {
                &self.submit
            } else {
                &self.cancel
            };
            window.focus(focus);
        }
    }
}
