# Встроенные компоненты аудио

[← README](../README.md)

Opus воспроизводится через `symphonia-adapter-libopus 0.2.9` и статически
собранный libopus из `opusic-sys 0.7.5`. Отдельная DLL не требуется.
Версии и проверки целостности пакетов фиксирует [Cargo.lock](../Cargo.lock).

- [libopus](OPUS-LICENSE.txt) — уведомление из исходного `opus/COPYING`.
- [opusic-sys](OPUSIC-SYS-LICENSE.txt) — лицензия Rust-привязок.
- [symphonia-adapter-libopus](OPUS-ADAPTER-LICENSE.txt) — MIT-лицензия адаптера.
- [Изменение Symphonia core](../vendor/symphonia-core/BEAT-PATCH.md) —
  граница выделения памяти; [MPL-2.0](../vendor/symphonia-core/LICENSE).

В CI уведомления этих компонентов и встроенного шрифта помещаются рядом
с EXE в `COMPONENT-NOTICES.txt`. Сохраняйте этот файл при распространении сборки.
