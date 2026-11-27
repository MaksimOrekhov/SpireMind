# SpireMind

SpireMind — локальный советник для одиночных забегов Slay the Spire 2. PoC SM-01 запускает опубликованную модель Instinct на CPU и ранжирует действия Ironclad A10 в симуляторе Spire Mentor для игры `v0.111.0`. CLI поддерживает боевой ход, выбор карточной награды и следующий узел карты. Подключение к запущенной игре относится к следующему этапу.

## Установка и запуск

Нужны Git, Python 3.12 для первоначальной загрузки и `rustup`. На Windows x64 установите Visual Studio 2022 Build Tools с нагрузкой **Desktop development with C++**, MSVC и Windows SDK. На macOS нужны Xcode Command Line Tools (`xcode-select --install`). Rust 1.99.1 задан в `rust-toolchain.toml`. Для сборки зависимости берутся из `Cargo.lock`.

Из корня репозитория выполните:

```text
python scripts/setup.py
cargo build --release --locked
cargo run --release --locked -- recommend combat --seed 160000008
cargo run --release --locked -- recommend card-reward --seed 160000000
cargo run --release --locked -- recommend map --seed 160000000
```

Если на macOS Python доступен под именем `python3`, используйте его для `scripts/setup.py`. Скрипт скачивает закреплённую ревизию исходников и сверяет SHA‑256 официального файла весов. После установки и сборки CLI работает в офлайне. Для `cargo` можно добавить `--offline`. Инференс использует нативные Rust-библиотеки и не требует Steam, Godot, .NET, PyTorch, GPU или сервер.

Для бенчмарка на Windows используйте собранный CLI. На macOS тот же исполняемый файл доступен как `./target/release/spiremind`.

```powershell
.\target\release\spiremind.exe benchmark --runs 10 --timeout-seconds 120
```

## Что возвращает CLI

- `recommend` воспроизводит забег по `--seed` и останавливается на первом подходящем решении с несколькими классами Instinct. `state.id` содержит симуляторный seed и индекс решения. `provenance` содержит точный commit движка, игру, модель, хеш и профиль входных данных.
- `legal_actions` содержит каждое законное действие с локальным `action_id`. Поле `detail` указывает экземпляр карты в руке, цель, карточку предложения, пропуск или координаты узла, где применимо. Индексы начинаются с нуля и действуют в пределах `state.id`.
- `recommendations` ранжирует классы модели. Один класс может объединять эквивалентные экземпляры карт. Список `action_ids` сохраняет все варианты, `representative_action_id` совпадает с представителем из upstream. Сортировка по логиту убывает, равные логиты сохраняют меньший `class_id` первым. `policy_probability` — softmax по классам доступных действий. Экспортированный чекпойнт содержит policy head, поэтому вероятность победы отсутствует.
- `latency_ms.forward` измеряет только проход модели. `benchmark` возвращает счётчики по итогам забегов, длительность и распределение времени forward pass. При незавершённой панели он выводит частичный отчёт и возвращает код 1. `recommend` сообщает о недоступном состоянии или проблеме с моделью JSON-сообщением в stderr и также возвращает код 1.

Для тестов без весов запустите `python scripts/setup.py --source-only`, затем `cargo test --release --locked`. Реальный smoke запускается отдельно: `cargo test --release --locked model_smoke -- --ignored --nocapture`. См. [фактические результаты и команды](docs/research/poc-results.md).

[Архитектура](docs/architecture.md) · [ADR интеграции](docs/adr/0001-engine-integration.md) · [Исследование upstream](docs/research/upstream.md) · [Дорожная карта](docs/roadmap.md) · [Уведомление MIT](THIRD_PARTY_NOTICES.md)
