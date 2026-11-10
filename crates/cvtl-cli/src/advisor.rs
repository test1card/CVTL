use crate::{check_version, quantity, read_yaml};
use cvtl_core::units::{Ohms, Watts};
use cvtl_envelope::{
    advise_pair, Advice, ChannelSpec, LimitingFactor, Margins, Point, ResistanceRange,
};
use serde::Deserialize;
use std::{fmt::Write, path::Path};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    schema_version: u32,
    voltage_factor: f64,
    current_factor: f64,
    total_limit_w: Option<f64>,
    candidates: Vec<Candidate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Candidate {
    label: String,
    channel_a: ChannelInput,
    channel_b: Option<ChannelInput>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelInput {
    resistance_ohm: [f64; 2],
    loop_resistance_ohm: f64,
    heater_limit_w: Option<f64>,
}

impl ChannelInput {
    fn to_spec(self) -> Result<ChannelSpec, String> {
        let [min, max] = self.resistance_ohm;
        Ok(ChannelSpec {
            resistance: ResistanceRange::new(
                quantity(Ohms::new(min), "resistance_ohm")?,
                quantity(Ohms::new(max), "resistance_ohm")?,
            )
            .map_err(|_| "Ожидается диапазон сопротивления 0 < R_min ≤ R_max")?,
            loop_resistance: quantity(Ohms::new(self.loop_resistance_ohm), "loop_resistance_ohm")?,
            heater_limit: optional_power(self.heater_limit_w, "heater_limit_w")?,
        })
    }
}

fn optional_power(power: Option<f64>, field: &str) -> Result<Option<Watts>, String> {
    power.map(|p| quantity(Watts::new(p), field)).transpose()
}

fn point(output: &mut String, name: &str, p: Point) {
    let limiting = match p.limiting {
        LimitingFactor::Current => "ток",
        LimitingFactor::Voltage => "напряжение",
        LimitingFactor::CurrentAndVoltage => "ток и напряжение",
        LimitingFactor::Heater => "нагреватель",
    };
    writeln!(
        output,
        "    {name}: {:.6} Вт при R = {:.6} Ом, I = {:.6} А, U_force = {:.6} В, U_sense = {:.6} В, запас U_force = {:.6} В. Ограничение — {limiting}.",
        p.power.get(), p.resistance.get(), p.current.get(), p.force_voltage.get(),
        p.sense_voltage.get(), p.voltage_headroom.get(),
    ).unwrap();
}

fn channel(output: &mut String, name: &str, advice: Advice) {
    writeln!(output, "  Канал {name}").unwrap();
    point(output, "предел источника", advice.source);
    if let Some(capped) = advice.capped {
        point(output, "с пределом нагревателя", capped);
    } else {
        writeln!(output, "    предел нагревателя не задан").unwrap();
    }
}

pub fn run(path: &Path) -> Result<String, String> {
    let input: Input = read_yaml(path)?;
    check_version(input.schema_version)?;
    let margins = Margins::new(input.voltage_factor, input.current_factor)
        .map_err(|_| "Коэффициенты запаса должны лежать в интервале (0, 1]")?;
    let total = optional_power(input.total_limit_w, "total_limit_w")?;
    if input.candidates.is_empty() {
        return Err("Список кандидатов пуст".into());
    }
    let mut output = String::from("Keithley 2604B — расчёт по номинальной области постоянного тока до 30 °C окружающей среды. Допуск к нагреву требует стендовой проверки.\n");
    for c in input.candidates {
        let result = advise_pair(
            c.channel_a.to_spec()?,
            c.channel_b.unwrap_or(c.channel_a).to_spec()?,
            margins,
            total,
        )
        .map_err(|_| {
            format!(
                "Кандидат {}: недопустимый предел, проводка или результат",
                c.label
            )
        })?;
        writeln!(output, "\nКандидат {}", c.label).unwrap();
        channel(&mut output, "A", result.a);
        channel(&mut output, "B", result.b);
        writeln!(
            output,
            "  Сумма по источнику: {:.6} Вт",
            result.source_sum.get()
        )
        .unwrap();
        if let Some(sum) = result.heater_capped_sum {
            writeln!(
                output,
                "  Сумма с пределами нагревателей: {:.6} Вт",
                sum.get()
            )
            .unwrap();
        }
        match result.capped_sum {
            Some(sum) => writeln!(output, "  Сумма с общим пределом: {:.6} Вт", sum.get()).unwrap(),
            None => writeln!(output, "  Итог с ограничениями недоступен без обоих пределов нагревателей и общего предела").unwrap(),
        }
    }
    Ok(output)
}
