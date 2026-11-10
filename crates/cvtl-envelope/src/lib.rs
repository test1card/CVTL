//! Nominal continuous two-regime source envelope and heater selection estimates.
//!
//! The model uses the nominal 40 V / 1 A and 6 V / 3 A regions of
//! Keithley 2604B, not the instrument's absolute range corners. See
//! <https://download.tek.com/document/SPEC-2602BD_Feb2022.pdf>.
//! Estimates are not operating permission or a verified hardware profile.
use cvtl_core::units::{Amperes, Ohms, Volts, Watts};
use std::{error::Error, fmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvelopeError;
impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid or non-finite envelope input or result")
    }
}
impl Error for EnvelopeError {}

#[derive(Debug, Clone, Copy)]
pub struct Margins {
    k_v: f64,
    k_i: f64,
}
impl Margins {
    pub fn new(k_v: f64, k_i: f64) -> Result<Self, EnvelopeError> {
        if [k_v, k_i]
            .into_iter()
            .all(|v| v.is_finite() && v > 0. && v <= 1.)
        {
            Ok(Self { k_v, k_i })
        } else {
            Err(EnvelopeError)
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub struct ResistanceRange {
    min: Ohms,
    max: Ohms,
}
impl ResistanceRange {
    pub fn new(min: Ohms, max: Ohms) -> Result<Self, EnvelopeError> {
        if min.get() > 0. && max.get() >= min.get() {
            Ok(Self { min, max })
        } else {
            Err(EnvelopeError)
        }
    }

    pub fn bounds(self) -> (Ohms, Ohms) {
        (self.min, self.max)
    }
}
#[derive(Debug, Clone, Copy)]
pub struct ChannelSpec {
    pub resistance: ResistanceRange,
    pub loop_resistance: Ohms,
    pub heater_limit: Option<Watts>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitingFactor {
    Current,
    Voltage,
    CurrentAndVoltage,
    Heater,
}
#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub power: Watts,
    pub current: Amperes,
    pub force_voltage: Volts,
    pub sense_voltage: Volts,
    pub voltage_headroom: Volts,
    pub resistance: Ohms,
    pub limiting: LimitingFactor,
}
#[derive(Debug, Clone, Copy)]
pub struct Advice {
    pub source: Point,
    pub capped: Option<Point>,
}
#[derive(Debug, Clone, Copy)]
pub struct PairAdvice {
    pub a: Advice,
    pub b: Advice,
    pub source_sum: Watts,
    /// Sum of individually heater-capped estimates, before any total limit.
    pub heater_capped_sum: Option<Watts>,
    /// Present only when both heater caps and the total cap were supplied.
    pub capped_sum: Option<Watts>,
}
fn watts(v: f64) -> Result<Watts, EnvelopeError> {
    Watts::new(v).ok_or(EnvelopeError)
}
// Configured safety limits must be positive, matching the configuration schema.
// To request zero heat, command zero current instead of using a zero limit.
fn positive_cap(v: Option<Watts>) -> Result<(), EnvelopeError> {
    if v.is_some_and(|x| x.get() <= 0.) {
        Err(EnvelopeError)
    } else {
        Ok(())
    }
}
fn validate(spec: ChannelSpec) -> Result<(), EnvelopeError> {
    if spec.loop_resistance.get() < 0. {
        return Err(EnvelopeError);
    }
    positive_cap(spec.heater_limit)
}

/// Tests the source envelope only; heater and installation limits are separate.
pub fn check_current(
    r: Ohms,
    loop_r: Ohms,
    current: Amperes,
    m: Margins,
) -> Result<bool, EnvelopeError> {
    if r.get() <= 0. || loop_r.get() < 0. || current.get() < 0. {
        return Err(EnvelopeError);
    }
    let force = current.get() * (r.get() + loop_r.get());
    if !force.is_finite() {
        return Err(EnvelopeError);
    }
    Ok((current.get() <= 3. * m.k_i && force <= 6. * m.k_v)
        || (current.get() <= m.k_i && force <= 40. * m.k_v))
}

fn point(r: f64, wire: f64, m: Margins, cap: Option<f64>) -> Result<Point, EnvelopeError> {
    let total = r + wire;
    if !total.is_finite() {
        return Err(EnvelopeError);
    }
    let regimes = [(3. * m.k_i, 6. * m.k_v), (m.k_i, 40. * m.k_v)];
    let (ilim, vlim) = regimes
        .into_iter()
        .max_by(|a, b| {
            let ia = a.0.min(a.1 / total);
            let ib = b.0.min(b.1 / total);
            ia.total_cmp(&ib)
        })
        .ok_or(EnvelopeError)?;
    let source_i = ilim.min(vlim / total);
    let mut i = cap.map_or(source_i, |c| source_i.min((c / r).sqrt()));
    // Division and square root can round upward; enforce actual products, not
    // an epsilon-relaxed permission check. Each step is toward lower current.
    while i * total > vlim || cap.is_some_and(|c| i * (i * r) > c) {
        i = i.next_down();
    }
    let sense = i * r;
    let force = i * total;
    let power = i * sense;
    let near = |a: f64, b: f64| (a - b).abs() <= 16. * f64::EPSILON * a.abs().max(b.abs()).max(1.);
    let limiting =
        if cap.is_some_and(|c| c < source_i * source_i * r && !near(c, source_i * source_i * r)) {
            LimitingFactor::Heater
        } else if near(i, ilim) && near(force, vlim) {
            LimitingFactor::CurrentAndVoltage
        } else if near(i, ilim) {
            LimitingFactor::Current
        } else {
            LimitingFactor::Voltage
        };
    Ok(Point {
        power: watts(power)?,
        current: Amperes::new(i).ok_or(EnvelopeError)?,
        force_voltage: Volts::new(force).ok_or(EnvelopeError)?,
        sense_voltage: Volts::new(sense).ok_or(EnvelopeError)?,
        voltage_headroom: Volts::new((vlim - force).max(0.)).ok_or(EnvelopeError)?,
        resistance: Ohms::new(r).ok_or(EnvelopeError)?,
        limiting,
    })
}

/// Conservative minimum over the complete continuous resistance interval.
pub fn advise(spec: ChannelSpec, m: Margins) -> Result<Advice, EnvelopeError> {
    validate(spec)?;
    let lo = spec.resistance.min.get();
    let hi = spec.resistance.max.get();
    let wire = spec.loop_resistance.get();
    // Each regime has one maximum and no interior minimum. The only possible
    // additional minimum of their union is their crossing on the descending
    // high-current voltage branch and ascending low-current branch.
    let cross = 6. * m.k_v / m.k_i - wire;
    let mut worst = point(lo, wire, m, None)?;
    for r in [hi, cross] {
        if r >= lo && r <= hi {
            let p = point(r, wire, m, None)?;
            if p.power.get() < worst.power.get() {
                worst = p;
            }
        }
    }
    let capped = spec
        .heater_limit
        .map(|cap| point(worst.resistance.get(), wire, m, Some(cap.get())))
        .transpose()?;
    Ok(Advice {
        source: worst,
        capped,
    })
}

/// Two independent source estimates and optional intersection with all caps.
pub fn advise_pair(
    a: ChannelSpec,
    b: ChannelSpec,
    m: Margins,
    total_limit: Option<Watts>,
) -> Result<PairAdvice, EnvelopeError> {
    positive_cap(total_limit)?;
    let a = advise(a, m)?;
    let b = advise(b, m)?;
    let source_sum = watts(a.source.power.get() + b.source.power.get())?;
    let heater_capped_sum = match (a.capped, b.capped) {
        (Some(a), Some(b)) => Some(watts(a.power.get() + b.power.get())?),
        _ => None,
    };
    let capped_sum = match (heater_capped_sum, total_limit) {
        (Some(s), Some(t)) => Some(watts(s.get().min(t.get()))?),
        _ => None,
    };
    Ok(PairAdvice {
        a,
        b,
        source_sum,
        heater_capped_sum,
        capped_sum,
    })
}
