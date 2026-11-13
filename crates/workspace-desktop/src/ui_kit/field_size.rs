//! Задаёт общие размеры однострочных полей и селекторов.

/// Определяет высоту поля и отступы вокруг текста.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSize {
    Compact, // 32 логических пикселя для боковых панелей и компактных форм.
    Regular, // 40 логических пикселей для основных полей.
}

impl FieldSize {
    /// Возвращает высоту поля в логических пикселях.
    pub const fn height(self) -> u16 {
        match self {
            Self::Compact => 32,
            Self::Regular => 40,
        }
    }

    /// Возвращает горизонтальный отступ от края поля до текста.
    pub const fn horizontal_padding(self) -> u16 {
        match self {
            Self::Compact => 8,
            Self::Regular => 12,
        }
    }

    /// Распределяет оставшееся место поровну над и под строкой текста.
    pub(super) fn vertical_padding(self) -> u16 {
        (self.height() - Self::LINE_HEIGHT) / 2
    }

    pub(super) const LINE_HEIGHT: u16 = 20;
}
