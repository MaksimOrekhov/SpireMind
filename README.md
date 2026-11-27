# SpireMind

SpireMind — локальный советник для одиночных забегов Slay the Spire 2. SM-01 запускает опубликованную модель Instinct на CPU и ранжирует действия Ironclad A10 в симуляторе Spire Mentor для игры `v0.111.0`. SM-02 добавляет подключение к запущенной игре через Advisor Mode. Оба режима используют одни и те же веса и поддерживают боевой ход, карточную награду и выбор следующего узла карты.

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

## SM-02. Советник для запущенной игры

Live-клиент читает `advice_request` штатного мода **Spire Mentor 0.11.10**. Заявленная граница — одиночный стандартный Ironclad A0 или A10, игра `v0.111.0` (`41cef1ea`) или `v0.111.1` (`cc1f9462`), модель SM-01. Фактический статус каждой поверхности записан в [отчёте SM-02](docs/research/live-results.md).

1. Установите Spire Mentor 0.11.10 и BaseLib 3.4.7. При недоступном Workshop соберите закреплённый мод из исходников по инструкции в отчёте. Запустите игру в режиме с модами. В **Mod Configuration → Spire Mentor** выключите **Auto-launch Python advisor** и включите режим **Coach** или **Hints**. Оставьте Auto Play и Auto Battle выключенными.
2. Из корня SpireMind выполните `python scripts/setup.py`, затем `python scripts/setup.py --check-source`. Соберите `cargo build --release --locked` в **x64 Native Tools Command Prompt for VS 2022**. Обновлённый MSVC нужен для транзитивного кода upstream. При запуске уже собранного CLI компилятор и Python не используются.
3. Запустите советник в отдельном терминале до первого выбора на карте. Подставьте путь к `release_info.json` установленной игры.

```powershell
$release = 'D:\SteamLibrary\steamapps\common\Slay the Spire 2\release_info.json'
.\target\release\spiremind.exe live --release-info $release
```

Вручную начните стандартный одиночный забег Ironclad. Рекомендации появятся в терминале SpireMind. Панель Spire Mentor `Hints` в игре относится к моду. В терминале позиция и индексы относятся к текущему снимку. Ранжирование выражает вероятность выбора класса действия. Статус `forced` означает ровно одно легальное действие без оценки модели. `blocked` сообщает причину отказа. `stale` аннулирует предыдущий совет. При отсутствии нового сигнала совет истекает через 1,5 с по умолчанию. `--ttl-ms 5000` даёт больше времени для чтения и сохраняет немедленную очистку при получении события. Пропущенный переход карты блокирует бой до восстановления точной позиции из следующего видимого снимка или подтверждённого выбора на карте.

Для проверки сохраните санитизированные снимки и результаты в игнорируемом каталоге. `--json` выводит события в JSON Lines с явной инвалидацией. `receive_to_result_ms` отсчитывается от приёма полного кадра в этом процессе до публикации результата. Мод не присылает сопоставимую метку времени захвата экрана.

```powershell
New-Item -ItemType Directory -Force .local/live-captures | Out-Null
.\target\release\spiremind.exe live --release-info $release --json --capture-dir .local/live-captures |
  Tee-Object -FilePath .local/live-captures/events.jsonl
$snapshots = @(Get-ChildItem .local/live-captures -Filter 'advisor-*.json' | Select-Object -ExpandProperty FullName)
.\target\release\spiremind.exe replay @snapshots
```

Остановите CLI через `Ctrl+C`. Чтобы отключить мод, переведите его в режим **Off** или выключите в меню модов. Для отката ручной установки закройте игру и вынесите папку `mods/SpireMentor` из каталога игры. `mods/BaseLib` оставьте, если она нужна другим модам. Для Workshop-установки отпишитесь от Spire Mentor. [ADR live bridge](docs/adr/0002-live-bridge.md)
