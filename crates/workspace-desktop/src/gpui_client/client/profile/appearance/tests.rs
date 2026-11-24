//! Проверяет настройку анимаций, темы, папок и фона из профиля рабочего окна.

use std::{collections::HashMap, sync::Arc};

use gpui::{Modifiers, TestAppContext, VisualTestContext, px, rgb, size};
use uuid::Uuid;
use workspace_core::{
    auth::SessionStatus,
    bootstrap::{ChatList, ChatListStatus},
    context::{AccountId, OwnerContext, SessionGeneration},
    messenger::catalog::{Chat, ChatMetadata},
};

use crate::gpui_client::{
    Options, bind_keys,
    client::Client,
    contracts::{AccountSnapshot, RuntimeEvent, RuntimeExit},
    preferences::appearance::{Background, BackgroundPreset},
    runtime::RuntimeClient,
};

use super::*;

/// Меняет внешний вид из рабочего окна и восстанавливает сохранённый результат.
#[gpui::test]
fn appearance_changes_live_window_and_persists_background(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let owner = OwnerContext {
        account_id: AccountId("appearance-test".into()),
        server: "https://workspace.test".into(),
        organization_id: "org".into(),
        project_id: "project".into(),
        user_id: Uuid::from_u128(1).to_string(),
    };
    let session = AccountSession {
        owner: owner.clone(),
        generation: SessionGeneration(1),
    };
    let first = Uuid::from_u128(10);
    let second = Uuid::from_u128(20);
    let chat = |id, name: &str, unread| Chat {
        id,
        name: name.into(),
        direct: true,
        topics: Vec::new(),
        metadata: ChatMetadata {
            active_unread_count: Some(unread),
            ..Default::default()
        },
    };
    cx.update(|app| {
        bind_keys(app);
        Preferences::install(directory.path(), app);
    });
    let (runtime, _events) = RuntimeClient::start_in(directory.path().into()).unwrap();
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.simulate_resize(size(px(1440.), px(900.)));
    ui.update(|_, app| {
        client.update(app, |view, cx| {
            view.handle_ready(Ok(()), cx);
            view.handle_runtime(
                RuntimeEvent::Accounts(vec![AccountSnapshot {
                    session: session.clone(),
                    status: SessionStatus::Active,
                    unread: None,
                }]),
                cx,
            );
            view.handle_runtime(
                RuntimeEvent::Catalog(HashMap::from([(
                    owner.clone(),
                    ChatList {
                        generation: session.generation,
                        status: ChatListStatus::Ready,
                        chats: Some(Arc::new([
                            chat(first, "Экран", 0),
                            chat(second, "Альбом", 2),
                        ])),
                    },
                )])),
                cx,
            );
        })
    });
    ui.run_until_parked();

    let click = |ui: &mut VisualTestContext, id: &'static str| {
        let bounds = ui
            .debug_bounds(id)
            .unwrap_or_else(|| panic!("Элемент не виден: {id}"));
        ui.simulate_click(bounds.center(), Modifiers::none());
        ui.run_until_parked();
    };
    click(ui, "workspace-profile-trigger");
    click(ui, "profile-appearance");
    let animations = "right-panel-appearance-animations";
    let animations_on = "right-panel-appearance-animations-on";
    let animations_off = "right-panel-appearance-animations-off";
    assert!(ui.debug_bounds(animations_on).is_some());
    click(ui, animations);
    assert!(ui.debug_bounds(animations_off).is_some());
    assert!(!ui.read(Preferences::animations_enabled));
    ui.simulate_keystrokes("enter");
    ui.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse("enter").unwrap(),
    });
    ui.run_until_parked();
    assert!(ui.read(Preferences::animations_enabled));
    click(ui, animations);
    assert!(!ui.read(Preferences::animations_enabled));

    click(ui, "right-panel-appearance-theme-trigger");
    let theme = ui
        .debug_bounds("right-panel-appearance-theme-options")
        .unwrap();
    assert_eq!(theme.size.height, px(138.));
    click(ui, "settings-theme-mode-dark");
    click(ui, "settings-theme-palette-emerald-chat");
    let accent = ui.update(|window, app| Palette::for_window(window, app).accent);
    assert_eq!(accent, rgb(0x2ecf72).into());
    click(ui, "right-panel-appearance-theme-trigger");

    let first_row = "catalog-chat-00000000-0000-0000-0000-00000000000a";
    let second_row = "catalog-chat-00000000-0000-0000-0000-000000000014";
    let standard = ui.debug_bounds(first_row).unwrap();
    assert!(standard.origin.y < ui.debug_bounds(second_row).unwrap().origin.y);
    click(ui, "right-panel-appearance-sorting-trigger");
    click(ui, "settings-unread-first");
    assert!(
        ui.debug_bounds(second_row).unwrap().origin.y
            < ui.debug_bounds(first_row).unwrap().origin.y
    );
    click(ui, "right-panel-appearance-sorting-trigger");
    click(ui, "right-panel-appearance-density-trigger");
    click(ui, "settings-compact");
    assert!(ui.debug_bounds(first_row).unwrap().size.height < standard.size.height);
    click(ui, "right-panel-appearance-density-trigger");

    click(ui, "right-panel-appearance-folder-trigger");
    click(ui, "settings-horizontal");
    assert_eq!(
        ui.read(|app| client
            .read(app)
            .sidebar
            .read(app)
            .folder_placement(&session)),
        Some(false)
    );
    click(ui, "right-panel-appearance-folder-trigger");

    click(ui, "right-panel-appearance-background-trigger");
    let mountains = ui.debug_bounds("settings-background-mountains").unwrap();
    assert!(
        mountains.size.height > px(90.),
        "Карточка сохраняет пропорцию иллюстрации"
    );
    click(ui, "settings-background-mountains");
    assert!(ui.debug_bounds("settings-background-dialog").is_some());
    click(ui, "settings-background-apply-chat");
    assert!(ui.debug_bounds("settings-background-dialog").is_some());
    assert_eq!(
        ui.read(|app| Preferences::read(app)
            .appearance
            .background
            .for_chat(None)
            .cloned()),
        None
    );
    click(ui, "settings-background-apply-all");
    assert!(!ui.read(|app| client.read(app).right_panel.read(app).has_modal()));

    ui.update(|_, app| {
        Preferences::install(directory.path(), app);
        Preferences::select(app, Some(&owner));
        assert!(!Preferences::animations_enabled(app));
        let appearance = &Preferences::read(app).appearance;
        assert_eq!(appearance.theme, ThemeChoice::Dark);
        assert_eq!(appearance.palette, ThemePalette::EmeraldChat);
        assert_eq!(
            appearance.background.for_chat(None),
            Some(&Background::Preset(BackgroundPreset::Mountains))
        );
    });
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
