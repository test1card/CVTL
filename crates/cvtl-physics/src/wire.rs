//! Bounded steady one-dimensional wire model. Positive endpoint heat enters the node.
//! The integral of k(T) is used for each cell (Kirchhoff potential). Radiation is
//! positive when absorbed from the surroundings; each segment describes one strand.
use cvtl_core::units::{Amperes, Kelvin, Meters, OhmMeters, SquareMeters, Watts};
use cvtl_materials::ThermalConductivityCurve;

#[derive(Debug, Clone)]
pub enum WireError {
    InvalidInput,
    MissingResistivity,
    OutOfRange,
    NoConvergence,
}
#[derive(Debug, Clone)]
pub struct ResistivityCurve {
    points: Vec<(f64, f64)>,
}
impl ResistivityCurve {
    pub fn constant(value: OhmMeters) -> Result<Self, WireError> {
        if value.get() <= 0.0 {
            return Err(WireError::InvalidInput);
        }
        Ok(Self {
            points: vec![(0.0, value.get())],
        })
    }
    pub fn table(points: Vec<(Kelvin, OhmMeters)>) -> Result<Self, WireError> {
        let points: Vec<_> = points
            .into_iter()
            .map(|(t, r)| (t.get(), r.get()))
            .collect();
        if points.len() < 2
            || points.iter().any(|(t, r)| *t <= 0.0 || *r <= 0.0)
            || points.windows(2).any(|p| p[0].0 >= p[1].0)
        {
            return Err(WireError::InvalidInput);
        }
        Ok(Self { points })
    }
    fn value(&self, t: f64) -> Result<(f64, f64), WireError> {
        if self.points.len() == 1 {
            return Ok((self.points[0].1, 0.0));
        }
        if t < self.points[0].0 || t > self.points.last().unwrap().0 {
            return Err(WireError::OutOfRange);
        }
        let i = self
            .points
            .partition_point(|p| p.0 <= t)
            .saturating_sub(1)
            .min(self.points.len() - 2);
        let (x, y) = self.points[i];
        let (x1, y1) = self.points[i + 1];
        let slope = (y1 - y) / (x1 - x);
        Ok((y + slope * (t - x), slope))
    }
}
#[derive(Debug, Clone)]
pub struct Radiation {
    pub emissivity: f64,
    pub surface_area_m2: SquareMeters,
    pub environment_k: Kelvin,
}
#[derive(Debug, Clone)]
pub struct WireSegment {
    pub conductivity: ThermalConductivityCurve,
    pub resistivity: Option<ResistivityCurve>,
    pub length_m: Meters,
    pub area_m2: SquareMeters,
    pub current_a: Amperes,
    pub start_k: Kelvin,
    pub end_k: Kelvin,
    pub radiation: Option<Radiation>,
}
#[derive(Debug, Clone)]
pub struct WireResult {
    pub heat_into_start_w: Watts,
    pub heat_into_end_w: Watts,
    pub joule_w: Watts,
    pub radiation_w: Watts,
}
#[derive(Debug, Clone)]
pub struct Sensitivity {
    pub nominal: WireResult,
    pub corners: Vec<WireResult>,
}
const SIGMA: f64 = 5.670_374_419e-8;
const N: usize = 512;
fn source(s: &WireSegment, t: f64) -> Result<(f64, f64, f64), WireError> {
    let (rho, drho) = if s.current_a.get() == 0.0 {
        (0.0, 0.0)
    } else {
        s.resistivity
            .as_ref()
            .ok_or(WireError::MissingResistivity)?
            .value(t)?
    };
    let a = s.area_m2.get();
    let i = s.current_a.get();
    let joule = i * i * rho / a;
    let (rad, derivative) = if let Some(r) = &s.radiation {
        let c = SIGMA * r.emissivity * r.surface_area_m2.get() / s.length_m.get();
        (
            c * (r.environment_k.get().powi(4) - t.powi(4)),
            -4.0 * c * t.powi(3),
        )
    } else {
        (0.0, 0.0)
    };
    let derivative = i * i * drho / a + derivative;
    if !joule.is_finite() || !rad.is_finite() || !derivative.is_finite() {
        return Err(WireError::InvalidInput);
    }
    Ok((joule, rad, derivative))
}
fn kval(s: &WireSegment, t: f64) -> Result<f64, WireError> {
    s.conductivity
        .value(Kelvin::new(t).ok_or(WireError::InvalidInput)?)
        .map(|v| v.get())
        .map_err(|_| WireError::OutOfRange)
}
fn integral(s: &WireSegment, a: f64, b: f64) -> Result<f64, WireError> {
    s.conductivity
        .integral(
            Kelvin::new(a).ok_or(WireError::InvalidInput)?,
            Kelvin::new(b).ok_or(WireError::InvalidInput)?,
        )
        .map(|v| v.get())
        .map_err(|_| WireError::OutOfRange)
}
fn residual(s: &WireSegment, t: &[f64], h: f64) -> Result<Vec<f64>, WireError> {
    let mut f = vec![0.0; N - 1];
    let a = s.area_m2.get();
    for j in 1..N {
        let (q, r, _) = source(s, t[j])?;
        f[j - 1] =
            a * (integral(s, t[j], t[j + 1])? - integral(s, t[j - 1], t[j])?) / h + h * (q + r);
        if !f[j - 1].is_finite() {
            return Err(WireError::NoConvergence);
        }
    }
    Ok(f)
}
pub fn solve(s: &WireSegment) -> Result<WireResult, WireError> {
    let l = s.length_m.get();
    let a = s.area_m2.get();
    let t0 = s.start_k.get();
    let t1 = s.end_k.get();
    if l <= 0.0 || a <= 0.0 || t0 <= 0.0 || t1 <= 0.0 {
        return Err(WireError::InvalidInput);
    }
    if let Some(r) = &s.radiation {
        if !(0.0..=1.0).contains(&r.emissivity)
            || r.surface_area_m2.get() < 0.0
            || r.environment_k.get() <= 0.0
        {
            return Err(WireError::InvalidInput);
        }
    }
    kval(s, t0)?;
    kval(s, t1)?;
    let (q0, r0, _) = source(s, t0)?;
    let (q1, r1, _) = source(s, t1)?;
    let conductive = a * integral(s, t0, t1)? / l;
    if s.current_a.get() == 0.0 && s.radiation.is_none() {
        return Ok(WireResult {
            heat_into_start_w: Watts::new(conductive).ok_or(WireError::InvalidInput)?,
            heat_into_end_w: Watts::new(-conductive).ok_or(WireError::InvalidInput)?,
            joule_w: Watts::new(0.0).unwrap(),
            radiation_w: Watts::new(0.0).unwrap(),
        });
    }
    let h = l / N as f64;
    let scale = conductive
        .abs()
        .max(l * (q0.abs() + q1.abs() + r0.abs() + r1.abs()))
        .max(f64::MIN_POSITIVE);
    if h == 0.0 || !scale.is_finite() {
        return Err(WireError::InvalidInput);
    }
    let mut t: Vec<f64> = (0..=N)
        .map(|j| t0 + (t1 - t0) * (j as f64 / N as f64))
        .collect();
    t[N] = t1;
    let mut converged = false;
    for _ in 0..60 {
        let f = residual(s, &t, h)?;
        let norm = f.iter().fold(0.0_f64, |x, y| x.max(y.abs()));
        if norm < 1e-12 * scale {
            converged = true;
            break;
        }
        let mut upper = vec![0.0; N - 1];
        let mut rhs = vec![0.0; N - 1];
        for j in 1..N {
            let (_, _, d) = source(s, t[j])?;
            let diagonal = -2.0 * a * kval(s, t[j])? / h + h * d;
            let lower = if j > 1 {
                a * kval(s, t[j - 1])? / h
            } else {
                0.0
            };
            let up = if j < N - 1 {
                a * kval(s, t[j + 1])? / h
            } else {
                0.0
            };
            let denom = if j == 1 {
                diagonal
            } else {
                diagonal - lower * upper[j - 2]
            };
            if !denom.is_finite() || denom.abs() < 1e-300 {
                return Err(WireError::NoConvergence);
            }
            upper[j - 1] = up / denom;
            rhs[j - 1] = (-f[j - 1] - if j > 1 { lower * rhs[j - 2] } else { 0.0 }) / denom;
        }
        for j in (0..N - 2).rev() {
            rhs[j] -= upper[j] * rhs[j + 1];
        }
        let mut alpha = 1.0;
        let mut accepted = false;
        for _ in 0..30 {
            let mut candidate = t.clone();
            for j in 1..N {
                candidate[j] += alpha * rhs[j - 1];
            }
            if let Ok(next) = residual(s, &candidate, h) {
                let nn = next.iter().fold(0.0_f64, |x, y| x.max(y.abs()));
                if nn < norm {
                    t = candidate;
                    accepted = true;
                    break;
                }
            }
            alpha *= 0.5;
        }
        if !accepted {
            return Err(WireError::NoConvergence);
        }
    }
    if !converged {
        return Err(WireError::NoConvergence);
    }
    let mut j = 0.0;
    let mut r = 0.0;
    for (idx, &temp) in t.iter().enumerate() {
        let (x, y, _) = source(s, temp)?;
        let weight = if idx == 0 || idx == N { 0.5 } else { 1.0 };
        j += h * weight * x;
        r += h * weight * y;
    }
    let (j0, r0, _) = source(s, t[0])?;
    let (jn, rn, _) = source(s, t[N])?;
    let start = a * integral(s, t[0], t[1])? / h + h * (j0 + r0) * 0.5;
    let end = a * integral(s, t[N], t[N - 1])? / h + h * (jn + rn) * 0.5;
    let balance_scale = start
        .abs()
        .max(end.abs())
        .max(j.abs() + r.abs())
        .max(f64::MIN_POSITIVE);
    if !start.is_finite()
        || !end.is_finite()
        || !j.is_finite()
        || !r.is_finite()
        || (start + end - j - r).abs() > 1e-8 * balance_scale
    {
        return Err(WireError::NoConvergence);
    }
    Ok(WireResult {
        heat_into_start_w: Watts::new(start).ok_or(WireError::InvalidInput)?,
        heat_into_end_w: Watts::new(end).ok_or(WireError::InvalidInput)?,
        joule_w: Watts::new(j).ok_or(WireError::InvalidInput)?,
        radiation_w: Watts::new(r).ok_or(WireError::InvalidInput)?,
    })
}
/// Sample the four scale corners, not a complete uncertainty interval.
pub fn sensitivity(
    s: &WireSegment,
    k_relative: f64,
    length_relative: f64,
) -> Result<Sensitivity, WireError> {
    if !k_relative.is_finite()
        || !length_relative.is_finite()
        || !(0.0..1.0).contains(&k_relative)
        || !(0.0..1.0).contains(&length_relative)
    {
        return Err(WireError::InvalidInput);
    }
    let nominal = solve(s)?;
    let mut corners = Vec::with_capacity(4);
    for k in [1.0 - k_relative, 1.0 + k_relative] {
        for l in [1.0 - length_relative, 1.0 + length_relative] {
            let mut v = s.clone();
            v.length_m = Meters::new(s.length_m.get() * l).ok_or(WireError::InvalidInput)?;
            v.conductivity = s
                .conductivity
                .scaled(k)
                .map_err(|_| WireError::InvalidInput)?;
            corners.push(solve(&v)?);
        }
    }
    Ok(Sensitivity { nominal, corners })
}
