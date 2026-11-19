# Прототип GPUI на macOS

[План миграции](gpui-migration.md) · [GPUI 0.2.2](https://docs.rs/gpui/0.2.2/gpui/)

`workspace-desktop` использует точную версию GPUI `0.2.2` из crates.io.
Для сборки на macOS нужны Xcode с macOS SDK и Clang: GPUI генерирует
привязки к системным заголовкам. Feature `font-kit` включает системный
движок текста macOS. Оконный клиент и прототип используют Metal и компилирует
шейдеры при открытии окна через `runtime_shaders`. Отдельный компилятор
`xcrun metal` для этого режима не нужен. Среда проверки: arm64,
macOS 27.0.1, Xcode 26.4.1 (macOS SDK 26.4), Rust 1.100.0.

Из корня репозитория в графической сессии:

```sh
cargo run --locked -p workspace-desktop --example gpui_chat_probe
```

Откроется **Workspace GPUI Chat Probe** с тестовой историей и редактором.
Закрытие окна завершает пример. Пример не читает профиль и не
подключается к Workspace.

Feature `gpui` включена по умолчанию для штатного окна и примеров. Feature `gpui-test` включает тестовый контекст `#[gpui::test]`.

```sh
cargo run --locked -p workspace-desktop --bin workspace-desktop
cargo build --locked -p workspace-desktop --no-default-features --bin workspace-auth
cargo test --locked --workspace --no-default-features
```

## Открыть галерею компонентов

В графической сессии macOS запустите галерею из корня репозитория.

```sh
cargo run --locked -p workspace-desktop --example ui_gallery
```

Светлая и тёмная палитры показаны рядом. Выберите строку каталога мышью
или клавишами Tab и Enter. В нижней части каждой панели можно вводить
многострочный черновик.

## Стенд ленты с догрузкой

```sh
cargo run --locked -p workspace-desktop --example gpui_history_probe
```

Окно использует тот же `Viewport`, что и чат, и фиксированные данные UI-11. В длинной сцене показан участок из 600 сообщений. Локальный источник выдаёт соседние страницы по 50 из истории на 20 000 сообщений с задержкой 240 мс.

- **К старому краю** и **К новому краю** возвращают фиксированную исходную позицию. Прокрутите к выбранному краю. Строка состояния показывает сторону, число загруженных сообщений и расстояние на момент запроса. Запрос начинается в пределах одной высоты списка до края, с верхним пределом 1 200 логических px для измеряемого запаса.
- **Короткий хвост** начинает с трёх сообщений и догружает страницу сверху без жеста. Последнее сообщение остаётся у нижнего края.

Автотест подаёт жесты вверх и вниз, продолжает прокрутку до получения ответа и проверяет тот же UUID с допуском 1 логический px после вставки. Строки `page_requested`, `page_applied` и `anchor_measured` в тестовом выводе связывает `op_id`.

```sh
cargo test --locked -p workspace-desktop --features gpui-test --lib history_lab -- --nocapture
```

## Воспроизвести скачок истории при догрузке сверху

Из корня проекта в графической сессии macOS выполните:

```sh
cargo run --locked -p workspace-desktop --example gpui_chat_probe -- --reproduce-scroll-jump 2>&1 | tee /tmp/gpui-scroll-jump.log
```

Откроется **Workspace GPUI Chat Probe**. Сценарий сам переходит к известному UUID и подаёт
64 клавиши через GPUI с целевым интервалом 12 мс. В это время история вставляет 50 строк сверху,
загружает изображение в строке выше опоры и получает новое сообщение снизу. В конце сценарий
проверяет, что у нижнего края новое сообщение стало видимым. Не вводите свой текст и не скролльте
окно до завершения сценария: эти действия меняют измеряемую позицию и черновик.

- `op_id=prepend-1`: сопоставьте `update_requested`, `list_updated` и `anchor_measured`.
  Там есть UUID, число строк до и после и координаты в логических пикселях от области прокрутки.
  `target_inset_px` и `observed_inset_px` — заданное и фактическое смещения внутри опорной строки.
  Смещение `delta_px` допустимо в пределах ±1 px при том же UUID.
- `op_id=image-2` и `op_id=incoming-3` проверяют ту же опору после роста строки и прихода сообщения.
  `op_id=follow-4 mode=follow_end` — штатное движение к новому хвосту, здесь нужен `bottom_gap_px`
  в пределах ±1 px, а UUID должен смениться.
- `typing_result` считает обработанные клавиши и проверяет весь результат ввода без записи текста.
  `max_dispatch_us` — максимальное время обработки одного события внутри GPUI.
  `max_gap_us` — максимальная пауза между подачами клавиш, включая ожидание таймера и работу цикла окна.
  Для замеров на каждом шаге добавьте `WORKSPACE_GPUI_PROBE_TRACE_FRAMES=1` перед `cargo run`.
  `paint_wait` измеряет ожидание подтверждённой отрисовки, а не длительность одного кадра.

После `repro_finished result=ok` окно остаётся открытым. При расхождении оно останавливается
на проверяемом шаге с `result=failed`. Чтобы совместить снимок с журналом, оставьте окно открытым
и в отдельном терминале получите PID и номер окна этого прототипа:

```sh
probe_pid=$(pgrep -nx gpui_chat_probe)
window_number=$(swift - "$probe_pid" <<'SWIFT'
import CoreGraphics
import Foundation
guard CommandLine.arguments.count == 2,
      let pid = Int32(CommandLine.arguments[1]),
      let windows = CGWindowListCopyWindowInfo(
        [.excludeDesktopElements, .optionOnScreenOnly], kCGNullWindowID
      ) as? [[String: Any]],
      let window = windows.first(where: {
        $0[kCGWindowOwnerPID as String] as? Int32 == pid &&
        $0[kCGWindowLayer as String] as? Int == 0
      }),
      let number = window[kCGWindowNumber as String] as? Int
else { exit(1) }
print(number)
SWIFT
)
if [ -n "$window_number" ]; then
  screencapture -x -o -l "$window_number" /tmp/gpui-scroll-jump.png
fi
```

Эту команду используйте с одним запущенным прототипом. Если номер не найден,
выведите окно на экран и проверьте разрешение macOS на запись экрана для терминала.
На кадре будут только фиксированные данные тестового чата.

Автотест подаёт клавиши до и после догрузки в том же окне и проверяет итоговый черновик,
координату опоры и новое сообщение у нижнего края:

```sh
cargo test --locked -p workspace-desktop --features gpui-test --example gpui_chat_probe
```
