// FILE: crates/cvtl-analysis/src/stage_i.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Parse and project Stage I historical points
//   SCOPE: Fixed local CSV schema, units, stage mapping and nominal projection
//   DEPENDS: M-CVTL-CORE
//   LINKS: M-CVTL-ANALYSIS
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   StageI - validated table and nominal projection
//   Point - stationarity, typed temperatures and published power
//   Electrical - typed branch readings and calculated power
//   ImportError - line-specific input failure
// END_MODULE_MAP
//! Fixed-schema import of the published Stage I transcription.

use std::fmt::{self, Write};

use cvtl_core::units::{Amperes, Kelvin, Volts, Watts};

pub const MAX_LEGACY_BYTES: usize = 16 * 1024;
const HEADER: &str = "row,I1_A,U1_V,I2_A,U2_V,T1_K,T2_K,P_published_W,T2_questioned";
const ROWS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Electrical {
    pub current: Option<Amperes>,
    pub voltage: Volts,
    pub calculated_power: Option<Watts>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub number: u8,
    pub stage1: Kelvin,
    pub stage2: Kelvin,
    pub stage1_questioned: bool,
    pub branches: [Electrical; 2],
    pub published_power: Watts,
    pub calculated_power: Option<Watts>,
}

impl Point {
    pub fn inverted(&self) -> bool {
        self.stage2 > self.stage1
    }

    pub fn nominal_anchor(&self) -> bool {
        (1..=6).contains(&self.number) && !self.stage1_questioned && !self.inverted()
    }
}

pub struct StageI {
    points: [Point; ROWS],
}

impl StageI {
    pub fn from_legacy_csv(input: &str) -> Result<Self, ImportError> {
        if input.len() > MAX_LEGACY_BYTES {
            return Err(ImportError::new(1, "таблица превышает 16 KiB"));
        }
        let mut lines = input.lines();
        if lines.next() != Some(HEADER) {
            return Err(ImportError::new(1, "неверные заголовки таблицы этапа I"));
        }

        let mut points = Vec::with_capacity(ROWS);
        for number in 1..=ROWS as u8 {
            let line = usize::from(number) + 1;
            let row = lines
                .next()
                .ok_or_else(|| ImportError::new(line, "нет строки этапа I"))?;
            points.push(parse_row(row, number, line)?);
        }
        if lines.next().is_some() {
            return Err(ImportError::new(
                ROWS + 2,
                "после восьмой точки есть лишняя строка",
            ));
        }
        let points = points
            .try_into()
            .map_err(|_| ImportError::new(1, "ожидаются восемь точек"))?;
        Ok(Self { points })
    }

    pub fn points(&self) -> &[Point; ROWS] {
        &self.points
    }

    pub fn nominal_points(&self) -> impl Iterator<Item = &Point> {
        self.points.iter().filter(|p| p.nominal_anchor())
    }

    pub fn nominal_csv(&self) -> String {
        let mut output = String::from("stage2_k,p_net_w,stage1_k\n");
        for point in self.nominal_points() {
            writeln!(
                output,
                "{},{},{}",
                point.stage2.get(),
                point.published_power.get(),
                point.stage1.get()
            )
            .expect("write to String");
        }
        output
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportError {
    line: usize,
    reason: &'static str,
}

impl ImportError {
    fn new(line: usize, reason: &'static str) -> Self {
        Self { line, reason }
    }
}

impl fmt::Display for ImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Строка {} — {}", self.line, self.reason)
    }
}

impl std::error::Error for ImportError {}

fn parse_row(input: &str, number: u8, line: usize) -> Result<Point, ImportError> {
    let columns: [&str; 9] = input
        .split(',')
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| ImportError::new(line, "ожидаются девять колонок"))?;
    let [id, current_a, voltage_a, current_b, voltage_b, legacy_t1_k, legacy_t2_k, published, questioned] =
        columns;
    if id.parse::<u8>() != Ok(number) {
        return Err(ImportError::new(
            line,
            "номер точки повторён или нарушен порядок",
        ));
    }
    let branches = [
        electrical(current_a, voltage_a, line)?,
        electrical(current_b, voltage_b, line)?,
    ];
    let calculated_power = match branches.map(|branch| branch.calculated_power) {
        [Some(a), Some(b)] => Some(
            Watts::new(a.get() + b.get())
                .ok_or_else(|| ImportError::new(line, "суммарная мощность не конечна"))?,
        ),
        _ => None,
    };
    Ok(Point {
        number,
        // The historical labels are inverted in the source (TZ §2.2).
        stage1: quantity(
            legacy_t2_k,
            Kelvin::new,
            line,
            "некорректная температура stage1",
        )?,
        stage2: quantity(
            legacy_t1_k,
            Kelvin::new,
            line,
            "некорректная температура stage2",
        )?,
        stage1_questioned: questioned
            .parse()
            .map_err(|_| ImportError::new(line, "ожидается признак сомнения true или false"))?,
        branches,
        published_power: quantity(
            published,
            Watts::new,
            line,
            "некорректная опубликованная мощность",
        )?,
        calculated_power,
    })
}

fn electrical(current: &str, voltage: &str, line: usize) -> Result<Electrical, ImportError> {
    let current = if current.is_empty() {
        None
    } else {
        Some(quantity(current, Amperes::new, line, "некорректный ток")?)
    };
    let voltage = quantity(voltage, Volts::new, line, "некорректное напряжение")?;
    let calculated_power = current
        .map(|a| {
            Watts::new(a.get() * voltage.get())
                .ok_or_else(|| ImportError::new(line, "мощность ветви не конечна"))
        })
        .transpose()?;
    Ok(Electrical {
        current,
        voltage,
        calculated_power,
    })
}

fn quantity<T>(
    text: &str,
    unit: impl FnOnce(f64) -> Option<T>,
    line: usize,
    reason: &'static str,
) -> Result<T, ImportError> {
    text.parse()
        .ok()
        .and_then(unit)
        .ok_or_else(|| ImportError::new(line, reason))
}
