use crate::{check_version, quantity, read_yaml};
use cvtl_core::units::{Amperes, Kelvin, Meters, OhmMeters, SquareMeters};
use cvtl_materials::{lakeshore_wire_conductivity, nist_conductivity, LAKESHORE_WIRE_SOURCE};
use cvtl_physics::wire::{sensitivity, solve, Radiation, ResistivityCurve, WireError, WireSegment};
use serde::Deserialize;
use std::{collections::BTreeMap, f64::consts::PI, fmt::Write, path::Path};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    schema_version: u32,
    terminals: BTreeMap<String, f64>,
    segments: Vec<SegmentInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentInput {
    from: String,
    to: String,
    material: String,
    area_mm2: Option<f64>,
    diameter_mm: Option<f64>,
    length_m: f64,
    strands: u32,
    current_a: f64,
    resistivity: Option<ResistivityInput>,
    radiation: Option<RadiationInput>,
    uncertainty: Option<UncertaintyInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResistivityInput {
    source: String,
    table: Vec<[f64; 2]>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RadiationInput {
    surface_area_m2: f64,
    emissivity: f64,
    environment_k: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncertaintyInput {
    k_relative: f64,
    length_relative: f64,
}

struct Heat {
    nominal: f64,
    bounds: Option<(f64, f64)>,
}

impl Default for Heat {
    fn default() -> Self {
        Self {
            nominal: 0.0,
            bounds: Some((0.0, 0.0)),
        }
    }
}

impl Heat {
    fn add(&mut self, nominal: f64, bounds: Option<(f64, f64)>) -> Result<(), String> {
        self.nominal += nominal;
        self.bounds = self
            .bounds
            .zip(bounds)
            .map(|((a, b), (c, d))| (a + c, b + d));
        if !self.nominal.is_finite()
            || self
                .bounds
                .is_some_and(|(a, b)| !a.is_finite() || !b.is_finite())
        {
            return Err("Неконечная сумма тепловых потоков".into());
        }
        Ok(())
    }
}

fn temperature(terminals: &BTreeMap<String, f64>, node: &str) -> Result<Kelvin, String> {
    let value = *terminals
        .get(node)
        .ok_or_else(|| format!("Узел {node} не задан в terminals"))?;
    quantity(Kelvin::new(value), node)
}

fn section(input: &SegmentInput) -> Result<SquareMeters, String> {
    let area = match (input.area_mm2, input.diameter_mm) {
        (Some(a), None) if a > 0.0 => a * 1e-6,
        (None, Some(d)) if d > 0.0 => PI * (d * 0.5e-3).powi(2),
        _ => {
            return Err(
                "Задайте одно положительное сечение area_mm2 или диаметр diameter_mm".into(),
            )
        }
    };
    quantity(SquareMeters::new(area), "сечение жилы")
}

fn resistivity(input: &ResistivityInput) -> Result<ResistivityCurve, String> {
    if input.source.trim().is_empty() {
        return Err("Укажите источник таблицы удельного сопротивления".into());
    }
    let rows = input
        .table
        .iter()
        .map(|&[t, rho]| {
            Ok((
                quantity(Kelvin::new(t), "температура ρ(T)")?,
                quantity(OhmMeters::new(rho), "ρ(T)")?,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    ResistivityCurve::table(rows).map_err(|_| "Некорректная таблица удельного сопротивления".into())
}

fn radiation(input: &RadiationInput) -> Result<Radiation, String> {
    Ok(Radiation {
        surface_area_m2: quantity(SquareMeters::new(input.surface_area_m2), "surface_area_m2")?,
        emissivity: input.emissivity,
        environment_k: quantity(Kelvin::new(input.environment_k), "environment_k")?,
    })
}

pub fn run(path: &Path) -> Result<String, String> {
    let input: Input = read_yaml(path)?;
    check_version(input.schema_version)?;
    if input.segments.is_empty() {
        return Err("Список участков пуст".into());
    }
    let mut totals: BTreeMap<&str, Heat> = BTreeMap::new();
    let mut output = String::from("Стационарный расчёт проводки по заданному снимку. Знак «+» означает тепло, поступающее в узел.\n");
    for s in &input.segments {
        if s.from == s.to || s.from.trim().is_empty() || s.to.trim().is_empty() || s.strands == 0 {
            return Err("Участку нужны два разных узла и ненулевое число жил".into());
        }
        let (conductivity, source) = if s.material.starts_with("lakeshore_") {
            let curve = lakeshore_wire_conductivity(&s.material)
                .map_err(|_| format!("Нет теплопроводности для {}", s.material))?;
            (curve, LAKESHORE_WIRE_SOURCE)
        } else {
            let c = nist_conductivity(&s.material)
                .map_err(|_| format!("Нет теплопроводности для {}", s.material))?;
            (c.curve, c.provenance.source_url)
        };
        let segment = WireSegment {
            conductivity,
            resistivity: s.resistivity.as_ref().map(resistivity).transpose()?,
            length_m: quantity(Meters::new(s.length_m), "length_m")?,
            area_m2: section(s)?,
            current_a: quantity(Amperes::new(s.current_a), "current_a")?,
            start_k: temperature(&input.terminals, &s.from)?,
            end_k: temperature(&input.terminals, &s.to)?,
            radiation: s.radiation.as_ref().map(radiation).transpose()?,
        };
        let context = |error| {
            let reason = match error {
                WireError::InvalidInput => "недопустимые параметры проводника или источников тепла",
                WireError::MissingResistivity => {
                    "при ненулевом токе требуется таблица удельного сопротивления"
                }
                WireError::OutOfRange => "температура вне диапазона свойств материала",
                WireError::NoConvergence => "расчёт не сошёлся в допустимой области",
            };
            format!("Участок {} → {}: {reason}", s.from, s.to)
        };
        let (nominal, corners) = if let Some(u) = &s.uncertainty {
            let band = sensitivity(&segment, u.k_relative, u.length_relative).map_err(context)?;
            (band.nominal, Some(band.corners))
        } else {
            (solve(&segment).map_err(context)?, None)
        };
        let count = f64::from(s.strands);
        let flows = |result: &cvtl_physics::wire::WireResult| {
            [
                result.heat_into_start_w.get() * count,
                result.heat_into_end_w.get() * count,
            ]
        };
        for (i, name) in [s.from.as_str(), s.to.as_str()].into_iter().enumerate() {
            let flux = flows(&nominal)[i];
            let bounds = corners.as_ref().map(|c| {
                c.iter()
                    .map(|v| flows(v)[i])
                    .fold((flux, flux), |(lo, hi), v| (lo.min(v), hi.max(v)))
            });
            totals.entry(name).or_default().add(flux, bounds)?;
        }
        let [from_w, to_w] = flows(&nominal);
        let joule = nominal.joule_w.get() * count;
        let radiative = nominal.radiation_w.get() * count;
        if !joule.is_finite() || !radiative.is_finite() {
            return Err("Неконечная сумма источников тепла участка".into());
        }
        writeln!(output, "\n{} → {} ({} жил): в начальный узел {from_w:.6} Вт, в конечный {to_w:.6} Вт. Джоулево тепло {joule:.6} Вт, излучение {radiative:.6} Вт.", s.from, s.to, s.strands).unwrap();
        writeln!(output, "  Теплопроводность {} — {source}", s.material).unwrap();
        if let Some(profile) = &s.resistivity {
            writeln!(
                output,
                "  Удельное сопротивление — профиль пользователя, {}",
                profile.source
            )
            .unwrap();
        }
    }
    writeln!(output, "\nТепло в узлы").unwrap();
    for (node, heat) in totals {
        write!(output, "  {node}: {:.6} Вт", heat.nominal).unwrap();
        match heat.bounds {
            Some((lo, hi)) => {
                writeln!(output, ", сценарный диапазон [{lo:.6}, {hi:.6}] Вт").unwrap()
            }
            None => writeln!(output, ", диапазон неопределённости не задан").unwrap(),
        }
    }
    Ok(output)
}
