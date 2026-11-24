//! Описывает рабочие сочетания и проверяет ввод, общие для диалога и клавиатуры окна.

use gpui::{Keystroke, Modifiers, actions};

use super::{Language, PreferenceValues};

actions!(workspace_hotkeys, [CancelRecording]);

/// Действие, которое можно назначить на собственное сочетание клавиш.
#[derive(Clone, Copy)]
pub(in crate::gpui_client) struct Shortcut {
    pub id: &'static str,              // Ключ этой команды в профиле.
    pub title: &'static str,           // Ключ подписи команды.
    pub binding: Option<&'static str>, // Стандартное сочетание в синтаксисе GPUI.
}

/// Пункты в порядке Electron, разделённые на три группы.
pub(in crate::gpui_client) const SHORTCUT_GROUPS: [&[Shortcut]; 3] = [
    &[
        Shortcut {
            id: "navigation.back",
            title: "hotkeys.actions.navigation.back",
            binding: Some("alt-left"),
        },
        Shortcut {
            id: "navigation.forward",
            title: "hotkeys.actions.navigation.forward",
            binding: Some("alt-right"),
        },
        Shortcut {
            id: "navigation.messenger",
            title: "hotkeys.actions.navigation.messenger",
            binding: None,
        },
        Shortcut {
            id: "navigation.calendar",
            title: "hotkeys.actions.navigation.calendar",
            binding: None,
        },
        Shortcut {
            id: "navigation.mail",
            title: "hotkeys.actions.navigation.mail",
            binding: None,
        },
        Shortcut {
            id: "navigation.calls",
            title: "hotkeys.actions.navigation.calls",
            binding: None,
        },
        Shortcut {
            id: "navigation.activity",
            title: "hotkeys.actions.navigation.activity",
            binding: None,
        },
        Shortcut {
            id: "navigation.chatSearch",
            title: "hotkeys.actions.navigation.chatSearch",
            binding: Some("cmd-k"),
        },
        Shortcut {
            id: "navigation.folderList",
            title: "hotkeys.actions.navigation.folderList",
            binding: None,
        },
    ],
    &[
        Shortcut {
            id: "interface.theme",
            title: "hotkeys.actions.interface.theme",
            binding: None,
        },
        Shortcut {
            id: "interface.help",
            title: "hotkeys.actions.interface.help",
            binding: Some("cmd-/"),
        },
        Shortcut {
            id: "interface.sidebar",
            title: "hotkeys.actions.interface.sidebar",
            binding: Some("cmd-\\"),
        },
        Shortcut {
            id: "interface.infoPanel",
            title: "hotkeys.actions.interface.infoPanel",
            binding: Some("cmd-."),
        },
        Shortcut {
            id: "interface.quoteMessage",
            title: "hotkeys.actions.interface.quoteMessage",
            binding: Some("ctrl-l"),
        },
    ],
    &[
        Shortcut {
            id: "composer.editLastMessage",
            title: "hotkeys.actions.composer.editLastMessage",
            binding: Some("up"),
        },
        Shortcut {
            id: "composer.bold",
            title: "hotkeys.actions.composer.bold",
            binding: Some("cmd-b"),
        },
        Shortcut {
            id: "composer.italic",
            title: "hotkeys.actions.composer.italic",
            binding: Some("cmd-i"),
        },
        Shortcut {
            id: "composer.strike",
            title: "hotkeys.actions.composer.strike",
            binding: Some("shift-cmd-x"),
        },
        Shortcut {
            id: "composer.code",
            title: "hotkeys.actions.composer.code",
            binding: Some("cmd-e"),
        },
    ],
];

/// Системные сочетания для справки. Пользователь не может изменить их здесь.
pub(in crate::gpui_client) const FIXED: [Shortcut; 5] = [
    Shortcut {
        id: "fixed.cancel",
        title: "hotkeys.fixed.cancel",
        binding: Some("escape"),
    },
    Shortcut {
        id: "fixed.focus",
        title: "hotkeys.fixed.focus",
        binding: Some("tab"),
    },
    Shortcut {
        id: "fixed.send",
        title: "hotkeys.fixed.send",
        binding: Some("enter"),
    },
    Shortcut {
        id: "fixed.editing",
        title: "hotkeys.fixed.editing",
        binding: Some("cmd-c"),
    },
    Shortcut {
        id: "fixed.focusZones",
        title: "hotkeys.fixed.focusZones",
        binding: Some("f6"),
    },
];

impl Shortcut {
    /// Возвращает название действия в выбранной локали.
    pub fn title(self, language: Language) -> &'static str {
        language.tr(self.title)
    }

    /// Показывает пояснение двух команд, которые зависят от фокуса и контекста.
    pub fn hint(self, language: Language) -> Option<&'static str> {
        match self.id {
            "interface.quoteMessage" => Some(language.tr("hotkeys.quoteHint")),
            "composer.editLastMessage" => Some(language.tr("hotkeys.editLastMessageHint")),
            _ => None,
        }
    }

    /// Читает замену пользователя или стандартную клавишу действия.
    pub fn binding(self, settings: &PreferenceValues) -> Option<&str> {
        settings
            .hotkeys
            .get(self.id)
            .map_or(self.binding, Option::as_deref)
    }
}

/// Причина, по которой сочетание нельзя назначить.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::gpui_client) enum BindingError {
    Invalid,  // GPUI не понял запись клавиши.
    Reserved, // Сочетание нужно системе, редактору или обходу фокуса.
    Conflict, // Клавиша уже назначена другому действию.
}

impl BindingError {
    /// Показывает причину рядом с полем записи.
    pub fn message(self, language: Language) -> &'static str {
        match self {
            Self::Invalid => language.tr("hotkeys.invalid"),
            Self::Reserved => language.tr("hotkeys.reserved"),
            Self::Conflict => language.tr("hotkeys.conflict"),
        }
    }
}

/// Проверяет, что клавиша свободна и не перехватывает стандартную команду.
pub(in crate::gpui_client) fn validate(
    shortcut: Shortcut,
    binding: &str,
    settings: &PreferenceValues,
) -> Result<(), BindingError> {
    let candidate = Keystroke::parse(binding).map_err(|_| BindingError::Invalid)?;
    let modifiers = candidate.modifiers;
    let key = candidate.key.as_str();
    let command = modifiers.platform || modifiers.control;
    let command_only = command && !modifiers.alt && !modifiers.shift;
    let plain_edit_last =
        shortcut.id == "composer.editLastMessage" && key == "up" && !modifiers.modified();
    let quote = shortcut.id == "interface.quoteMessage" && binding == "ctrl-l";
    if matches!(key, "escape" | "tab" | "enter" | "f6")
        || (!modifiers.control && !modifiers.platform && !modifiers.alt && !plain_edit_last)
        || (command && matches!(key, "+" | "=" | "-" | "0"))
        || (command && modifiers.shift && matches!(key, "r" | "t" | "z"))
        || (command_only && key.len() == 1 && "acfvxlnorpstwyzq123456789".contains(key) && !quote)
        || (key == "insert" && (command || modifiers.shift))
        || (key == "delete" && modifiers.shift)
        || (key == "f4" && modifiers.alt)
    {
        return Err(BindingError::Reserved);
    }
    if SHORTCUT_GROUPS.into_iter().flatten().any(|other| {
        other.id != shortcut.id
            && other
                .binding(settings)
                .and_then(|binding| Keystroke::parse(binding).ok())
                .is_some_and(|assigned| {
                    assigned.modifiers == candidate.modifiers && assigned.key == candidate.key
                })
    }) {
        return Err(BindingError::Conflict);
    }
    Ok(())
}

/// Запоминает физическую клавишу GPUI без результата системной композиции.
pub(in crate::gpui_client) fn binding_from_key(key: &Keystroke) -> String {
    Keystroke {
        key_char: None,
        ..key.clone()
    }
    .unparse()
}

/// Находит действие для клавиши с учётом отключения и выбора профиля.
pub(in crate::gpui_client) fn matching(
    key: &Keystroke,
    settings: &PreferenceValues,
) -> Option<Shortcut> {
    if !settings.hotkeys_enabled
        || (!key.modifiers.control
            && !key.modifiers.alt
            && !key.modifiers.platform
            && key.key != "up")
    {
        return None;
    }
    SHORTCUT_GROUPS
        .into_iter()
        .flatten()
        .copied()
        .find(|shortcut| {
            shortcut.binding(settings).is_some_and(|current| {
                Keystroke::parse(current)
                    .is_ok_and(|parsed| parsed.key == key.key && parsed.modifiers == key.modifiers)
            })
        })
}

/// Показывает сочетание в короткой записи macOS.
pub(in crate::gpui_client) fn format(binding: &str) -> String {
    let Ok(key) = Keystroke::parse(binding) else {
        return binding.into();
    };
    let Modifiers {
        control,
        alt,
        shift,
        platform,
        function,
    } = key.modifiers;
    let key_label = match key.key.as_str() {
        "left" => "←".into(),
        "right" => "→".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "tab" => "⇥".into(),
        "enter" => "↵".into(),
        "escape" => "Esc".into(),
        "backspace" => "⌫".into(),
        "delete" => "⌦".into(),
        "space" => "Space".into(),
        key => key.to_uppercase(),
    };
    [
        (control, "⌃"),
        (alt, "⌥"),
        (shift, "⇧"),
        (platform, "⌘"),
        (function, "fn"),
    ]
    .into_iter()
    .filter_map(|(active, glyph)| active.then_some(glyph))
    .chain(std::iter::once(key_label.as_str()))
    .collect::<Vec<_>>()
    .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет резервирование, конфликты и удаление стандартного сочетания.
    #[test]
    fn refuses_reserved_and_conflicting_assignments() {
        let search = SHORTCUT_GROUPS[0][7];
        let mut settings = PreferenceValues::default();
        assert_eq!(
            validate(search, "tab", &settings),
            Err(BindingError::Reserved)
        );
        assert_eq!(
            validate(search, "cmd-b", &settings),
            Err(BindingError::Conflict)
        );
        assert_eq!(validate(search, "ctrl-shift-j", &settings), Ok(()));
        settings.hotkeys.insert(search.id.into(), None);
        assert!(matching(&Keystroke::parse("cmd-k").unwrap(), &settings).is_none());
        settings
            .hotkeys
            .insert(search.id.into(), Some("alt-j".into()));
        assert_eq!(
            matching(&Keystroke::parse("alt-j").unwrap(), &settings)
                .unwrap()
                .id,
            search.id
        );
    }
}
