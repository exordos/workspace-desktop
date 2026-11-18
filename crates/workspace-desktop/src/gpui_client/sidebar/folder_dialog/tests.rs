//! Проверяет диалог по штатным событиям мыши и клавиатуры в окне GPUI.

use std::{cell::RefCell, rc::Rc};

use gpui::{
    Context, Entity, FocusHandle, KeyUpEvent, Keystroke, Modifiers, Render, TestAppContext,
    VisualTestContext, Window, div, prelude::*, px,
};

use crate::gpui_client::{
    contracts::AccountSession,
    sidebar::{FolderIntent, Sidebar},
    ui::{Button, Palette, ThemeMode},
};
use workspace_core::context::{AccountId, OwnerContext, SessionGeneration};

use super::*;

/// Главное окно с реальной кнопкой под модальным перекрытием.
struct Root {
    dialog: Entity<FolderDialog>, // Дочернее модальное представление.
    launcher: FocusHandle,        // Кнопка для проверки возврата фокуса.
    all_chats: FocusHandle,       // Кнопка, которая остаётся после удаления папки.
    clicks: usize,                // Сколько раз фон действительно получил клик.
}

impl Render for Root {
    /// Подключает открытие формы к обычной кнопке позади перекрытия.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = cx.entity().downgrade();
        let palette = Palette::for_mode(ThemeMode::Light);
        div()
            .relative()
            .size_full()
            .p(px(8.))
            .child(
                Button::new("folder-launcher", "Создать папку")
                    .render(palette, Some(&self.launcher), move |window, app| {
                        let _ = root.update(app, |view, cx| {
                            view.clicks += 1;
                            view.dialog
                                .update(cx, |view, cx| view.open_create(window, cx));
                        });
                    })
                    .debug_selector(|| "folder-launcher".into()),
            )
            .child(Button::new("folder-fallback", "Все чаты").render(
                palette,
                Some(&self.all_chats),
                |_, _| {},
            ))
            .child(self.dialog.clone())
    }
}

/// Данные отправленной команды без фонового исполнителя.
#[derive(Debug, PartialEq)]
enum Recorded {
    Create(Uuid, String, Option<u32>), // UUID, название и цвет новой папки.
    Update(Uuid, Option<String>, Option<Option<u32>>), // UUID и только изменённые поля.
    Delete(Uuid),                      // Явно подтверждённое удаление.
}

/// Компоненты и команды одного тестового окна.
struct Harness<'a> {
    root: Entity<Root>,                   // Окно и кнопка открытия.
    dialog: Entity<FolderDialog>,         // Сам диалог.
    commands: Rc<RefCell<Vec<Recorded>>>, // Полученные команды диалога.
    ui: &'a mut VisualTestContext,        // Мышь, ввод и проверка кадра.
}

impl<'a> Harness<'a> {
    /// Открывает тестовое окно и подписывается на настоящие события формы.
    fn new(cx: &'a mut TestAppContext) -> Self {
        cx.update(|app| {
            TextInput::bind_keys(app);
            FolderDialog::bind_keys(app);
        });
        let (root, ui) = cx.add_window_view(|_, cx| Root {
            dialog: cx.new(FolderDialog::new),
            launcher: cx.focus_handle(),
            all_chats: cx.focus_handle(),
            clicks: 0,
        });
        let dialog = ui.read(|app| root.read(app).dialog.clone());
        let commands = Rc::new(RefCell::new(Vec::new()));
        let recorded = commands.clone();
        ui.update(|window, app| {
            window.focus(&root.read(app).launcher);
            app.subscribe(&dialog, move |_, command, _| {
                let entry = match command {
                    FolderCommand::Create { id, title, color } => {
                        Recorded::Create(*id, title.clone(), *color)
                    }
                    FolderCommand::Update { id, title, color } => {
                        Recorded::Update(*id, title.clone(), *color)
                    }
                    FolderCommand::Delete(id) => Recorded::Delete(*id),
                    _ => panic!("диалог передал лишнее действие"),
                };
                recorded.borrow_mut().push(entry);
            })
            .detach();
        });
        ui.run_until_parked();
        Self {
            root,
            dialog,
            commands,
            ui,
        }
    }

    /// Возвращает ответ команды через обычную публичную точку входа.
    fn finish(&mut self, result: Result<(), FolderCommandError>) {
        self.ui.update(|_, app| {
            self.dialog
                .update(app, |dialog, cx| dialog.finish(result, cx))
        });
        self.ui.run_until_parked();
    }

    /// Открывает подтверждение удаления с постоянной кнопкой для возврата фокуса.
    fn open_delete(&mut self, id: Uuid) {
        self.ui.update(|window, app| {
            let fallback = self.root.read(app).all_chats.clone();
            self.dialog.update(app, |dialog, cx| {
                dialog.open_delete(id, "Проекты".into(), Some(fallback), window, cx)
            });
        });
        self.ui.run_until_parked();
    }

    /// Нажимает видимый контрол по фактическим границам в кадре.
    fn click(&mut self, selector: &'static str) {
        click(self.ui, selector);
    }

    /// Проверяет наличие диалога в рабочем кадре и исходный фокус после закрытия.
    fn restored(&mut self) {
        self.ui.update(|window, app| {
            assert!(!self.dialog.read(app).is_open());
            assert!(!window.is_action_available(&Dismiss, app));
            assert!(self.root.read(app).launcher.is_focused(window));
        });
    }
}

/// Нажимает элемент, показанный GPUI в последнем кадре.
fn click(ui: &mut VisualTestContext, selector: &'static str) {
    let bounds = ui
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("нет элемента {selector}"));
    ui.simulate_click(bounds.center(), Modifiers::none());
    ui.run_until_parked();
}

/// Сохраняет введённое название, цвет и UUID при потере подтверждения.
#[gpui::test]
fn create_folder_retries_the_same_command_and_traps_input(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.click("folder-launcher");
    assert!(h.ui.debug_bounds("folder-dialog-preview").is_some());
    h.ui.update(|window, app| {
        let dialog = h.dialog.read(app);
        assert!(dialog.name.read(app).is_focused(window));
    });
    h.ui.simulate_keystrokes("enter");
    assert!(h.commands.borrow().is_empty());

    for _ in 0..15 {
        h.ui.simulate_keystrokes("tab");
        h.ui.update(|window, app| {
            assert!(h.dialog.read(app).region.contains_focused(window, app));
        });
    }
    h.click("folder-dialog-name");
    h.ui.simulate_input(&"а".repeat(65));
    h.ui.run_until_parked();
    assert!(h.ui.debug_bounds("folder-dialog-notice").is_some());
    h.ui.simulate_keystrokes("enter");
    assert!(h.commands.borrow().is_empty());

    h.ui.simulate_keystrokes("cmd-a");
    h.ui.simulate_input("  Проекты  ");
    h.click("folder-dialog-color-3");
    h.click("folder-dialog-name");
    h.ui.simulate_keystrokes("enter");
    h.ui.run_until_parked();
    h.ui.simulate_keystrokes("escape tab enter");
    h.click("folder-launcher");
    h.ui.read(|app| assert_eq!(h.root.read(app).clicks, 1));
    assert_eq!(h.commands.borrow().len(), 1);

    h.finish(Err(FolderCommandError::Unknown));
    assert!(h.ui.debug_bounds("folder-dialog-notice").is_some());
    h.click("folder-dialog-color-7");
    h.click("folder-dialog-submit");
    h.finish(Ok(()));
    let calls = h.commands.borrow();
    let Recorded::Create(id, title, color) = &calls[0] else {
        panic!("ожидалось создание")
    };
    assert!(!id.is_nil());
    assert_eq!(title, "Проекты");
    assert_eq!(*color, Some(COLORS[3].0));
    assert_eq!(&calls[1], &calls[0]);
    drop(calls);
    h.restored();
}

/// Сохраняет только изменённое поле и удаляет папку по отдельному подтверждению.
#[gpui::test]
fn edits_only_changed_fields_and_requires_delete_confirmation(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    let id = Uuid::from_u128(21);
    h.ui.update(|window, app| {
        h.dialog.update(app, |dialog, cx| {
            dialog.open_edit(id, "Проекты".into(), Some(COLORS[0].0), window, cx);
        });
    });
    h.ui.run_until_parked();
    h.ui.simulate_keystrokes("enter");
    assert!(h.commands.borrow().is_empty());
    h.click("folder-dialog-color-2");
    h.click("folder-dialog-submit");
    assert_eq!(
        *h.commands.borrow(),
        [Recorded::Update(id, None, Some(Some(COLORS[2].0)))]
    );
    h.finish(Err(FolderCommandError::Unknown));
    h.click("folder-dialog-color-7");
    h.click("folder-dialog-submit");
    assert_eq!(h.commands.borrow().len(), 1);
    h.ui.read(|app| {
        let dialog = h.dialog.read(app);
        assert_eq!(dialog.name.read(app).text(), "Проекты");
        assert_eq!(dialog.open.as_ref().unwrap().color, Some(COLORS[2].0));
    });
    h.click("folder-dialog-cancel");
    h.restored();

    h.open_delete(id);
    h.ui.update(|window, app| assert!(h.dialog.read(app).cancel.is_focused(window)));
    h.ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
    });
    h.ui.run_until_parked();
    h.restored();
    assert_eq!(h.commands.borrow().len(), 1);

    h.open_delete(id);
    h.click("folder-dialog-submit");
    h.finish(Err(FolderCommandError::Unknown));
    h.click("folder-dialog-submit");
    assert_eq!(h.commands.borrow().len(), 2);
    h.click("folder-dialog-cancel");
    h.restored();

    h.open_delete(id);
    h.click("folder-dialog-submit");
    assert_eq!(h.commands.borrow().last(), Some(&Recorded::Delete(id)));
    h.finish(Err(FolderCommandError::Refresh));
    assert!(h.ui.debug_bounds("folder-dialog-notice").is_some());
    h.ui.simulate_keystrokes("escape");
    h.ui.run_until_parked();
    h.ui.update(|window, app| {
        assert!(!h.dialog.read(app).is_open());
        assert!(h.root.read(app).all_chats.is_focused(window));
    });

    h.open_delete(id);
    h.click("folder-dialog-submit");
    assert_eq!(h.commands.borrow().last(), Some(&Recorded::Delete(id)));
    h.ui.update(|_, app| h.dialog.update(app, FolderDialog::reset));
    h.finish(Err(FolderCommandError::Rejected));
    h.ui.update(|window, app| {
        assert!(!h.dialog.read(app).is_open());
        assert!(!h.dialog.read(app).region.contains_focused(window, app));
    });
}

/// Подключает форму и сайдбар в одном корне, как в рабочем окне.
struct SidebarRoot {
    sidebar: Entity<Sidebar>, // Каталог и владелец команд папок.
}

impl Render for SidebarRoot {
    /// Всегда рисует форму отдельным слоем, в том числе для возврата фокуса.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(self.sidebar.clone())
            .child(self.sidebar.read(cx).folder_dialog())
    }
}

/// Нажатие плюса и Enter отправляет одну команду для исходного входа.
#[gpui::test]
fn plus_opens_folder_dialog_and_sends_one_create_for_the_current_entry(cx: &mut TestAppContext) {
    cx.update(Sidebar::bind_keys);
    let (root, ui) = cx.add_window_view(|_, cx| SidebarRoot {
        sidebar: cx.new(Sidebar::new),
    });
    let sidebar = ui.read(|app| root.read(app).sidebar.clone());
    let session = AccountSession {
        owner: OwnerContext {
            account_id: AccountId("folder-test".into()),
            server: "https://workspace.test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        },
        generation: SessionGeneration(7),
    };
    let commands = Rc::new(RefCell::new(Vec::new()));
    let received = commands.clone();
    ui.update(|_, app| {
        app.subscribe(&sidebar, move |_, intent: &FolderIntent, _| {
            received.borrow_mut().push(intent.clone());
        })
        .detach();
        sidebar.update(app, |sidebar, cx| {
            sidebar.update_snapshot(Some(&session), None, None, cx);
            sidebar.set_folder_online(true, cx);
        });
    });
    ui.run_until_parked();

    click(ui, "folder-rail-create");
    assert!(ui.debug_bounds("folder-dialog").is_some());
    ui.simulate_input("Рабочие");
    ui.simulate_keystrokes("enter enter");
    ui.run_until_parked();
    let operation = {
        let issued = commands.borrow();
        let [
            FolderIntent::Command {
                session: sent,
                operation,
                command: FolderCommand::Create { id, title, color },
            },
        ] = issued.as_slice()
        else {
            panic!("ожидалась одна команда создания");
        };
        assert_eq!(sent, &session);
        assert!(!id.is_nil());
        assert_eq!(title, "Рабочие");
        assert_eq!(*color, Some(COLORS[0].0));
        *operation
    };
    let stale = AccountSession {
        generation: SessionGeneration(6),
        ..session.clone()
    };
    ui.update(|_, app| {
        sidebar.update(app, |sidebar, cx| {
            sidebar.finish_folder_command(&stale, operation, Ok(()), cx);
        })
    });
    ui.run_until_parked();
    assert!(ui.debug_bounds("folder-dialog").is_some());
    ui.update(|_, app| {
        sidebar.update(app, |sidebar, cx| {
            sidebar.finish_folder_command(&session, operation, Ok(()), cx);
        })
    });
    ui.run_until_parked();
    ui.read(|app| assert!(!sidebar.read(app).folder_dialog().read(app).is_open()));
}
