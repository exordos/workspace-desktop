//! Общие виджеты Iced, их тема и размеры.
//! Вызывающий код передаёт состояние и обрабатывает события виджетов.

mod button;
mod icon;
mod text_input;
mod theme;

pub use button::{ButtonAppearance, ButtonOptions, ButtonSize, ButtonVariant, button, button_with};
pub use icon::{IconName, IconTone, icon};
pub use text_input::text_input;
pub use theme::{ThemeMode, workspace_theme};

const CONTROL_RADIUS: f32 = 8.0;
const CONTROL_TEXT_SIZE: u32 = 14;
