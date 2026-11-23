//! Подставляет синтетические данные в реальные экраны для отдельных нативных снимков.

use gpui::{AnyView, App, Context, Render, Window, div, prelude::*};
use tempfile::TempDir;
use uuid::Uuid;
use workspace_core::{
    context::{AccountId, OwnerContext, SessionGeneration},
    directory::{ProfilePresence, UserProfile},
    messenger::catalog::{Chat, ChatMetadata, NewChatCatalog},
};

use crate::gpui_client::{
    contracts::AccountSession,
    preferences::{Preferences, appearance::ThemeChoice},
    runtime::RuntimeClient,
    ui::{Palette, ThemeMode},
};

use super::{ChannelDialog, Filter, Load, NewChats};

/// Фиксированные состояния режима создания для сравнения с макетами.
#[derive(Clone, Copy)]
pub enum NewChatPreview {
    Catalog,       // Открыты активные каналы и лички.
    Unsubscribed,  // Вкладка без подписки показывает подсказку «В работе».
    People,        // Открыт выбор собеседника.
    CreateChannel, // Заполнено название нового канала.
    Members,       // После создания выбраны два участника.
}

impl NewChatPreview {
    /// Возвращает стабильный ключ команды захвата.
    pub fn id(self) -> &'static str {
        match self {
            Self::Catalog => "new-chats-panel",
            Self::Unsubscribed => "new-chats-unsubscribed",
            Self::People => "new-chat-people",
            Self::CreateChannel => "new-channel-form",
            Self::Members => "new-channel-members",
        }
    }

    /// Показывает пояснение к захватываемому состоянию.
    pub fn description(self) -> &'static str {
        match self {
            Self::Catalog => "Сайдбар «Новые чаты и каналы», вкладка «Все».",
            Self::Unsubscribed => "Подсказка «В работе» под фильтрами каналов.",
            Self::People => "Сайдбар выбора собеседника из справочника.",
            Self::CreateChannel => "Форма создания общего канала.",
            Self::Members => "Форма добавления участников после создания канала.",
        }
    }

    /// Возвращает ширину исходной области в логических пикселях.
    pub fn width(self) -> f32 {
        match self {
            Self::Catalog | Self::Unsubscribed | Self::People => 299.,
            Self::CreateChannel | Self::Members => 351.,
        }
    }

    /// Помечает состояния, которые занимают высоту сайдбара.
    pub fn is_sidebar(self) -> bool {
        matches!(self, Self::Catalog | Self::Unsubscribed | Self::People)
    }
}

/// Сохраняет временную папку модалки до закрытия окна захвата.
struct Frame {
    body: AnyView,             // Реальный экран с подставленными данными.
    _profile: Option<TempDir>, // None для сайдбара, которому не нужен Runtime.
}

impl Render for Frame {
    /// Передаёт область реальному экрану без копирования его компоновки.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .when(self._profile.is_none(), |panel| {
                panel.bg(Palette::for_window(window, cx).surface)
            })
            .child(self.body.clone())
    }
}

/// Открывает экран с фикстурой без изменения обычного профиля приложения.
pub fn new_chat_preview(kind: NewChatPreview, theme: ThemeMode, app: &mut App) -> AnyView {
    Preferences::update(app, |settings| {
        settings.appearance.theme = match theme {
            ThemeMode::Light => ThemeChoice::Light,
            ThemeMode::Dark => ThemeChoice::Dark,
        };
    })
    .expect("настройки сцены хранятся в памяти");
    let sidebar = app.new(|cx| {
        let mut sidebar = NewChats::new(None, cx);
        sidebar.open = true;
        if matches!(kind, NewChatPreview::Unsubscribed) {
            sidebar.filter = Filter::Unsubscribed;
        }
        sidebar.choosing = matches!(kind, NewChatPreview::People);
        sidebar.catalog = Load::Ready(NewChatCatalog {
            active: vec![
                chat(1, "Дизайн Workspace", false, false),
                chat(2, "Анна Котова", true, false),
                chat(3, "Документация", false, false),
                chat(4, "Проектная команда", false, true),
            ],
            archived: Vec::new(),
        });
        sidebar.people.data = Load::Ready(people());
        sidebar
    });
    let (body, profile) = if kind.is_sidebar() {
        (sidebar.into(), None)
    } else {
        let profile = tempfile::tempdir().expect("изолированный профиль сцены");
        let (runtime, _) =
            RuntimeClient::start_in(profile.path().to_owned()).expect("сцена без входа");
        let origin = sidebar.downgrade();
        let dialog = app.new(|cx| {
            let region = cx.focus_handle();
            let mut dialog = ChannelDialog::new(runtime, session(), 0, origin, region, cx);
            dialog.preview_content(matches!(kind, NewChatPreview::Members), people(), cx);
            dialog
        });
        (dialog.into(), Some(profile))
    };
    app.new(|_| Frame {
        body,
        _profile: profile,
    })
    .into()
}

/// Создаёт синтетическое описание одного разговора.
fn chat(id: u128, name: &str, direct: bool, invite_only: bool) -> Chat {
    Chat {
        id: Uuid::from_u128(id),
        name: name.into(),
        direct,
        topics: Vec::new(),
        metadata: ChatMetadata {
            invite_only,
            ..Default::default()
        },
    }
}

/// Даёт обеим формам одинаковые стабильные карточки пользователей.
fn people() -> Vec<(Uuid, UserProfile)> {
    [
        ("Анна Котова", "Дизайнер"),
        ("Михаил Петров", "Инженер"),
        ("Александра Смирнова", "В сети"),
        ("Мария Васильева", "Редактор"),
        ("Ирина Мельникова", "Отошла"),
        ("Дмитрий Орлов", "Разработчик"),
        ("Игорь Макаров", "Аналитик"),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (name, status))| {
        (
            Uuid::from_u128(100 + index as u128),
            UserProfile {
                display_name: name.into(),
                email: None,
                presence: (index == 2).then_some(ProfilePresence::Active),
                status_text: Some(status.into()),
                status_emoji: None,
                avatar: None,
                created_at: None,
            },
        )
    })
    .collect()
}

/// Делает токен контекста синтетической формы без сохранённого входа.
fn session() -> AccountSession {
    AccountSession {
        owner: OwnerContext {
            account_id: AccountId("fixture".into()),
            server: "https://workspace.test".into(),
            organization_id: "fixture".into(),
            project_id: "fixture".into(),
            user_id: "fixture".into(),
        },
        generation: SessionGeneration(1),
    }
}
