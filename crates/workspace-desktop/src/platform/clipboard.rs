//! Читает пути файлов, которые пользователь скопировал в системный буфер macOS.

use std::path::PathBuf;

/// Забирает записи типа file URL при явной вставке в рабочий редактор.
pub(crate) fn file_paths() -> Vec<PathBuf> {
    // В GPUI-тестах буфер изолирован. Реальные файлы пользователя ему недоступны.
    #[cfg(test)]
    {
        Vec::new()
    }
    #[cfg(not(test))]
    {
        use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};
        use objc2_foundation::NSURL;

        // Редактор вызывает адаптер из обработчика Paste в главном потоке AppKit.
        unsafe {
            NSPasteboard::generalPasteboard()
                .pasteboardItems()
                .map_or_else(Vec::new, |items| {
                    (0..items.len())
                        .filter_map(|index| items.get(index))
                        .filter_map(|item| {
                            let text = item.stringForType(NSPasteboardTypeFileURL)?;
                            let url = NSURL::URLWithString(&text)?;
                            if !url.isFileURL()
                                || url.host().is_some_and(|host| {
                                    let host = host.to_string();
                                    !host.is_empty() && !host.eq_ignore_ascii_case("localhost")
                                })
                            {
                                return None;
                            }
                            let path = PathBuf::from(url.path()?.to_string());
                            path.is_absolute().then_some(path)
                        })
                        .collect()
                })
        }
    }
}
