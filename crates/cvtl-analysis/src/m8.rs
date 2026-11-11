// FILE: crates/cvtl-analysis/src/m8.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Estimate cooling of the A1 cold assembly from recorded intervals
//   SCOPE: Windowed energy balance, local derivatives and synthetic uncertainty
//   DEPENDS: M-CVTL-CORE, M-CVTL-PHYSICS
//   LINKS: M-CVTL-ANALYSIS
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   Interval - recorded interval and observation at its end
//   Model - parameters and provenance of the applied cold boundary
//   Options - requested analysis window and reproducible seed
//   Report - estimate, window evidence and identifiability status
//   analyze - cooling estimate with known capacities
//   analyze_pair - common cooling and unknown fixture capacity from two runs
// END_MODULE_MAP

mod uncertainty;
mod window;

use cvtl_core::units::{JoulesPerKelvin, KelvinPerSecond, Watts};
use cvtl_physics::{request, Bridge, EmulationMode};
use serde::Serialize;

pub const MAX_INTERVALS: usize = 5000;
pub const DRAWS: usize = 4096;
pub const DEFAULT_SEED: u64 = 20_261_111;

#[derive(Clone, Copy, Debug)]
pub struct Interval {
    pub begin_ns: u64,
    pub end_ns: u64,
    pub heater_energy_j: [f64; 2],
    /// Stage1, stage2, fixture and jacket at the end of the model step.
    pub end_k: Option<[f64; 4]>,
}

#[derive(Clone)]
pub struct AppliedWire {
    pub bridge: Bridge,
    /// Identity of the recorded geometry and the archived conductivity.
    pub reference: String,
}

#[derive(Clone)]
pub struct Model {
    pub capacity_stage2_j_per_k: f64,
    pub capacity_tf_j_per_k: f64,
    pub stage_link_w_per_k: f64,
    pub fixture_link_w_per_k: f64,
    pub initial_k: [f64; 4],
    pub other_w: Option<f64>,
    pub wire: Option<AppliedWire>,
    pub group: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub start_s: Option<f64>,
    pub window_s: f64,
    pub seed: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            start_s: None,
            window_s: 12.0,
            seed: DEFAULT_SEED,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Estimate {
    pub nominal: f64,
    pub std_dev: f64,
    pub p05: f64,
    pub median: f64,
    pub p95: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WindowReport {
    pub start_s: f64,
    pub end_s: f64,
    pub intervals: usize,
    /// Interval-weighted stage1, stage2, fixture and jacket temperatures.
    pub mean_temperatures_k: [f64; 4],
    pub mean_heater_w: f64,
    pub derivative_at_s: f64,
    pub derivative_stage2_k_per_s: Estimate,
    pub derivative_tf_k_per_s: Estimate,
    pub fixture_compensation_delta_w: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub status: &'static str,
    pub reason: Option<String>,
    pub profile: &'static str,
    pub seed: u64,
    pub draws: usize,
    pub rejected_draws: usize,
    pub window: Option<WindowReport>,
    pub paired_window: Option<WindowReport>,
    pub q2_eff_w: Option<Estimate>,
    pub identified_cf_j_per_k: Option<Estimate>,
}

impl Report {
    pub fn not_determined(seed: u64, reason: impl Into<String>) -> Self {
        Self {
            status: "не определяется",
            reason: Some(reason.into()),
            profile: "synthetic_test_only_a1",
            seed,
            draws: 0,
            rejected_draws: 0,
            window: None,
            paired_window: None,
            q2_eff_w: None,
            identified_cf_j_per_k: None,
        }
    }
}

pub fn analyze(model: &Model, intervals: &[Interval], options: Options) -> Report {
    calculate(model, intervals, None, options)
        .unwrap_or_else(|reason| Report::not_determined(options.seed, reason))
}

pub fn analyze_pair(
    first_model: &Model,
    first: &[Interval],
    second_model: &Model,
    second: &[Interval],
    options: Options,
) -> Report {
    calculate(first_model, first, Some((second_model, second)), options)
        .unwrap_or_else(|reason| Report::not_determined(options.seed, reason))
}

fn calculate(
    model: &Model,
    intervals: &[Interval],
    pair: Option<(&Model, &[Interval])>,
    options: Options,
) -> Result<Report, String> {
    validate(model, intervals, pair.is_none())?;
    let first = window::select(intervals, options)?;
    let second = if let Some((other, intervals)) = pair {
        validate(other, intervals, false)?;
        if !comparable(model, other) {
            return Err("Для пары нужны общая группа и сопоставимые исходные условия".into());
        }
        Some((other, window::select(intervals, options)?))
    } else {
        None
    };
    let paired = second.as_ref().map(|(other, window)| (*other, window));
    let result = uncertainty::estimate(model, &first, paired, options.seed)?;
    let cf = result
        .cf
        .map_or(model.capacity_tf_j_per_k, |value| value.nominal);
    let primary = summarize(&first, result.primary_derivatives, cf)?;
    let paired_window = match (paired, result.paired_derivatives) {
        (Some((_, window)), Some(derivatives)) => Some(summarize(window, derivatives, cf)?),
        _ => None,
    };
    Ok(Report {
        status: "определяется",
        reason: None,
        profile: "synthetic_test_only_a1",
        seed: options.seed,
        draws: DRAWS,
        rejected_draws: result.rejected_draws,
        window: Some(primary),
        paired_window,
        q2_eff_w: Some(result.q2),
        identified_cf_j_per_k: result.cf,
    })
}

fn validate(model: &Model, intervals: &[Interval], known_cf: bool) -> Result<(), String> {
    if intervals.len() > MAX_INTERVALS {
        return Err("Число тепловых интервалов превышает предел анализа".into());
    }
    let positive = |value: f64| value.is_finite() && value > 0.0;
    let nonnegative = |value: f64| value.is_finite() && value >= 0.0;
    if !positive(model.capacity_stage2_j_per_k)
        || (known_cf && !positive(model.capacity_tf_j_per_k))
        || !nonnegative(model.stage_link_w_per_k)
        || !nonnegative(model.fixture_link_w_per_k)
        || !model.initial_k.into_iter().all(positive)
    {
        return Err("Недопустимые параметры тепловой границы".into());
    }
    let other_w = model
        .other_w
        .ok_or("Q2 неразличим с неизмеренным постоянным притоком")?;
    if !other_w.is_finite()
        || model.wire.as_ref().is_some_and(|wire| {
            wire.reference.is_empty() || !matches!(wire.bridge, Bridge::Conductor { .. })
        })
    {
        return Err("Недопустимое описание применённых притоков".into());
    }
    Ok(())
}

fn comparable(first: &Model, second: &Model) -> bool {
    first.group.as_ref().is_some_and(|group| !group.is_empty())
        && first.group == second.group
        && first.initial_k == second.initial_k
        && first.capacity_stage2_j_per_k == second.capacity_stage2_j_per_k
        && first.stage_link_w_per_k == second.stage_link_w_per_k
        && first.fixture_link_w_per_k == second.fixture_link_w_per_k
        && first.other_w == second.other_w
        && first.wire.as_ref().map(|wire| &wire.reference)
            == second.wire.as_ref().map(|wire| &wire.reference)
}

fn summarize(
    window: &window::Window,
    [stage2, tf]: [Estimate; 2],
    cf: f64,
) -> Result<WindowReport, String> {
    let duration = window.durations_s.iter().sum::<f64>();
    let mut mean_temperatures_k = [0.0_f64; 4];
    for (&dt, temperatures) in window.durations_s.iter().zip(&window.temperatures_k) {
        for (mean, temperature) in mean_temperatures_k.iter_mut().zip(temperatures) {
            *mean += (dt / duration) * temperature;
        }
    }
    let mean_heater_w = window.energy_j.iter().flatten().sum::<f64>() / duration;
    let capacity = JoulesPerKelvin::new(cf).ok_or("Не представима теплоёмкость насадки")?;
    let temperature_rate =
        KelvinPerSecond::new(tf.nominal).ok_or("Не представима производная tf")?;
    let zero = Watts::new(0.0).unwrap();
    let compensation = request(
        EmulationMode::FixtureCompensation {
            capacity,
            temperature_rate,
            parasitic: zero,
        },
        zero,
    )?;
    if !mean_heater_w.is_finite() || !mean_temperatures_k.iter().all(|value| value.is_finite()) {
        return Err("Средние значения теплового окна вне численного диапазона".into());
    }
    Ok(WindowReport {
        start_s: window.start_s,
        end_s: window.end_s,
        intervals: window.energy_j.len(),
        mean_temperatures_k,
        mean_heater_w,
        derivative_at_s: window.seconds[0].midpoint(*window.seconds.last().unwrap()),
        derivative_stage2_k_per_s: stage2,
        derivative_tf_k_per_s: tf,
        fixture_compensation_delta_w: compensation.requested.get(),
    })
}
