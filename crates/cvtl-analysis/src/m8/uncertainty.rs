// FILE: crates/cvtl-analysis/src/m8/uncertainty.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Evaluate the cold boundary and propagate synthetic uncertainties
//   SCOPE: Interval energy, first-order gradients, seeded trials and paired inference
//   DEPENDS: M-CVTL-CORE, M-CVTL-PHYSICS
//   LINKS: M-CVTL-ANALYSIS
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   Calculation - uncertainty estimates for cooling, capacity and slopes
//   estimate - calculate known-capacity or paired results
// END_MODULE_MAP

use cvtl_core::units::Kelvin;
use cvtl_physics::Bridge;

use super::{window::Window, Estimate, Model, DRAWS};

const MASS_WIDTH: f64 = 0.01;
const CP_WIDTH: f64 = 0.015;
const LINK_WIDTH: f64 = 0.02;
const ENERGY_WIDTH: f64 = 0.005;
const LENGTH_WIDTH: f64 = 0.02;
const OFFSET_WIDTH_K: f64 = 0.002;
const NOISE_WIDTH_K: f64 = 0.0005;
const OFFSET_VARIANCE: f64 = OFFSET_WIDTH_K * OFFSET_WIDTH_K / 3.0;
const NOISE_VARIANCE: f64 = NOISE_WIDTH_K * NOISE_WIDTH_K / 3.0;
const MIN_POWER_DIFFERENCE_W: f64 = 0.05;

pub(super) struct Calculation {
    pub(super) q2: Estimate,
    pub(super) cf: Option<Estimate>,
    pub(super) primary_derivatives: [Estimate; 2],
    pub(super) paired_derivatives: Option<[Estimate; 2]>,
    pub(super) rejected_draws: usize,
}

struct Balance<'a> {
    model: &'a Model,
    window: &'a Window,
    width_s: f64,
    heater_w: f64,
    other_w: f64,
}

#[derive(Clone, Copy)]
struct Observations {
    stage_gap_k: f64,
    wire_w: f64,
    endpoint_slopes: [f64; 2],
    fitted_slopes: [f64; 2],
}

impl<'a> Balance<'a> {
    fn new(model: &'a Model, window: &'a Window) -> Result<Self, String> {
        let width_s = window.durations_s.iter().sum::<f64>();
        let heater_w = window.energy_j.iter().flatten().sum::<f64>() / width_s;
        if !width_s.is_finite() || width_s <= 0.0 || !heater_w.is_finite() {
            return Err("Численный диапазон теплового окна превышен".into());
        }
        Ok(Self {
            model,
            window,
            width_s,
            heater_w,
            other_w: model.other_w.ok_or("Неизмеренный холодный приток")?,
        })
    }

    fn observe(&self, temperatures: &[[f64; 4]]) -> Result<Observations, String> {
        if temperatures
            .iter()
            .flatten()
            .any(|value| !value.is_finite() || *value <= 0.0)
        {
            return Err("Непригодная температура теплового окна".into());
        }
        let mut stage_gap_k = 0.0;
        let mut wire_w = 0.0;
        for (&dt, point) in self.window.durations_s.iter().zip(temperatures) {
            let weight = dt / self.width_s;
            stage_gap_k += weight * (point[0] - point[1]);
            if let Some(wire) = &self.model.wire {
                let [stage1, stage2] = stage_temperatures(point)?;
                wire_w += weight * wire.bridge.flow(stage1, stage2)?.get();
            }
        }
        let first = temperatures[0];
        let last = temperatures[temperatures.len() - 1];
        let endpoint_slopes = [
            (last[1] - first[1]) / self.width_s,
            (last[2] - first[2]) / self.width_s,
        ];
        let mut fitted_slopes = [0.0; 2];
        for (&weight, point) in self.window.derivative_weights.iter().zip(temperatures) {
            fitted_slopes[0] += weight * (point[1] - first[1]);
            fitted_slopes[1] += weight * (point[2] - first[2]);
        }
        Ok(Observations {
            stage_gap_k,
            wire_w,
            endpoint_slopes,
            fitted_slopes,
        })
    }

    fn cooling(&self, observed: Observations, factors: Factors, include_cf: bool) -> f64 {
        let mut q2 = factors.energy * self.heater_w
            + factors.link * self.model.stage_link_w_per_k * observed.stage_gap_k
            + observed.wire_w / factors.wire_length
            + self.other_w
            - factors.c2 * self.model.capacity_stage2_j_per_k * observed.endpoint_slopes[0];
        if include_cf {
            q2 -= factors.cf * self.model.capacity_tf_j_per_k * observed.endpoint_slopes[1];
        }
        q2
    }

    fn first_order(&self, nominal: Observations, include_cf: bool) -> Result<Gradient, String> {
        let c2 = self.model.capacity_stage2_j_per_k;
        let cf = if include_cf {
            self.model.capacity_tf_j_per_k
        } else {
            0.0
        };
        let g12 = self.model.stage_link_w_per_k;
        let temperatures = &self.window.temperatures_k;
        let mut coefficients = vec![[0.0; 3]; temperatures.len()];
        for ((&dt, point), coefficient) in self
            .window
            .durations_s
            .iter()
            .zip(temperatures)
            .zip(&mut coefficients)
        {
            let weight = dt / self.width_s;
            coefficient[0] = weight * g12;
            coefficient[1] = -weight * g12;
            if let Some(wire) = &self.model.wire {
                let Bridge::Conductor {
                    conductivity,
                    area,
                    length,
                } = &wire.bridge
                else {
                    return Err("Применённая проводка должна быть проводником".into());
                };
                let [stage1, stage2] = stage_temperatures(point)?;
                let conductivity_at = |temperature| {
                    conductivity
                        .value(temperature)
                        .map(|value| value.get())
                        .map_err(|_| "Температура проводки вне диапазона архивного k(T)")
                };
                let geometry = weight * area.get() / length.get();
                coefficient[0] += geometry * conductivity_at(stage1)?;
                coefficient[1] -= geometry * conductivity_at(stage2)?;
            }
        }
        // A single reading may contribute to both conduction and storage.
        // Combine its signed coefficients before computing its noise variance.
        coefficients[0][1] += c2 / self.width_s;
        coefficients[0][2] += cf / self.width_s;
        let last = coefficients.last_mut().unwrap();
        last[1] -= c2 / self.width_s;
        last[2] -= cf / self.width_s;

        let mut channel_sum = [0.0; 3];
        let mut noise_norm = 0.0;
        for row in coefficients {
            for (sum, coefficient) in channel_sum.iter_mut().zip(row) {
                *sum += coefficient;
                noise_norm += coefficient * coefficient;
            }
        }
        let read_variance = NOISE_VARIANCE * noise_norm
            + OFFSET_VARIANCE * channel_sum.into_iter().map(|sum| sum * sum).sum::<f64>();
        let stage2_storage = -c2 * nominal.endpoint_slopes[0];
        let fixture_storage = -cf * nominal.endpoint_slopes[1];
        Ok(Gradient {
            // Relative changes of mass and cp are independent primitives.
            parameters: [
                stage2_storage * MASS_WIDTH,
                stage2_storage * CP_WIDTH,
                fixture_storage * MASS_WIDTH,
                fixture_storage * CP_WIDTH,
                g12 * nominal.stage_gap_k * LINK_WIDTH,
                self.heater_w * ENERGY_WIDTH,
                -nominal.wire_w * LENGTH_WIDTH,
            ],
            read_variance,
        })
    }

    fn tf_endpoint_variance(&self) -> f64 {
        2.0 * NOISE_VARIANCE / self.width_s.powi(2)
    }
}

fn stage_temperatures(point: &[f64; 4]) -> Result<[Kelvin; 2], String> {
    let invalid = || "Температура проводки вне численного диапазона".to_owned();
    Ok([
        Kelvin::new(point[0]).ok_or_else(invalid)?,
        Kelvin::new(point[1]).ok_or_else(invalid)?,
    ])
}

struct Gradient {
    parameters: [f64; 7],
    read_variance: f64,
}

impl Gradient {
    fn variance(&self) -> f64 {
        self.read_variance + self.parameters.iter().map(|s| s * s).sum::<f64>() / 3.0
    }

    fn pair_variance(&self, other: &Self, a: f64, b: f64) -> f64 {
        // The model and power calibration are common to the declared pair.
        // Their covariance is preserved by adding coefficients first.
        let common = self
            .parameters
            .into_iter()
            .zip(other.parameters)
            .map(|(x, y)| (a * x + b * y).powi(2))
            .sum::<f64>()
            / 3.0;
        common + a * a * self.read_variance + b * b * other.read_variance
    }
}

fn fitted_slope_std_dev(weights: &[f64]) -> f64 {
    // The fit is evaluated on T_i - T_0. A common offset cancels, and the
    // coefficient of T_0 is the negative sum of the other coefficients.
    let rest = &weights[1..];
    let norm = rest.iter().map(|weight| weight * weight).sum::<f64>();
    (NOISE_VARIANCE * (norm + rest.iter().sum::<f64>().powi(2))).sqrt()
}

#[derive(Clone, Copy)]
struct Factors {
    c2: f64,
    cf: f64,
    link: f64,
    energy: f64,
    wire_length: f64,
}

impl Factors {
    const NOMINAL: Self = Self {
        c2: 1.0,
        cf: 1.0,
        link: 1.0,
        energy: 1.0,
        wire_length: 1.0,
    };

    fn sample(rng: &mut SplitMix64) -> Self {
        Self {
            c2: (1.0 + rng.uniform(MASS_WIDTH)) * (1.0 + rng.uniform(CP_WIDTH)),
            cf: (1.0 + rng.uniform(MASS_WIDTH)) * (1.0 + rng.uniform(CP_WIDTH)),
            link: 1.0 + rng.uniform(LINK_WIDTH),
            energy: 1.0 + rng.uniform(ENERGY_WIDTH),
            wire_length: 1.0 + rng.uniform(LENGTH_WIDTH),
        }
    }
}

#[derive(Clone, Copy)]
struct Draw {
    q2: f64,
    cf: Option<f64>,
    primary_derivatives: [f64; 2],
    paired_derivatives: Option<[f64; 2]>,
}

fn evaluate(
    first: &Balance<'_>,
    first_obs: Observations,
    pair: Option<(&Balance<'_>, Observations)>,
    factors: Factors,
    slope_floor: f64,
) -> Result<Draw, String> {
    let y0 = first.cooling(first_obs, factors, pair.is_none());
    let (q2, cf, paired_derivatives) = if let Some((second, second_obs)) = pair {
        if (factors.energy * (second.heater_w - first.heater_w)).abs() < MIN_POWER_DIFFERENCE_W {
            return Err("Мощности в паре различаются менее чем на 0,05 Вт".into());
        }
        let y1 = second.cooling(second_obs, factors, false);
        let s0 = first_obs.endpoint_slopes[1];
        let difference = second_obs.endpoint_slopes[1] - s0;
        if difference.abs() <= slope_floor {
            return Err("Наклоны tf в паре неразличимы".into());
        }
        let cf = (y1 - y0) / difference;
        if !cf.is_finite() || cf <= 0.0 {
            return Err(
                "Парная оценка теплоёмкости насадки неположительна или не представима".into(),
            );
        }
        (y0 - cf * s0, Some(cf), Some(second_obs.fitted_slopes))
    } else {
        (y0, None, None)
    };
    if !q2.is_finite()
        || !first_obs.fitted_slopes.into_iter().all(f64::is_finite)
        || paired_derivatives.is_some_and(|slopes| !slopes.into_iter().all(f64::is_finite))
    {
        return Err("Результат теплового расчёта вне численного диапазона".into());
    }
    Ok(Draw {
        q2,
        cf,
        primary_derivatives: first_obs.fitted_slopes,
        paired_derivatives,
    })
}

fn pair_std_dev(
    first: &Balance<'_>,
    first_obs: Observations,
    second: &Balance<'_>,
    second_obs: Observations,
    cf: f64,
) -> Result<(f64, f64), String> {
    let a = first.first_order(first_obs, false)?;
    let b = second.first_order(second_obs, false)?;
    let s0 = first_obs.endpoint_slopes[1];
    let d = second_obs.endpoint_slopes[1] - s0;
    let s0_variance = first.tf_endpoint_variance();
    let s1_variance = second.tf_endpoint_variance();

    // y contains stage1 and stage2 only, while s contains tf only.
    let cf_variance =
        a.pair_variance(&b, -1.0 / d, 1.0 / d) + (cf / d).powi(2) * (s0_variance + s1_variance);
    let q_variance = a.pair_variance(&b, 1.0 + s0 / d, -s0 / d)
        + (cf * (1.0 + s0 / d)).powi(2) * s0_variance
        + (cf * s0 / d).powi(2) * s1_variance;
    Ok((q_variance.sqrt(), cf_variance.sqrt()))
}

pub(super) fn estimate(
    model: &Model,
    window: &Window,
    pair: Option<(&Model, &Window)>,
    seed: u64,
) -> Result<Calculation, String> {
    let first = Balance::new(model, window)?;
    let second = pair
        .map(|(model, window)| Balance::new(model, window))
        .transpose()?;
    let primary_obs = first.observe(&window.temperatures_k)?;
    let pair_obs = second
        .as_ref()
        .map(|balance| {
            balance
                .observe(&balance.window.temperatures_k)
                .map(|observations| (balance, observations))
        })
        .transpose()?;
    let slope_floor = second.as_ref().map_or(0.0, |balance| {
        (3.0 * (first.tf_endpoint_variance() + balance.tf_endpoint_variance()).sqrt()).max(1e-6)
    });
    let nominal = evaluate(&first, primary_obs, pair_obs, Factors::NOMINAL, slope_floor)?;
    let (q_std_dev, cf_std_dev) =
        if let (Some((second, second_obs)), Some(cf)) = (pair_obs, nominal.cf) {
            let (q, c) = pair_std_dev(&first, primary_obs, second, second_obs, cf)?;
            (q, Some(c))
        } else {
            (
                first.first_order(primary_obs, true)?.variance().sqrt(),
                None,
            )
        };

    let draws = trials(&first, second.as_ref(), slope_floor, seed)?;
    let rejected_draws = DRAWS - draws.len();
    let q2 = summarize(nominal.q2, q_std_dev, draws.iter().map(|draw| draw.q2))?;
    let cf = nominal
        .cf
        .zip(cf_std_dev)
        .map(|(nominal, std_dev)| {
            summarize(nominal, std_dev, draws.iter().filter_map(|draw| draw.cf))
        })
        .transpose()?;
    let primary_derivatives =
        derivative_estimates(window, nominal.primary_derivatives, &draws, false)?;
    let paired_derivatives = second
        .zip(nominal.paired_derivatives)
        .map(|(balance, nominal)| derivative_estimates(balance.window, nominal, &draws, true))
        .transpose()?;
    Ok(Calculation {
        q2,
        cf,
        primary_derivatives,
        paired_derivatives,
        rejected_draws,
    })
}

fn trials(
    first: &Balance<'_>,
    second: Option<&Balance<'_>>,
    slope_floor: f64,
    seed: u64,
) -> Result<Vec<Draw>, String> {
    let mut rng = SplitMix64(seed);
    let mut primary = Vec::with_capacity(first.window.temperatures_k.len());
    let mut paired = Vec::with_capacity(second.map_or(0, |b| b.window.temperatures_k.len()));
    let mut draws = Vec::with_capacity(DRAWS);
    for _ in 0..DRAWS {
        let factors = Factors::sample(&mut rng);
        perturb_temperatures(&first.window.temperatures_k, &mut primary, &mut rng);
        if let Some(second) = second {
            perturb_temperatures(&second.window.temperatures_k, &mut paired, &mut rng);
        }
        let result = (|| {
            let one = first.observe(&primary)?;
            let two = second
                .map(|balance| {
                    balance
                        .observe(&paired)
                        .map(|observations| (balance, observations))
                })
                .transpose()?;
            evaluate(first, one, two, factors, slope_floor)
        })();
        if let Ok(draw) = result {
            draws.push(draw);
        }
    }
    let rejected = DRAWS - draws.len();
    if rejected * 100 > DRAWS {
        return Err(format!(
            "Доля непригодных реализаций Монте-Карло превышает 1 % ({rejected} из {DRAWS})"
        ));
    }
    Ok(draws)
}

fn perturb_temperatures(original: &[[f64; 4]], result: &mut Vec<[f64; 4]>, rng: &mut SplitMix64) {
    let offsets = [0.0; 4].map(|_| rng.uniform(OFFSET_WIDTH_K));
    result.clear();
    result.extend(original.iter().map(|point| {
        std::array::from_fn(|channel| {
            point[channel] + offsets[channel] + rng.uniform(NOISE_WIDTH_K)
        })
    }));
}

fn derivative_estimates(
    window: &Window,
    nominal: [f64; 2],
    draws: &[Draw],
    paired: bool,
) -> Result<[Estimate; 2], String> {
    let std_dev = fitted_slope_std_dev(&window.derivative_weights);
    let channel = |index: usize| {
        summarize(
            nominal[index],
            std_dev,
            draws.iter().filter_map(|draw| {
                if paired {
                    draw.paired_derivatives.map(|slopes| slopes[index])
                } else {
                    Some(draw.primary_derivatives[index])
                }
            }),
        )
    };
    Ok([channel(0)?, channel(1)?])
}

fn summarize(
    nominal: f64,
    std_dev: f64,
    draws: impl Iterator<Item = f64>,
) -> Result<Estimate, String> {
    let mut ordered = draws.collect::<Vec<_>>();
    if !nominal.is_finite()
        || !std_dev.is_finite()
        || ordered.is_empty()
        || !ordered.iter().all(|sample| sample.is_finite())
    {
        return Err("Не представима оценка неопределённости".into());
    }
    ordered.sort_unstable_by(f64::total_cmp);
    let quantile = |p: f64| {
        let at = p * (ordered.len() - 1) as f64;
        let lower = at.floor() as usize;
        let upper = at.ceil() as usize;
        let fraction = at - lower as f64;
        ordered[lower] * (1.0 - fraction) + ordered[upper] * fraction
    };
    Ok(Estimate {
        nominal,
        std_dev,
        p05: quantile(0.05),
        median: quantile(0.5),
        p95: quantile(0.95),
    })
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn uniform(&mut self, half_width: f64) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = self.0;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        mixed ^= mixed >> 31;
        let unit = (mixed >> 11) as f64 / (1_u64 << 53) as f64;
        (2.0 * unit - 1.0) * half_width
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affine_uniform_oracle_has_expected_seeded_quantiles() {
        let calculate = || {
            let mut rng = SplitMix64(20_261_111);
            let gradient = Gradient {
                parameters: [0.2, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                read_variance: 0.0,
            };
            summarize(
                4.0,
                gradient.variance().sqrt(),
                (0..DRAWS).map(|_| 4.0 + rng.uniform(0.2)),
            )
            .unwrap()
        };
        let estimate = calculate();
        assert!((estimate.std_dev - 0.2 / 3.0_f64.sqrt()).abs() < 1e-14);
        for (actual, expected) in [estimate.p05, estimate.median, estimate.p95]
            .into_iter()
            .zip([3.82, 4.0, 4.18])
        {
            assert!((actual - expected).abs() <= 0.02);
        }
        assert_eq!(estimate, calculate());
    }

    #[test]
    fn paired_common_primitives_cancel_for_equal_and_opposite_sensitivity() {
        let gradient = Gradient {
            parameters: [0.2, -0.1, 0.0, 0.0, 0.0, 0.0, 0.0],
            read_variance: 0.03,
        };
        assert!((gradient.pair_variance(&gradient, 1.0, -1.0) - 0.06).abs() < 1e-15);
    }

    #[test]
    fn paired_oracle_requires_distinct_slopes_even_with_distinct_heater_power() {
        let model = Model {
            capacity_stage2_j_per_k: 20.0,
            capacity_tf_j_per_k: f64::NAN,
            stage_link_w_per_k: 0.05,
            fixture_link_w_per_k: 0.5,
            initial_k: [45.0, 15.0, 16.0, 77.0],
            other_w: Some(0.0),
            wire: None,
            group: Some("oracle".into()),
        };
        let window = |heater_w, tf_slope| Window {
            start_s: 0.0,
            end_s: 12.0,
            seconds: vec![0.0, 12.0],
            temperatures_k: vec![
                [45.0, 15.0, 16.0, 77.0],
                [45.0, 15.0, 16.0 + 12.0 * tf_slope, 77.0],
            ],
            durations_s: vec![12.0],
            energy_j: vec![[12.0 * heater_w, 0.0]],
            derivative_weights: vec![-1.0 / 12.0, 1.0 / 12.0],
        };
        let first = window(1.5, -0.2);
        let second = window(3.5, 0.2);
        let calculated = estimate(&model, &first, Some((&model, &second)), 20_261_111).unwrap();
        assert!((calculated.q2.nominal - 4.0).abs() < 1e-12);
        assert!((calculated.cf.unwrap().nominal - 5.0).abs() < 1e-12);

        let equal_slope = window(3.5, -0.2);
        let failure = estimate(&model, &first, Some((&model, &equal_slope)), 20_261_111)
            .err()
            .unwrap();
        assert!(failure.contains("Наклоны"));
    }
}
