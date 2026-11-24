//! Рисует настройки защищённого чата и передаёт изменение политики экрану разговора.

use gpui::{Context, Div, Window, div, prelude::*, px, rgb};
use workspace_core::messenger::details::{ChannelSidebar, SidebarDetails};

use super::{Conversation, EncryptedScreen, Mutation, MutationKind, Panel, State, action};
use crate::gpui_client::{
    preferences::Language,
    ui::{Icon, Palette, TextRole, activate, label},
};

/// Показывает сведения о канале и действия, доступные из его настроек.
pub(super) fn render(
    panel: &Panel,
    channel: &ChannelSidebar,
    palette: Palette,
    language: Language,
    cx: &Context<Conversation>,
) -> Div {
    let text = |key| language.tr(key);
    let keys_focus = panel.encrypted_settings_keys.clone();
    let keys = activate(
        div()
            .id("conversation-encrypted-settings-keys")
            .debug_selector(|| "conversation-encrypted-settings-keys".into())
            .min_h(px(40.))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(gpui::transparent_black())
            .hover(|style| style.bg(palette.card_hover))
            .child(Icon::ChatLock.element(px(24.), palette.icon))
            .child(label(palette, TextRole::Body, text("e2ee.keyTitle")).flex_1())
            .child(Icon::ChevronRight.element(px(20.), palette.text_muted)),
        Some(&panel.encrypted_settings_keys),
        palette,
        action(cx, move |view, window, cx| {
            view.open_encrypted_screen(EncryptedScreen::Keys, keys_focus.clone(), window, cx);
        }),
    );

    div()
        .debug_selector(|| "conversation-encrypted-settings-screen".into())
        .flex()
        .flex_col()
        .gap(px(20.))
        .child(
            div()
                .flex()
                .flex_col()
                .px_2()
                .gap_3()
                .child(field(palette, text("channel.channelName"), &channel.name))
                .child(field(
                    palette,
                    text("chatInfo.description"),
                    channel
                        .description
                        .as_deref()
                        .map(str::trim)
                        .unwrap_or_else(|| text("channel.noDescription")),
                )),
        )
        .child(
            label(
                palette,
                TextRole::Caption,
                text("e2ee.channelSettingsReadOnly"),
            )
            .px_2()
            .text_color(palette.text_muted),
        )
        .child(sharing_row(panel, channel, palette, language, cx))
        .when(
            panel.mutation_error == Some(MutationKind::KeySharing),
            |body| {
                body.child(
                    label(
                        palette,
                        TextRole::Caption,
                        text("channel.keySharingUpdateFailed"),
                    )
                    .px_2()
                    .text_color(palette.danger)
                    .debug_selector(|| "conversation-encrypted-settings-key-sharing-error".into()),
                )
            },
        )
        .child(keys)
        .child(metadata_row(
            palette,
            text("channel.type"),
            if channel.is_private {
                text("channel.typeClosedProtected")
            } else {
                text("channel.typeOpen")
            },
        ))
        .child(metadata_row(
            palette,
            text("channel.members"),
            &channel.members.len().to_string(),
        ))
}

/// Рисует подпись и значение поля, которое нельзя редактировать.
fn field(palette: Palette, title: &'static str, value: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(label(palette, TextRole::Body, title).text_color(palette.text_muted))
        .child(label(palette, TextRole::Body, value.to_owned()))
}

/// Располагает название свойства слева, а текущее серверное значение справа.
fn metadata_row(palette: Palette, title: &'static str, value: &str) -> Div {
    div()
        .flex()
        .justify_between()
        .items_start()
        .gap_2()
        .px_2()
        .child(label(palette, TextRole::Body, title).flex_none())
        .child(
            label(palette, TextRole::Body, value.to_owned())
                .min_w_0()
                .text_right()
                .text_color(palette.text_muted),
        )
}

/// Показывает подтверждённое значение и принимает ввод только в готовом снимке владельца.
fn sharing_row(
    panel: &Panel,
    channel: &ChannelSidebar,
    palette: Palette,
    language: Language,
    cx: &Context<Conversation>,
) -> gpui::Stateful<Div> {
    let checked = channel.allow_key_sharing_by_holders == Some(true);
    let editable = channel.can_edit_key_sharing
        && panel.mutation.is_none()
        && matches!(panel.state, Some(State::Ready));
    let row = div()
        .id("conversation-encrypted-settings-key-sharing")
        .debug_selector(|| "conversation-encrypted-settings-key-sharing".into())
        .min_h(px(40.))
        .px_2()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(gpui::transparent_black())
        .child(label(
            palette,
            TextRole::Body,
            language.tr("channel.allowMembersToShareKeys"),
        ))
        .child(
            div()
                .w(px(36.))
                .h(px(20.))
                .p(px(2.))
                .rounded_full()
                .flex_none()
                .flex()
                .justify_between()
                .bg(if checked {
                    palette.accent
                } else {
                    palette.border
                })
                .when(checked, |element| element.child(div()))
                .child(div().size(px(16.)).rounded_full().bg(rgb(0xffffff))),
        );
    if editable {
        activate(
            row.hover(|style| style.bg(palette.card_hover)),
            Some(&panel.encrypted_settings_sharing),
            palette,
            action(cx, Conversation::toggle_encrypted_key_sharing),
        )
    } else {
        row.opacity(0.55)
    }
}

impl Conversation {
    /// Использует последнее подтверждённое значение, если активная панель ещё доступна.
    fn toggle_encrypted_key_sharing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = &self.details else {
            return;
        };
        if !self.has_access()
            || panel.encrypted_screen() != Some(EncryptedScreen::Settings)
            || !panel.host.read(cx).is_active(&panel.key)
        {
            return;
        }
        let Some(SidebarDetails::Channel(channel)) = panel.snapshot.as_deref() else {
            return;
        };
        if channel.can_edit_key_sharing
            && let Some(allow) = channel.allow_key_sharing_by_holders
        {
            self.start_sidebar_mutation(Mutation::KeySharing(!allow), window, cx);
        }
    }
}
