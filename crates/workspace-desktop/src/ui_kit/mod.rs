//! Общие виджеты Iced, их тема и размеры.
//! Вызывающий код передаёт состояние и обрабатывает события виджетов.

mod badge;
mod button;
mod icon;
mod presence;
mod selection;
mod switch;
mod text_input;
mod theme;

pub use badge::{BadgeSize, BadgeTone, mention_badge, unread_count_badge};
pub use button::{ButtonAppearance, ButtonOptions, ButtonSize, ButtonVariant, button, button_with};
pub use icon::{IconName, IconTone, icon};
pub use presence::{PresenceSize, PresenceStatus, presence_indicator};
pub use selection::{SelectionSize, selection_checkbox, selection_indicator};
pub use switch::switch;
pub use text_input::text_input;
pub use theme::{ThemeMode, workspace_theme};

const CONTROL_RADIUS: f32 = 8.0;
const CONTROL_TEXT_SIZE: u32 = 14;
