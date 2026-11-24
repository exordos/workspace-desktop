//! Общие примитивы клиента GPUI. Здесь находятся тема, иконки, элементы управления
//! и представление чатов. Компоненты получают данные и действия от экрана.

pub mod animation;
mod content;
mod controls;
mod conversation_notice;
mod date_time;
mod dialog_card;
mod filter_group;
mod focus;
mod icons;
mod person_row;
pub mod poll;
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
pub(in crate::gpui_client) use content::{calendar_date_label, profile_joined_date};
pub use controls::{Button, ButtonAppearance, FieldAppearance, field};
pub(in crate::gpui_client) use controls::{
    activate, rounded_surface_corners, sidebar_item, switch_indicator,
};
pub use conversation_notice::ConversationNotice;
pub use date_time::{
    DatePicker, DatePickerChanged, DateTimePicker, DateTimePickerChanged, PickerMode,
    PickerOptions, TimePicker, TimePickerChanged, TimePrecision,
};
pub use dialog_card::dialog_card;
pub use filter_group::{FilterGroup, filter_button};
pub use focus::track_focus_input;
pub use icons::{Icon, SEARCH_ICON_SIZE, WorkspaceAssets};
pub use person_row::PersonRow;
pub use quote::{QuoteSurface, quote_frame, quote_preview};
pub(crate) use text_input::{TextInput, TextInputChanged, TextInputSubmitted};
pub(super) use theme::{CONTROL_BORDER, MESSAGE_BUBBLE_MAX_WIDTH, PANEL_RADIUS, SURFACE_RADIUS};
pub use theme::{Palette, TextRole, ThemeMode, label};
pub(in crate::gpui_client) use theme::{ellipsized_label, text_role};
pub use top_bar::{TopBar, TopBarAccount, TopBarAction, TopBarSection, TopBarSelection};
