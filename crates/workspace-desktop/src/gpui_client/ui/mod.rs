//! Общие примитивы клиента GPUI. Здесь находятся тема, иконки, элементы управления
//! и представление чатов. Компоненты получают данные и действия от экрана.

mod content;
mod controls;
mod conversation_notice;
mod dialog_card;
mod filter_group;
mod focus;
mod icons;
mod person_row;
pub(super) mod popup;
mod quote;
mod text_input;
mod theme;
mod top_bar;

#[cfg(all(test, feature = "gpui-test"))]
mod tests;

pub use content::{
    Avatar, AvatarPresence, AvatarSize, BadgeTone, ChatRow, ChatRowPreview, DeliveryMark,
    MessageBubble, mention_badge, unread_badge,
};
pub(in crate::gpui_client) use content::profile_joined_date;
pub use controls::{Button, ButtonAppearance, FieldAppearance, field};
pub(in crate::gpui_client) use controls::{activate, rounded_surface_corners};
pub use conversation_notice::ConversationNotice;
pub use dialog_card::dialog_card;
pub use filter_group::{FilterGroup, filter_button};
pub use focus::track_focus_input;
pub use icons::{Icon, SEARCH_ICON_SIZE, WorkspaceAssets};
pub use person_row::PersonRow;
pub use quote::{QuoteSurface, quote_frame, quote_preview};
pub(crate) use text_input::{TextInput, TextInputChanged, TextInputSubmitted};
pub(super) use theme::{CONTROL_BORDER, PANEL_RADIUS, SURFACE_RADIUS};
pub use theme::{Palette, TextRole, ThemeMode, label};
pub(in crate::gpui_client) use theme::{ellipsized_label, text_role};
pub use top_bar::{TopBar, TopBarAccount, TopBarAction, TopBarSection, TopBarSelection};
