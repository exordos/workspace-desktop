//! Рисует свёрнутый ряд и раскрытый список «Моей активности» над чатами.

use gpui::{Context, Div, Stateful, div, prelude::*, px, rgb};

use super::{AccountSession, AppPreferences, ChatDensity, Sidebar, SidebarAction};
use crate::gpui_client::{
    contracts::ActivityPage,
    preferences::PreferenceValues,
    ui::{
        BadgeTone, Icon, PANEL_RADIUS, Palette, TextRole, activate, label, popup::tooltip,
        unread_badge,
    },
};

const PAGES: [ActivityPage; 5] = [
    ActivityPage::Inbox,
    ActivityPage::Favorites,
    ActivityPage::Marked,
    ActivityPage::Mentions,
    ActivityPage::Drafts,
];

/// Сведения активного входа для пунктов над каталогом.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::gpui_client) struct ActivityNavigation {
    pub page: Option<ActivityPage>, // Текущая страница для подсветки выбранного пункта.
    pub inbox_count: Option<u32>,   // Активные непрочитанные «Все чаты», если снимок получен.
    pub mention_count: Option<u64>, // Непрочитанные упоминания, если счётчик получен.
}

impl Sidebar {
    /// Обновляет подсветку и счётчики либо убирает блок для недоступного входа.
    pub(in crate::gpui_client) fn set_activity(
        &mut self,
        activity: Option<ActivityNavigation>,
        cx: &mut Context<Self>,
    ) {
        if self.activity != activity {
            self.activity = activity;
            cx.notify();
        }
    }

    /// Сохраняет раскрытие в настройках того входа, который всё ещё показан.
    fn toggle_activity(&self, session: &AccountSession, cx: &mut Context<Self>) {
        if self.activity.is_none()
            || self.active.as_ref().map(|active| &active.session) != Some(session)
            || !AppPreferences::is_selected(cx, &session.owner)
        {
            return;
        }
        let result = AppPreferences::update(cx, |values| {
            values.activity_expanded = !values.activity_expanded;
        });
        if let Err(error) = result {
            log::warn!(
                "event=activity_preference_save outcome=failed reason={:?}",
                error.kind()
            );
        } else {
            cx.notify();
        }
    }
}

/// Показывает те же пять переходов в обеих плотностях и состояниях раскрытия.
pub(super) fn render(
    navigation: &ActivityNavigation,
    session: &AccountSession,
    settings: &PreferenceValues,
    palette: Palette,
    cx: &Context<Sidebar>,
) -> Stateful<Div> {
    let expanded = settings.activity_expanded;
    let title = settings.language.tr("nav.activity");
    let chevron = if expanded {
        Icon::ChevronUp
    } else {
        Icon::ChevronDown
    };
    let session_for_toggle = session.clone();
    let sidebar = cx.entity().downgrade();
    let toggle = activate(
        div()
            .id("activity-toggle")
            .debug_selector(|| "activity-toggle".into())
            .flex()
            .items_center()
            .justify_center()
            .h(px(32.))
            .rounded(PANEL_RADIUS)
            .border_1()
            .border_color(gpui::transparent_black())
            .hover(|style| style.bg(palette.card_hover))
            .map(|toggle| {
                if expanded {
                    toggle.w_full().px_1().justify_between().child(label(
                        palette,
                        TextRole::Heading,
                        title,
                    ))
                } else {
                    toggle.w(px(32.)).flex_none().bg(palette.card)
                }
            })
            .child(chevron.element(px(16.), palette.text_muted)),
        None,
        palette,
        move |_, app| {
            let _ = sidebar.update(app, |sidebar, cx| {
                sidebar.toggle_activity(&session_for_toggle, cx)
            });
        },
    )
    .tooltip(move |_, app| tooltip(palette, title.into(), app));

    let items = PAGES
        .into_iter()
        .map(|page| item(page, navigation, session, settings, palette, cx));
    let rail = div()
        .id("activity-rail")
        .debug_selector(|| "activity-rail".into())
        .flex()
        .min_w_0()
        .flex_none()
        .pb_2();
    if expanded {
        rail.flex_col().gap_1().child(toggle).children(items)
    } else {
        rail.gap_2().children(items).child(toggle)
    }
}

/// Рисует кнопку раздела и передаёт оболочке адрес входа, из которого она нажата.
fn item(
    page: ActivityPage,
    navigation: &ActivityNavigation,
    session: &AccountSession,
    settings: &PreferenceValues,
    palette: Palette,
    cx: &Context<Sidebar>,
) -> Stateful<Div> {
    let expanded = settings.activity_expanded;
    let compact_density = settings.appearance.density == ChatDensity::Compact;
    let title = page.title(settings.language);
    let selector = page.id();
    let (expanded_icon, compact_icon, color) = match page {
        ActivityPage::Inbox => (Icon::ActivityInbox, Icon::ActivityInboxCompact, 0x8d6dff),
        ActivityPage::Favorites => (Icon::HomeFilled, Icon::Home, 0x4b98e6),
        ActivityPage::Marked => (Icon::ActivityMarked, Icon::ActivityMarkedCompact, 0xf04c4c),
        ActivityPage::Mentions => (Icon::ActivityMentions, Icon::ActivityMentions, 0xffd633),
        ActivityPage::Drafts => (Icon::ActivityDrafts, Icon::ActivityDraftsCompact, 0xf458d2),
    };
    let count = match page {
        ActivityPage::Inbox => navigation.inbox_count.map(u64::from),
        ActivityPage::Mentions => navigation.mention_count,
        _ => None,
    };
    let badge = count.and_then(|count| unread_badge(palette, count, BadgeTone::Accent));
    let button = div()
        .id(selector)
        .debug_selector(move || selector.into())
        .relative()
        .flex()
        .min_w_0()
        .items_center()
        .rounded(PANEL_RADIUS)
        .border_1()
        .border_color(gpui::transparent_black())
        .bg(if navigation.page == Some(page) {
            palette.card_selected
        } else {
            palette.card
        });
    let button = if expanded {
        let chip = if compact_density { 28. } else { 30. };
        button
            .w_full()
            .gap_2()
            .px_2()
            .py(if compact_density { px(5.) } else { px(7.) })
            .child(
                div()
                    .flex()
                    .flex_none()
                    .size(px(chip))
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(rgb(color))
                    .child(expanded_icon.element(px(24.), gpui::white())),
            )
            .child(
                label(palette, TextRole::Label, title)
                    .min_w_0()
                    .flex_1()
                    .truncate(),
            )
            .children(badge)
    } else {
        let size = if matches!(page, ActivityPage::Inbox | ActivityPage::Mentions) {
            20.
        } else {
            18.
        };
        button
            .h(px(32.))
            .flex_1()
            .justify_center()
            .child(compact_icon.element(px(size), palette.text))
            .children(badge.map(|badge| {
                badge
                    .absolute()
                    .right_0()
                    .top_0()
                    .h(px(16.))
                    .min_w(px(16.))
                    .px_1()
            }))
    };
    let session = session.clone();
    let sidebar = cx.entity().downgrade();
    activate(
        button.hover(|style| style.bg(palette.card_hover)),
        None,
        palette,
        move |_, app| {
            let _ = sidebar.update(app, |sidebar, cx| {
                if sidebar.active.as_ref().map(|active| &active.session) == Some(&session) {
                    cx.emit(SidebarAction::OpenActivity {
                        page,
                        session: session.clone(),
                    });
                }
            });
        },
    )
    .tooltip(move |_, app| tooltip(palette, title.into(), app))
}
