//! Shared Iced widgets. Callers own application state and handle widget messages.

mod button;
mod text_input;
mod theme;

pub use button::{ButtonAppearance, ButtonOptions, ButtonSize, ButtonVariant, button, button_with};
pub use text_input::text_input;
pub use theme::{ThemeMode, workspace_theme};

const CONTROL_RADIUS: f32 = 8.0;
const CONTROL_TEXT_SIZE: u32 = 14;
