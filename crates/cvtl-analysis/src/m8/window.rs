// FILE: crates/cvtl-analysis/src/m8/window.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Select complete thermal intervals and fit a local derivative
//   SCOPE: Model-clock bounds, canonical readings and quadratic slope weights
//   DEPENDS: M-CVTL-ANALYSIS
//   LINKS: M-CVTL-ANALYSIS
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   Window - aligned endpoints, heater energies and derivative weights
//   select - bounded contiguous window selection on model time
// END_MODULE_MAP

use std::time::Duration;

use super::{Interval, Options};

const MAX_GAP_NS: u64 = 500_000_000;
const MIN_POINTS: usize = 5;

pub(super) struct Window {
    pub(super) start_s: f64,
    pub(super) end_s: f64,
    pub(super) seconds: Vec<f64>,
    pub(super) temperatures_k: Vec<[f64; 4]>,
    pub(super) durations_s: Vec<f64>,
    pub(super) energy_j: Vec<[f64; 2]>,
    pub(super) derivative_weights: Vec<f64>,
}

pub(super) fn select(intervals: &[Interval], options: Options) -> Result<Window, String> {
    if !(10.0..=30.0).contains(&options.window_s) {
        return Err("Длительность окна должна быть от 10 до 30 с".into());
    }
    let first = intervals.first().ok_or("Нет тепловых интервалов")?;
    let last = intervals.last().ok_or("Нет тепловых интервалов")?;
    let origin = first.begin_ns;
    let width = i128::from(nanoseconds(options.window_s)?);
    let (requested_start, requested_end) = match options.start_s {
        Some(start_s) => {
            let start = i128::from(origin) + i128::from(nanoseconds(start_s)?);
            (start, start + width)
        }
        None => {
            let end = i128::from(last.end_ns);
            (end - width, end)
        }
    };
    let contained = |interval: &Interval| {
        i128::from(interval.begin_ns) >= requested_start
            && i128::from(interval.end_ns) <= requested_end
    };
    // The first recorded interval has no measured predecessor.
    let after_first = &intervals[1..];
    let first_index = 1 + after_first
        .iter()
        .position(contained)
        .ok_or("В окне нет целого теплового интервала с предыдущим отсчётом")?;
    let last_index = 1 + after_first.iter().rposition(contained).unwrap();
    let selected = &intervals[first_index..=last_index];
    let start = selected[0].begin_ns;
    let end = selected.last().unwrap().end_ns;
    if i128::from(start) - requested_start > i128::from(MAX_GAP_NS)
        || requested_end - i128::from(end) > i128::from(MAX_GAP_NS)
    {
        return Err("Тепловые интервалы не покрывают края заданного окна".into());
    }
    if selected.len() + 1 < MIN_POINTS {
        return Err("В окне меньше пяти температурных отсчётов".into());
    }
    let predecessor = &intervals[first_index - 1];
    if predecessor.end_ns != start || predecessor.end_ns <= predecessor.begin_ns || start < origin {
        return Err("Нет непрерывного температурного отсчёта в начале окна".into());
    }

    let mut window = Window {
        start_s: seconds(start - origin),
        end_s: 0.0,
        seconds: Vec::with_capacity(selected.len() + 1),
        temperatures_k: Vec::with_capacity(selected.len() + 1),
        durations_s: Vec::with_capacity(selected.len()),
        energy_j: Vec::with_capacity(selected.len()),
        derivative_weights: Vec::new(),
    };
    window.seconds.push(window.start_s);
    window.temperatures_k.push(temperature(predecessor)?);
    let mut previous_end = start;
    for interval in selected {
        let duration_ns = interval.end_ns.saturating_sub(interval.begin_ns);
        if !contained(interval)
            || interval.begin_ns != previous_end
            || !(1..=MAX_GAP_NS).contains(&duration_ns)
        {
            return Err("Разрыв или неверные границы теплового интервала в окне".into());
        }
        if !interval
            .heater_energy_j
            .iter()
            .all(|e| e.is_finite() && *e >= 0.0)
        {
            return Err("Недопустимая энергия нагревателей в окне".into());
        }
        window.seconds.push(seconds(interval.end_ns - origin));
        window.temperatures_k.push(temperature(interval)?);
        window.durations_s.push(seconds(duration_ns));
        window.energy_j.push(interval.heater_energy_j);
        previous_end = interval.end_ns;
    }
    window.end_s = *window.seconds.last().unwrap();
    window.derivative_weights = slope_weights(start, end, selected)?;
    Ok(window)
}

fn nanoseconds(value: f64) -> Result<u64, String> {
    Duration::try_from_secs_f64(value)
        .ok()
        .and_then(|duration| duration.as_nanos().try_into().ok())
        .ok_or_else(|| "Недопустимое модельное время окна".into())
}

fn seconds(ns: u64) -> f64 {
    Duration::from_nanos(ns).as_secs_f64()
}

fn temperature(interval: &Interval) -> Result<[f64; 4], String> {
    interval
        .end_k
        .filter(|values| values.iter().all(|t| t.is_finite() && *t > 0.0))
        .ok_or_else(|| "В окне есть непригодный температурный отсчёт".into())
}

fn slope_weights(start: u64, end: u64, intervals: &[Interval]) -> Result<Vec<f64>, String> {
    let span_ns = (end - start) as f64;
    let half_span_s = seconds(end - start) * 0.5;
    let mut scaled = Vec::with_capacity(intervals.len() + 1);
    scaled.push(-1.0);
    scaled.extend(
        intervals
            .iter()
            .map(|interval| 2.0 * ((interval.end_ns - start) as f64 / span_ns) - 1.0),
    );
    let count = scaled.len() as f64;
    let mean_x = scaled.iter().sum::<f64>() / count;
    let mean_square = scaled.iter().map(|x| x * x).sum::<f64>() / count;
    let (square_norm, cross) = scaled.iter().fold((0.0, 0.0), |(norm, cross), x| {
        let centered_square = x * x - mean_square;
        (
            norm + centered_square * centered_square,
            cross + (x - mean_x) * centered_square,
        )
    });
    let rank_floor = 64.0 * f64::EPSILON * count;
    if square_norm <= rank_floor {
        return Err("Вырожденная сетка квадратичной аппроксимации".into());
    }
    let projection = cross / square_norm;
    // The residual of x is orthogonal to both the constant and x².
    for x in &mut scaled {
        *x = *x - mean_x - projection * (*x * *x - mean_square);
    }
    let norm = scaled.iter().map(|r| r * r).sum::<f64>();
    if norm <= rank_floor {
        return Err("Вырожденная сетка квадратичной аппроксимации".into());
    }
    let denominator = norm * half_span_s;
    for weight in &mut scaled {
        *weight /= denominator;
    }
    Ok(scaled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(origin: u64) -> Vec<Interval> {
        let mut begin_ns = origin;
        (0..130)
            .map(|index| {
                let end_ns = begin_ns
                    + if index % 2 == 0 {
                        90_000_000
                    } else {
                        110_000_000
                    };
                let t = seconds(end_ns - origin) - 7.0;
                let interval = Interval {
                    begin_ns,
                    end_ns,
                    heater_energy_j: [0.01, 0.02],
                    end_k: Some([45.0, 18.0 - 0.2 * t + 0.03 * t * t, 16.0, 77.0]),
                };
                begin_ns = end_ns;
                interval
            })
            .collect()
    }

    #[test]
    fn absolute_nanosecond_offset_does_not_change_the_fit() {
        let options = Options {
            start_s: Some(1.0),
            ..Options::default()
        };
        let zero = select(&trace(0), options).unwrap();
        let large = select(&trace(u64::MAX - 13_000_000_000), options).unwrap();
        assert_eq!(large.seconds, zero.seconds);
        assert_eq!(large.derivative_weights, zero.derivative_weights);
        let slope = large
            .derivative_weights
            .iter()
            .zip(large.temperatures_k)
            .map(|(weight, temperatures)| weight * temperatures[1])
            .sum::<f64>();
        assert!((slope + 0.2).abs() < 1e-12);
    }

    #[test]
    fn retains_previous_endpoint_without_its_heater_energy() {
        let mut intervals = trace(0);
        intervals[0].heater_energy_j = [1000.0, 1000.0];
        let options = Options {
            start_s: Some(0.0),
            ..Options::default()
        };
        let window = select(&intervals, options).unwrap();
        assert_eq!(window.start_s, 0.09);
        assert_eq!(window.end_s, 12.0);
        assert_eq!(window.temperatures_k[0], intervals[0].end_k.unwrap());
        assert_eq!(window.energy_j[0], intervals[1].heater_energy_j);
        assert_eq!(window.seconds.len(), window.energy_j.len() + 1);
    }

    #[test]
    fn rejects_duplicate_interval_and_inadequate_edge_coverage() {
        let mut intervals = trace(0);
        intervals.insert(50, intervals[49]);
        let options = Options {
            start_s: Some(1.0),
            ..Options::default()
        };
        assert!(select(&intervals, options).is_err());
        assert!(select(&trace(0)[..124], options).is_err());
    }
}
