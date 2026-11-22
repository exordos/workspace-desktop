//! Общие примитивы клиента GPUI. Здесь находятся тема, иконки, элементы управления
//! и представление чатов. Компоненты получают данные и действия от экрана.

mod content;
mod controls;
mod focus;
mod icons;
pub(super) mod popup;
mod text_input;
mod theme;
mod top_bar;

#[cfg(all(test, feature = "gpui-test"))]
mod tests;

pub use content::{
    Avatar, AvatarPresence, AvatarSize, BadgeTone, ChatRow, ChatRowPreview, DeliveryMark,
    MessageBubble, mention_badge, unread_badge,
};
pub(in crate::gpui_client) use controls::activate;
pub use controls::{Button, ButtonAppearance, FieldAppearance, field};
pub use focus::track_focus_input;
pub use icons::{Icon, SEARCH_ICON_SIZE, WorkspaceAssets};
pub(crate) use text_input::{TextInput, TextInputChanged, TextInputSubmitted};
pub(in crate::gpui_client) use theme::ellipsized_label;
pub(super) use theme::{PANEL_RADIUS, SURFACE_RADIUS};
pub use theme::{Palette, TextRole, ThemeMode, label};
pub use top_bar::{TopBar, TopBarAccount, TopBarAction, TopBarSection, TopBarSelection};
