# Phase B: локальный VISA-мост и проверка стенда

Этот каталог — первая часть Фазы B. Он не входит в сборку Фазы A и не
включает нагрев: `hardware-check` только читает идентичность, состояние
выходов, измерения и термометрию. Перед измерениями обе ветви Keithley
командуются OFF, а их состояние проверяется по чтению.

## 1. Запустить мост

Мост использует рабочее окружение CryoDAQ: в нём PyVISA-py находит Keithley
2604B и python-gpib работает с NI GPIB-USB-HS.

```sh
/home/lab53/miniforge3/envs/cryodaq/bin/python tools/phase_b/visa_bridge.py \
  --resource 'USB0::0x05E6::0x2604::4083236::0::INSTR' --port 7101
```

```sh
/home/lab53/miniforge3/envs/cryodaq/bin/python tools/phase_b/visa_bridge.py \
  --resource 'GPIB0::12::INSTR' --port 7102
```

Мост печатает JSON с фактическим портом и слушает только loopback. Проверка
без сервера:

```sh
/home/lab53/miniforge3/envs/cryodaq/bin/python tools/phase_b/visa_bridge.py \
  --resource 'GPIB0::12::INSTR' --check
```

## 2. Проверить стенд

```sh
cargo run --locked -p cvtl-cli -- hardware-check \
  --keithley 127.0.0.1:7101 --lake 127.0.0.1:7102 --channels 1,2,3,4
```

Результат — JSON: идентичность, `off` для A и B, I/U каждой ветви, очередь
ошибок, температуры и сырые единицы датчиков, а также времена обмена.
`--unit ohms` переключает сырые единицы, `--timeout-ms` задаёт срок.

## Протокол моста

```
client -> bridge   W <command>\n   |  Q <command>\n
bridge -> client   OK\n            |  R <length>\n<payload>\n  |  ERR <text>\n
```

`W` — запись без ответа, `Q` — запрос с ответом. Один абсолютный срок покрывает
запись и чтение; любая ошибка протокола или обмена делает сеанс непригодным.
Мост обслуживает одного клиента за раз, принимает только печатный ASCII до
1024 байт и не выполняет полученный текст как код.

## Что уже выяснено на стенде

- Реальный 2604B отвечает `Keithley Instruments Inc., Model 2604B, 4083236, 3.2.1`;
  регистр в `*IDN?` смешанный, поэтому разбор идентичности регистронезависим.
- Реальный 218S **игнорирует аргумент канала** в `KRDG? <n>`, `SRDG? <n>` и
  `RDGST? <n>`: возвращается значение первого канала. Правильный путь — пакетные
  `KRDG?`, `SRDG?`, `RDGST?`, которые отдают все восемь каналов. Драйвер
  `LakeShore218::read_channels` делает один пакет на такт.
- На `GPIB0::12` канал 4 имеет статус `032`; пакетный драйвер сообщает ошибку
  этого канала, не теряя остальные. Для A1 это значит, что карта датчиков
  (B-04) обязательна: сейчас цикл требует пригодными все четыре канала.
- Пакетное чтение трёх величин с одного 218S занимает ~55 мс на шине;
  два `measure.iv()` при NPLC 1.0 — ~42 мс.
- pyvisa-py в `.venv` (Python 3.12) и в conda (Python 3.14) по-разному читает
  USB-серийник (`4083236` против `4083236\x00`). Мост пробует оба варианта.

## Ограничения

- Один владелец шины: перед запуском моста CryoDAQ должен быть остановлен.
- `hardware-check` очищает очередь ошибок Keithley и выдаёт OFF обеим ветвям;
  других записей нет.
- Многоприборная карта `stage1/stage2/tf/jacket` и fast/cascade-контур — следующие
  пакеты. Сейчас проверяется один ресурс 218S и один Keithley.
- Нагрев, пределы, watchdog и независимое отключение в этот пакет не входят.

## Тесты

```sh
cargo test --locked -p cvtl-instruments visa_bridge
cargo test --locked -p cvtl-sim --test lakeshore_batch
cargo test --locked -p cvtl-sim --test lakeshore_protocol
/home/lab53/miniforge3/envs/cryodaq/bin/python tools/phase_b/test_visa_bridge.py
```
