//! Общие виджеты Iced, их тема и размеры.
//! Вызывающий код передаёт состояние и обрабатывает события виджетов.

mod avatar;
mod badge;
mod button;
mod field_size;
mod form_field;
mod icon;
mod input_selector;
mod listbox;
mod member_row;
mod message_bubble;
mod message_composer;
mod panel_menu_row;
mod presence;
mod search_input;
mod select;
mod selectable_row;
mod selection;
mod switch;
mod text_input;
mod theme;

pub use avatar::{AvatarContent, AvatarDecoration, AvatarPhoto, AvatarSize, avatar, avatar_group};
pub use badge::{BadgeSize, BadgeTone, mention_badge, unread_count_badge};
pub use button::{ButtonAppearance, ButtonOptions, ButtonSize, ButtonVariant, button, button_with};
pub use field_size::FieldSize;
pub use form_field::form_field;
pub use icon::{IconName, IconTone, icon};
pub use input_selector::{InputSelectorOptions, input_selector};
pub use listbox::{ListboxOption, listbox};
pub use member_row::{member_row, member_row_content};
pub use message_bubble::{MessageDelivery, MessageDirection, message_bubble};
pub use message_composer::{MessageComposerOptions, message_composer};
pub use panel_menu_row::{PanelMenuRowOptions, panel_menu_row};
pub use presence::{PresenceSize, PresenceStatus, presence_indicator};
pub use search_input::search_input;
pub use select::select;
pub use selectable_row::selectable_row;
pub use selection::{SelectionSize, selection_checkbox, selection_indicator};
pub use switch::switch;
pub use text_input::{text_input, text_input_with_error};
pub use theme::{ThemeMode, workspace_theme};

const CONTROL_RADIUS: f32 = 8.0;
const CONTROL_TEXT_SIZE: u32 = 14;
