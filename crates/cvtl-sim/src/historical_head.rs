// FILE: crates/cvtl-sim/src/historical_head.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Calculate the historical Stage I characteristic for a test-only scenario
//   SCOPE: Canonical anchors, interpolation, synthetic continuation and provenance
//   DEPENDS: M-CVTL-CORE
//   LINKS: M-CVTL-SIM
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   HistoricalHead - closed-range characteristic of the nominal historical branch
//   report - reproducible set of historical, interpolated and synthetic points
// END_MODULE_MAP

use cvtl_core::units::Kelvin;
use serde::Serialize;
use sha2::{Digest, Sha256};

const SOURCE_PATH: &str = "data/machines/kde420sa_stage1_data.csv";
const SOURCE: &str = include_str!("../../../data/machines/kde420sa_stage1_data.csv");
const UPPER_K: f64 = 300.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoricalOrigin {
    HistoricalAnchor,
    Interpolated,
    Synthetic,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HistoricalPoint {
    pub stage2_k: f64,
    pub p_net_w: f64,
    pub stage1_k: f64,
    pub origin: HistoricalOrigin,
    pub source_row: Option<u8>,
}

#[derive(Debug, Serialize)]
pub struct HistoricalReport {
    pub synthetic: bool,
    pub test_only: bool,
    pub source_path: &'static str,
    pub source_sha256: String,
    pub nominal_rows: [u8; 6],
    pub liquid_nitrogen: bool,
    pub radiation_shield: bool,
    pub parasitics_removed: bool,
    pub continuation_from_k: f64,
    pub continuation_to_k: f64,
    pub power_slope_w_per_k: f64,
    pub stage_gap_at_start_k: f64,
    pub points: Vec<HistoricalPoint>,
}

pub struct HistoricalHead {
    anchors: [HistoricalPoint; 6],
}

impl HistoricalHead {
    pub fn new_test_only() -> Result<Self, &'static str> {
        Ok(Self {
            anchors: parse_nominal(SOURCE)?,
        })
    }

    pub fn at(&self, stage2: Kelvin) -> Result<HistoricalPoint, &'static str> {
        let stage2_k = stage2.get();
        if !(self.anchors[0].stage2_k..=UPPER_K).contains(&stage2_k) {
            return Err("Температура stage2 вне диапазона исторического сценария");
        }
        let upper = self
            .anchors
            .partition_point(|point| point.stage2_k < stage2_k);
        if let Some(right) = self.anchors.get(upper) {
            if right.stage2_k == stage2_k {
                return Ok(*right);
            }
            let left = self.anchors[upper - 1];
            let fraction = (stage2_k - left.stage2_k) / (right.stage2_k - left.stage2_k);
            return Ok(HistoricalPoint {
                stage2_k,
                p_net_w: left.p_net_w + fraction * (right.p_net_w - left.p_net_w),
                stage1_k: left.stage1_k + fraction * (right.stage1_k - left.stage1_k),
                origin: HistoricalOrigin::Interpolated,
                source_row: None,
            });
        }

        let last = self.anchors[5];
        let delta_k = stage2_k - last.stage2_k;
        let fraction = delta_k / (UPPER_K - last.stage2_k);
        Ok(HistoricalPoint {
            stage2_k,
            p_net_w: last.p_net_w + delta_k * self.last_power_slope(),
            stage1_k: stage2_k + (last.stage1_k - last.stage2_k) * (1.0 - fraction),
            origin: HistoricalOrigin::Synthetic,
            source_row: None,
        })
    }

    fn last_power_slope(&self) -> f64 {
        power_slope(self.anchors[4], self.anchors[5])
    }
}

pub fn report() -> Result<HistoricalReport, &'static str> {
    let model = HistoricalHead::new_test_only()?;
    let last = model.anchors[5];
    let mut points = Vec::with_capacity(15);
    for pair in model.anchors.windows(2) {
        points.push(pair[0]);
        let midpoint = (pair[0].stage2_k + pair[1].stage2_k) / 2.0;
        points.push(model.at(Kelvin::new(midpoint).expect("bounded temperatures"))?);
    }
    points.push(last);
    for stage2 in [50.0, 100.0, 200.0, UPPER_K] {
        points.push(model.at(Kelvin::new(stage2).expect("finite sample temperature"))?);
    }
    Ok(HistoricalReport {
        synthetic: true,
        test_only: true,
        source_path: SOURCE_PATH,
        source_sha256: format!("{:x}", Sha256::digest(SOURCE.as_bytes())),
        nominal_rows: [1, 2, 3, 4, 5, 6],
        liquid_nitrogen: false,
        radiation_shield: false,
        parasitics_removed: false,
        continuation_from_k: last.stage2_k,
        continuation_to_k: UPPER_K,
        power_slope_w_per_k: model.last_power_slope(),
        stage_gap_at_start_k: last.stage1_k - last.stage2_k,
        points,
    })
}

fn power_slope(left: HistoricalPoint, right: HistoricalPoint) -> f64 {
    (right.p_net_w - left.p_net_w) / (right.stage2_k - left.stage2_k)
}

fn parse_nominal(source: &str) -> Result<[HistoricalPoint; 6], &'static str> {
    let mut lines = source.lines();
    if lines.next() != Some("stage2_k,p_net_w,stage1_k") {
        return Err("Неверные заголовки канонического CSV этапа I");
    }
    let mut anchors = Vec::with_capacity(6);
    for row in 1..=6 {
        let line = lines.next().ok_or("В каноническом CSV нужны шесть строк")?;
        let mut fields = line.split(',');
        let mut number = || {
            fields
                .next()
                .and_then(|field| field.parse::<f64>().ok())
                .filter(|value| value.is_finite())
                .ok_or("Неверное число в каноническом CSV этапа I")
        };
        let point = HistoricalPoint {
            stage2_k: number()?,
            p_net_w: number()?,
            stage1_k: number()?,
            origin: HistoricalOrigin::HistoricalAnchor,
            source_row: Some(row),
        };
        if fields.next().is_some() {
            return Err("В строке канонического CSV нужны три числа");
        }
        if point.stage2_k <= 0.0 || point.stage1_k < point.stage2_k || point.p_net_w < 0.0 {
            return Err("Некорректная опора исторической характеристики");
        }
        if anchors.last().is_some_and(|previous: &HistoricalPoint| {
            previous.stage2_k >= point.stage2_k
                || previous.stage1_k > point.stage1_k
                || previous.p_net_w > point.p_net_w
        }) {
            return Err("Опоры исторической характеристики должны возрастать");
        }
        anchors.push(point);
    }
    if lines.next().is_some() {
        return Err("После шестой опоры есть лишняя строка");
    }

    let last = anchors[5];
    if last.stage2_k >= UPPER_K || last.stage1_k > UPPER_K {
        return Err("Последняя опора вне границы синтетического продолжения");
    }
    let endpoint_power = last.p_net_w + (UPPER_K - last.stage2_k) * power_slope(anchors[4], last);
    if !endpoint_power.is_finite() {
        return Err("Синтетическое продолжение мощности вне численного диапазона");
    }
    anchors
        .try_into()
        .map_err(|_| "В каноническом CSV нужны шесть строк")
}

#[cfg(test)]
mod tests {
    use super::{parse_nominal, SOURCE};

    #[test]
    fn invalid_nominal_data_is_rejected() {
        for invalid in [
            SOURCE.replacen("p_net_w", "power", 1),
            SOURCE.replacen("23.4,22,43.8\n", "", 1),
            format!("{SOURCE}41.3,34.1,39.8\n"),
            SOURCE.replacen("4.95,0,28.45", "4.95,0,28.45,7", 1),
            SOURCE.replacen("4.95,0,28.45", "4.95,NaN,28.45", 1),
            SOURCE.replacen("4.95,0,28.45", "0,0,28.45", 1),
            SOURCE.replacen("4.95,0,28.45", "4.95,-1,28.45", 1),
            SOURCE.replacen("4.95,0,28.45", "4.95,0,4", 1),
            SOURCE.replacen("6.1,0.63,28.5", "4.95,0.63,28.5", 1),
            SOURCE.replacen("6.1,0.63,28.5", "6.1,0.63,28", 1),
            SOURCE.replacen("7.14,1.31,28.6", "7.14,0.5,28.6", 1),
            SOURCE.replacen("23.4,22,43.8", "300,22,300", 1),
            SOURCE.replacen("23.4,22,43.8", "23.4,1.7e308,43.8", 1),
        ] {
            assert!(parse_nominal(&invalid).is_err(), "{invalid}");
        }
    }
}
