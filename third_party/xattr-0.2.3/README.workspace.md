# Совместимость с `libc` на Linux

Копия пакета [xattr 0.2.3 с crates.io](https://crates.io/crates/xattr/0.2.3) с исходным кодом и лицензиями. GPUI 0.2.2 требует эту ветку через `gpui_http_client` и `zed-async-tar`.

`libc` 0.2.190 убрала неподдерживаемый псевдоним `ENOATTR` на Linux и Android. Локальная правка в `src/sys/mod.rs` использует `ENODATA` на этих системах. Удалите замену после обновления цепочки зависимостей GPUI. [Задача GPUI](https://github.com/zed-industries/zed/issues/67211)
