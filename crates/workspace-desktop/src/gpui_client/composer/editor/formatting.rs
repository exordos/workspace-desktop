//! Форматирует выделение редактора в Markdown и вычисляет новый диапазон курсора.

use std::ops::Range;

/// Действие панели, которое меняет исходный текст сообщения.
#[derive(Clone, Copy)]
pub(in crate::gpui_client::composer) enum Format {
    Link,             // Вставляет ссылку и выделяет её адрес.
    Bold,             // Оборачивает текст парой двойных звёздочек.
    Italic,           // Оборачивает текст парой звёздочек.
    Strike,           // Оборачивает текст парой двойных тильд.
    NumberedList,     // Нумерует выделенные строки.
    BulletedList,     // Добавляет маркер к каждой строке.
    Quote,            // Добавляет знак Markdown-цитаты к каждой строке.
    CodeBlock,        // Вставляет выделение в блок кода.
    InlineCode,       // Оборачивает текст обратными кавычками.
    Spoiler,          // Оборачивает скрываемый текст двойными вертикальными чертами.
    EncryptedSpoiler, // Закрывает выделение маркерами шифрования редактора.
}

impl Format {
    /// Возвращает замену и диапазон выделения относительно её начала в байтах UTF-8.
    pub(super) fn edit(self, selected: &str) -> (String, Range<usize>) {
        match self {
            Self::Link => link(selected),
            Self::Bold => wrap(selected, "**", "**"),
            Self::Italic => wrap(selected, "*", "*"),
            Self::Strike => wrap(selected, "~~", "~~"),
            Self::NumberedList => prefix_lines(selected, |index| format!("{}. ", index + 1)),
            Self::BulletedList => prefix_lines(selected, |_| "- ".to_string()),
            Self::Quote => prefix_lines(selected, |_| "> ".to_string()),
            Self::CodeBlock => wrap(selected, "```\n", "\n```"),
            Self::InlineCode => wrap(selected, "`", "`"),
            Self::Spoiler => wrap(selected, "||", "||"),
            Self::EncryptedSpoiler => wrap(selected, ":::", ":::"),
        }
    }
}

/// Ставит курсор между маркерами для пустого текста или после обёрнутого фрагмента.
fn wrap(selected: &str, opening: &str, closing: &str) -> (String, Range<usize>) {
    let result = format!("{opening}{selected}{closing}");
    let caret = if selected.is_empty() {
        opening.len()
    } else {
        result.len()
    };
    (result, caret..caret)
}

/// Меняет каждую выделенную строку, включая пустую, и ставит курсор в конец.
fn prefix_lines(selected: &str, prefix: impl Fn(usize) -> String) -> (String, Range<usize>) {
    let result = selected
        .split('\n')
        .enumerate()
        .map(|(index, line)| prefix(index) + line)
        .collect::<Vec<_>>()
        .join("\n");
    let end = result.len();
    (result, end..end)
}

/// Оставляет адрес выделенным, чтобы его можно было сразу заменить.
fn link(selected: &str) -> (String, Range<usize>) {
    let label = if selected.is_empty() {
        "текст ссылки"
    } else {
        selected
    };
    let address = label.len() + 3;
    let result = format!("[{label}](https://)");
    let end = result.len() - 1;
    (result, address..end)
}

/// Продолжает список только с курсором в конце строки. Пустой пункт убирает маркер.
pub(super) fn list_newline(text: &str, caret: usize) -> Option<(Range<usize>, String)> {
    if caret < text.len() && !text[caret..].starts_with('\n') {
        return None;
    }
    let before = &text[..caret];
    let start = before.rfind('\n').map_or(0, |index| index + 1);
    let line = &text[start..caret];
    let body = line.trim_start();
    let indent = &line[..line.len() - body.len()];

    let (marker, remainder) = if body.starts_with(['-', '*', '+']) {
        (body[..1].to_string(), &body[1..])
    } else {
        let digits = body.bytes().take_while(u8::is_ascii_digit).count();
        let number = body[..digits].parse::<u64>().ok()?;
        (
            format!("{}.", number.checked_add(1)?),
            body[digits..].strip_prefix('.')?,
        )
    };

    if !remainder.starts_with(char::is_whitespace) {
        return None;
    }
    if remainder.trim().is_empty() {
        return Some((start..caret, String::new()));
    }
    Some((caret..caret, format!("\n{indent}{marker} ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет команды на многострочном Unicode и точный диапазон после замены.
    #[test]
    fn formats_utf8_selection_and_positions_caret() {
        let selected = "Тест🙂\nмир";
        for (action, expected) in [
            (Format::Bold, "**Тест🙂\nмир**"),
            (Format::Italic, "*Тест🙂\nмир*"),
            (Format::Strike, "~~Тест🙂\nмир~~"),
            (Format::NumberedList, "1. Тест🙂\n2. мир"),
            (Format::BulletedList, "- Тест🙂\n- мир"),
            (Format::Quote, "> Тест🙂\n> мир"),
            (Format::CodeBlock, "```\nТест🙂\nмир\n```"),
            (Format::InlineCode, "`Тест🙂\nмир`"),
            (Format::Spoiler, "||Тест🙂\nмир||"),
        ] {
            let (result, caret) = action.edit(selected);
            assert_eq!(result, expected);
            assert_eq!(caret, expected.len()..expected.len());
        }
        let (result, address) = Format::Link.edit(selected);
        assert_eq!(result, "[Тест🙂\nмир](https://)");
        assert_eq!(&result[address], "https://");
        assert_eq!(Format::Bold.edit(""), ("****".into(), 2..2));
        assert_eq!(Format::CodeBlock.edit(""), ("```\n\n```".into(), 4..4));
    }

    /// Проверяет Shift+Enter в пункте списка, выход из пустого пункта и обычный перенос.
    #[test]
    fn continues_list_at_the_end_of_line() {
        let numbered = "Тест\n  7. Слово🙂";
        let end = numbered.len();
        assert_eq!(
            list_newline(numbered, end),
            Some((end..end, "\n  8. ".into()))
        );
        assert_eq!(list_newline(numbered, end - "🙂".len()), None);
        assert_eq!(list_newline("Простой текст", "Простой текст".len()), None);
        assert_eq!(
            list_newline("Тест\n- ", "Тест\n- ".len()),
            Some(("Тест\n".len().."Тест\n- ".len(), String::new()))
        );
    }
}
