# CVTL

CVTL моделирует тепловой поток от остывающего тела и предназначен для его воспроизведения нагревателем на `stage2` криомашины. `stage1` — тёплая ступень, `stage2` — холодная.

Физическое ядро M1 содержит модель виртуального тела, тепловые мосты и баланс насадки. Источники и ограничения приведены в [каталоге материалов](data/materials/README.md). Демон пока проверяет конфигурацию и завершается, прототип GUI использует синтетические данные. Целевая ОС стенда — Ubuntu Linux 22.04 LTS. Порядок подключения через имеющиеся PyVISA и GPIB–USB описан в [ADR 0004](docs/adr/0004-keithley-transport.md).

## Запуск и проверка

Проверенная версия Rust — 1.94.1 с компонентами `rustfmt` и `clippy`. Из корня проекта выполнить:

```sh
cargo run --locked -p cvtl-daemon -- config/simulator.yaml
cargo fmt --all -- --check
cargo build --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Для [сравнения GUI](ui/README.md) нужен Node.js 22. После `npm ci` и `npm run build` в `ui/` четыре сборки доступны в `ui/dist/`. Например, из корня можно открыть выбранный вариант по `http://127.0.0.1:8000`:

```sh
python3 -m http.server 8000 --bind 127.0.0.1 --directory ui/dist/svelte-uplot
```

Использовать `config/` для конфигураций поставки. Локальные конфигурации и результаты прогонов хранить в `.local/` и `runs/` соответственно.

[Техническое задание](TZ.md) · [План M0–M9](docs/PLAN.md) · [Статус и проверки](docs/STATUS.md) · [Открытые вопросы](docs/OPEN_QUESTIONS.md)
