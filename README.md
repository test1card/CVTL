# CVTL

CVTL моделирует тепловой поток от остывающего тела и предназначен для его воспроизведения нагревателем на `stage2` криомашины. `stage1` — тёплая ступень, `stage2` — холодная.

Физическое ядро содержит модель виртуального тела, тепловые мосты, баланс насадки и расчёт проводки. CLI рассчитывает пределы мощности и помогает подобрать нагреватель. Источники свойств приведены в [каталоге материалов](data/materials/README.md). Демон пока проверяет конфигурацию и завершается, прототип GUI использует синтетические данные. Целевая ОС стенда — Ubuntu Linux 22.04 LTS. Порядок подключения через имеющиеся PyVISA и GPIB–USB описан в [ADR 0004](docs/adr/0004-keithley-transport.md).

## Запуск и проверка

Проверенная версия Rust — 1.94.1 с компонентами `rustfmt` и `clippy`. Из корня проекта выполнить:

```sh
cargo run --locked -p cvtl-daemon -- config/simulator.yaml
cargo fmt --all -- --check
cargo build --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Автономные расчёты M2

```sh
cargo run --locked -p cvtl-cli -- wire data/scenarios/m2-wire-copper.yaml
cargo run --locked -p cvtl-cli -- heater-advisor data/scenarios/m2-heater-candidates.yaml
```

`wire` принимает `schema_version: 1`, словарь `terminals` с температурами в К и список `segments`. У каждого участка есть `from`, `to`, `material`, `length_m`, `strands`, `current_a` и один из параметров `area_mm2` или `diameter_mm`. Геометрия и ток относятся к одной жиле. Выход даёт потоки в каждый узел со знаком и отдельно показывает джоулево тепло и излучение. Для расчёта по временному профилю используются последовательные снимки температур и токов.

### Дополнительные поля участка

- `resistivity` — `source` и `table` с парами `[температура в К, удельное сопротивление в Ом·м]`. Обязательно при ненулевом токе.
- `radiation` — `surface_area_m2` одной жилы, `emissivity` и `environment_k`. При отсутствии поля излучение не входит в баланс.
- `uncertainty` — относительные вариации `k_relative` и `length_relative`, каждая от 0 включительно до 1 исключительно. CLI выводит сценарный диапазон для узла, если вариации указаны у всех его участков.

`heater-advisor` принимает `schema_version: 1`, `voltage_factor`, `current_factor`, `total_limit_w` и `candidates`. В каждом кандидате указываются `label` и `channel_a` с диапазоном `resistance_ohm: [минимум, максимум]`, `loop_resistance_ohm` и `heater_limit_w`. Без `channel_b` расчёт предполагает одинаковые ветви. `null` в поле предела сохраняет статус «предел не задан». Математическая модель, источники и ограничения разобраны в [ADR 0006](docs/adr/0006-wire-and-envelope.md).

Для [сравнения GUI](ui/README.md) нужен Node.js 22. После `npm ci` и `npm run build` в `ui/` четыре сборки доступны в `ui/dist/`. Например, из корня можно открыть выбранный вариант по `http://127.0.0.1:8000`:

```sh
python3 -m http.server 8000 --bind 127.0.0.1 --directory ui/dist/svelte-uplot
```

Использовать `config/` для конфигураций поставки. Локальные конфигурации и результаты прогонов хранить в `.local/` и `runs/` соответственно.

[Техническое задание](TZ.md) · [План M0–M9](docs/PLAN.md) · [Статус и проверки](docs/STATUS.md) · [Открытые вопросы](docs/OPEN_QUESTIONS.md)
