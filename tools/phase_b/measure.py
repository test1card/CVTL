"""Офлайн-расчёт метрик по таблице Фазы B."""

import csv
import hashlib
import io
import json
import math
from pathlib import Path
import re
import sys


PASSPORT_FIELDS = (
    "run_id", "host", "source", "thermometer", "transport", "load", "power_reference", "conditions",
)
TIMING_PAIRS = {
    "ab_read_shift": ("a_read_started_ns", "b_read_started_ns"),
    "cycle_to_ack": ("started_ns", "command_ack_ns"),
    "read_a_to_ack": ("a_read_started_ns", "command_ack_ns"),
    "send_to_ack": ("command_sent_ns", "command_ack_ns"),
}
READ_AND_COMMAND_TIMES = (
    "a_read_started_ns", "a_read_finished_ns", "b_read_started_ns", "b_read_finished_ns",
    "command_sent_ns", "command_ack_ns",
)
REQUIRED_TICKS = ("tick", "scheduled_ns", "started_ns")
INTEGER_FIELDS = REQUIRED_TICKS + ("a_sample_ns", "b_sample_ns") + READ_AND_COMMAND_TIMES
POWER_FIELDS = ("a_voltage_v", "a_current_a", "b_voltage_v", "b_current_a", "target_w", "reference_w")
CSV_HEADER = INTEGER_FIELDS + POWER_FIELDS


def positive_int(value, field):
    if type(value) is not int or value <= 0:
        raise ValueError(f"{field} должен быть положительным целым числом")
    return value


def read_metadata(payload):
    data = json.loads(payload.decode("utf-8"))
    if not isinstance(data, dict) or type(data.get("schema_version")) is not int or data["schema_version"] != 1:
        raise ValueError("Поддерживается только schema_version 1")
    origin = data.get("origin")
    if not isinstance(origin, dict) or origin.get("kind") not in ("test_only", "external", "cvtl"):
        raise ValueError("origin.kind должен быть test_only, external или cvtl")
    description = origin.get("description")
    if not isinstance(description, str) or not description.strip():
        raise ValueError("Требуется текстовое описание origin.description")
    result_origin = {"kind": origin["kind"], "description": description}
    for field in ("raw_sha256", "export_sha256"):
        digest = origin.get(field)
        if digest is not None or origin["kind"] == "cvtl":
            if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-fA-F]{64}", digest):
                raise ValueError(f"Требуется SHA-256 в origin.{field}")
            result_origin[field] = digest.lower()

    passport = data.get("passport")
    if passport is None:
        passport = {}
    if not isinstance(passport, dict):
        raise ValueError("passport должен быть объектом")
    missing = []
    for field in PASSPORT_FIELDS:
        value = passport.get(field)
        if value is not None and not isinstance(value, str):
            raise ValueError(f"passport.{field} должен быть текстом или null")
        if value is None or not value.strip():
            missing.append(field)

    period_ns = positive_int(data.get("period_ns"), "period_ns")
    steady = data.get("steady_ticks")
    if not isinstance(steady, list) or len(steady) != 2:
        raise ValueError("steady_ticks должен содержать два номера тактов")
    first, last = (positive_int(tick, "steady_ticks") for tick in steady)
    if first > last:
        raise ValueError("Начало steady_ticks позже конца")
    if "step" not in data:
        raise ValueError("Требуется объект step или null")
    step = data["step"]
    if step is not None:
        if not isinstance(step, dict):
            raise ValueError("step должен быть объектом или null")
        tick = positive_int(step.get("tick"), "step.tick")
        last_tick = positive_int(step.get("last_tick"), "step.last_tick")
        if last_tick <= tick:
            raise ValueError("step.last_tick должен следовать за step.tick")
        step = {"tick": tick, "last_tick": last_tick}

    return {
        "schema_version": 1,
        "origin": result_origin,
        "passport_missing": missing,
        "period_ns": period_ns,
        "steady_ticks": [first, last],
        "step": step,
    }


def read_tick(cells, line, previous):
    if len(cells) != len(CSV_HEADER):
        raise ValueError(f"CSV строка {line}, неверное число столбцов")
    row = {}
    for field, cell in zip(CSV_HEADER, cells):
        value = cell.strip()
        if not value and field not in REQUIRED_TICKS:
            row[field] = None
            continue
        try:
            if field in INTEGER_FIELDS:
                if not re.fullmatch(r"[0-9]+", value):
                    raise ValueError
                row[field] = int(value)
            else:
                row[field] = float(value)
                if not math.isfinite(row[field]):
                    raise ValueError
        except ValueError:
            raise ValueError(f"CSV строка {line}, непригодное значение {field}") from None

    if row["tick"] <= 0:
        raise ValueError(f"CSV строка {line}, tick должен быть положительным")
    if previous is not None:
        for field in REQUIRED_TICKS:
            if row[field] <= previous[field]:
                raise ValueError(f"CSV строка {line}, {field} не возрастает")
    prior_field = "started_ns"
    for field in READ_AND_COMMAND_TIMES:
        if row[field] is not None:
            if row[field] < row[prior_field]:
                raise ValueError(f"CSV строка {line}, {field} раньше {prior_field}")
            prior_field = field

    branch_powers = []
    for branch in ("a", "b"):
        sample_field = f"{branch}_sample_ns"
        sample = row[sample_field]
        finish_field = f"{branch}_read_finished_ns"
        for end_field in (finish_field, "command_sent_ns"):
            end = row[end_field]
            if sample is not None and end is not None and sample > end:
                raise ValueError(f"CSV строка {line}, {sample_field} позже {end_field}")

        voltage, current = row[f"{branch}_voltage_v"], row[f"{branch}_current_a"]
        power = None if voltage is None or current is None else voltage * current
        if power is not None and not math.isfinite(power):
            raise ValueError(f"CSV строка {line}, неконечное произведение I × U ветви {branch.upper()}")
        branch_powers.append(power)

    row["incomplete"] = None in row.values()
    a_power, b_power = branch_powers
    total = None if a_power is None or b_power is None else a_power + b_power
    if total is not None and not math.isfinite(total):
        raise ValueError(f"CSV строка {line}, неконечная суммарная мощность")
    row["measured_w"] = total
    return row


def read_ticks(payload):
    reader = csv.reader(io.StringIO(payload.decode("utf-8"), newline=""), strict=True)
    rows = []
    try:
        if tuple(next(reader, ())) != CSV_HEADER:
            raise ValueError("CSV строка 1, заголовок отличается от схемы версии 1")
        for cells in reader:
            rows.append(read_tick(cells, reader.line_num, rows[-1] if rows else None))
    except csv.Error as error:
        raise ValueError(f"CSV строка {reader.line_num}, {error}") from error
    return rows


def percentiles(values):
    ordered = sorted(values)
    count = len(ordered)

    def rank(percent):
        return ordered[(count * percent + 99) // 100 - 1] if count else None

    return {"count": count, "median": rank(50), "p95": rank(95), "p99": rank(99), "max": rank(100)}


def timing_metrics(rows, period_ns):
    timings = {name: [] for name in TIMING_PAIRS}
    timings.update(jitter=[], sample_to_send=[])
    for previous, row in zip(rows, rows[1:]):
        if row["tick"] == previous["tick"] + 1:
            timings["jitter"].append(abs(row["started_ns"] - previous["started_ns"] - period_ns))
    for row in rows:
        for name, (start, end) in TIMING_PAIRS.items():
            if row[start] is not None and row[end] is not None:
                timings[name].append(row[end] - row[start])
        a, b, sent = row["a_sample_ns"], row["b_sample_ns"], row["command_sent_ns"]
        if a is not None and b is not None and sent is not None:
            timings["sample_to_send"].append(sent - min(a, b))
    return {name: percentiles(values) for name, values in timings.items()}


def mean(values):
    return math.fsum(value / len(values) for value in values) if values else None


def power_metrics(rows):
    measured = [row["measured_w"] for row in rows if row["measured_w"] is not None]
    result = {"measured_total": {"count": len(measured), "mean": mean(measured)}}
    for field, name in (("reference_w", "reference_error"), ("target_w", "target_error")):
        errors = []
        for row in rows:
            if row["measured_w"] is not None and row[field] is not None:
                error = row["measured_w"] - row[field]
                if not math.isfinite(error):
                    raise ValueError(f"CSV такт {row['tick']}, неконечная разность с {field}")
                errors.append(error)
        result[name] = {
            "count": len(errors), "mean_signed": mean(errors),
            "max_abs": max(map(abs, errors), default=None),
        }
    return result


def settling(rows, step):
    def insufficient(reason):
        return {"status": "insufficient", "tick": None, "elapsed_ns": None, "reason": reason}

    if step is None:
        return insufficient("step_not_declared")
    first, last = step["tick"], step["last_tick"]
    interval = [row for row in rows if first <= row["tick"] <= last]
    if len(interval) != last - first + 1:
        return insufficient("horizon_incomplete")
    applied = interval[0]
    ack, target = applied["command_ack_ns"], applied["target_w"]
    if ack is None or target is None or target <= 0:
        return insufficient("step_ack_or_positive_target_missing")

    lower_w = 0.98 * target
    upper_w = 1.02 * target
    settled = None
    for row in interval[1:]:
        power, end = row["measured_w"], row["b_read_finished_ns"]
        a_start, b_start = row["a_read_started_ns"], row["b_read_started_ns"]
        if None in (power, end, a_start, b_start):
            return insufficient("measurement_incomplete")
        if row["target_w"] != target:
            return insufficient("target_changed_or_missing")
        for at in (a_start, b_start, row["a_sample_ns"], row["b_sample_ns"]):
            if at is not None and at < ack:
                return insufficient("measurement_before_step_ack")
        if not lower_w <= power <= upper_w:
            settled = None
        elif settled is None:
            settled = (row["tick"], end - ack)

    if settled is None:
        return {"status": "not_settled", "tick": None, "elapsed_ns": None, "reason": None}
    tick, elapsed_ns = settled
    return {"status": "settled", "tick": tick, "elapsed_ns": elapsed_ns, "reason": None}


def measure(meta_payload, csv_payload):
    result = read_metadata(meta_payload)
    rows = read_ticks(csv_payload)
    first, last = result["steady_ticks"]
    steady = [row for row in rows if first <= row["tick"] <= last]
    first_tick = rows[0]["tick"] if rows else None
    last_tick = rows[-1]["tick"] if rows else None
    result.update(
        input_sha256={
            "meta": hashlib.sha256(meta_payload).hexdigest(),
            "csv": hashlib.sha256(csv_payload).hexdigest(),
        },
        coverage={
            "rows": len(rows),
            "first_tick": first_tick,
            "last_tick": last_tick,
            "missing_ticks": last_tick - first_tick + 1 - len(rows) if rows else 0,
            "incomplete_rows": sum(row["incomplete"] for row in rows),
            "steady_expected": last - first + 1,
            "steady_rows": len(steady),
        },
        timings_ns=timing_metrics(rows, result["period_ns"]),
        power_w=power_metrics(steady),
        settling=settling(rows, result["step"]),
    )
    return result


def main(args):
    if len(args) != 2:
        print("Использование  python3 tools/phase_b/measure.py META CSV", file=sys.stderr)
        return 2
    try:
        meta_payload, csv_payload = (Path(path).read_bytes() for path in args)
        report = measure(meta_payload, csv_payload)
        print(json.dumps(report, ensure_ascii=False, allow_nan=False, indent=2))
    except (OSError, ValueError, OverflowError) as error:
        print(f"Ошибка обработки  {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
