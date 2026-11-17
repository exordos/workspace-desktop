//! Общие примитивы клиента GPUI. Здесь находятся тема, иконки, элементы управления
//! и представление чатов. Компоненты получают данные и действия от экрана.

mod content;
mod controls;
mod icons;
mod theme;

#[cfg(all(test, feature = "gpui-test"))]
mod tests;

pub use content::{
    Avatar, AvatarPresence, AvatarSize, BadgeTone, ChatRow, ChatRowPreview, DeliveryMark,
    MessageBubble, mention_badge, unread_badge,
};
pub use controls::{Button, ButtonAppearance, FieldAppearance, field};
pub use icons::{Icon, SEARCH_ICON_SIZE, WorkspaceAssets};
pub use theme::{Palette, TextRole, ThemeMode, label};
