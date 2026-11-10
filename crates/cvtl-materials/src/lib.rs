//! Validated cryogenic material curves. Specific enthalpy is zero at the lower bound.
use cvtl_core::units::{
    JoulesPerKilogram, JoulesPerKilogramKelvin, Kelvin, WattsPerMeter, WattsPerMeterKelvin,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaterialError {
    InvalidCurve,
    OutOfRange,
    UnknownMaterial,
    UnavailableProperty,
}

#[derive(Debug, Clone)]
struct Curve {
    points: Vec<(f64, f64)>,
    cumulative: Vec<f64>,
}
impl Curve {
    fn new(points: Vec<(f64, f64)>) -> Result<Self, MaterialError> {
        if points.len() < 2
            || points
                .iter()
                .any(|(x, y)| !x.is_finite() || !y.is_finite() || *x <= 0. || *y <= 0.)
            || points.windows(2).any(|p| p[0].0 >= p[1].0)
        {
            return Err(MaterialError::InvalidCurve);
        }
        let mut cumulative = vec![0.];
        for p in points.windows(2) {
            let next =
                cumulative.last().unwrap() + (p[1].0 - p[0].0) * (p[0].1 * 0.5 + p[1].1 * 0.5);
            if !f64::is_finite(next) || next <= *cumulative.last().unwrap() {
                return Err(MaterialError::InvalidCurve);
            }
            cumulative.push(next);
        }
        Ok(Self { points, cumulative })
    }
    fn bounds(&self) -> (f64, f64) {
        (self.points[0].0, self.points.last().unwrap().0)
    }
    fn segment(&self, t: f64) -> Result<usize, MaterialError> {
        let (lo, hi) = self.bounds();
        if !t.is_finite() || t < lo || t > hi {
            return Err(MaterialError::OutOfRange);
        }
        Ok(self
            .points
            .partition_point(|p| p.0 <= t)
            .saturating_sub(1)
            .min(self.points.len() - 2))
    }
    fn value(&self, t: f64) -> Result<f64, MaterialError> {
        let i = self.segment(t)?;
        let (x, y) = self.points[i];
        let (x1, y1) = self.points[i + 1];
        let fraction = (t - x) / (x1 - x);
        Ok(y * (1. - fraction) + y1 * fraction)
    }
    fn primitive(&self, t: f64) -> Result<f64, MaterialError> {
        let i = self.segment(t)?;
        let (x, y) = self.points[i];
        let dx = t - x;
        Ok(self.cumulative[i] + dx * (y * 0.5 + self.value(t)? * 0.5))
    }
    fn integral(&self, a: f64, b: f64) -> Result<f64, MaterialError> {
        Ok(self.primitive(b)? - self.primitive(a)?)
    }
    fn inverse(&self, h: f64) -> Result<f64, MaterialError> {
        if !h.is_finite() || h < 0. || h > *self.cumulative.last().unwrap() {
            return Err(MaterialError::OutOfRange);
        }
        let i = self
            .cumulative
            .partition_point(|v| *v <= h)
            .saturating_sub(1)
            .min(self.points.len() - 2);
        let (x, y) = self.points[i];
        let (x1, y1) = self.points[i + 1];
        if h == self.cumulative[i] {
            return Ok(x);
        }
        if h == self.cumulative[i + 1] {
            return Ok(x1);
        }
        // Scale all quantities before solving to avoid squaring large cp values.
        let scale = y.max(y1);
        let a = y / scale;
        let b = y1 / scale;
        let fraction_h = (h - self.cumulative[i]) / (self.cumulative[i + 1] - self.cumulative[i]);
        let mut lo = 0.;
        let mut hi = 1.;
        for _ in 0..64 {
            let mid = (lo + hi) * 0.5;
            let area = mid * (a * (1. - mid * 0.5) + b * mid * 0.5) / (a * 0.5 + b * 0.5);
            if area < fraction_h {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ok(x * (1. - (lo + hi) * 0.5) + x1 * ((lo + hi) * 0.5))
    }
}

#[derive(Debug, Clone)]
pub struct HeatCapacityCurve(Curve);
impl HeatCapacityCurve {
    pub fn new(p: Vec<(Kelvin, JoulesPerKilogramKelvin)>) -> Result<Self, MaterialError> {
        Ok(Self(Curve::new(
            p.into_iter().map(|(x, y)| (x.get(), y.get())).collect(),
        )?))
    }
    pub fn value(&self, t: Kelvin) -> Result<JoulesPerKilogramKelvin, MaterialError> {
        JoulesPerKilogramKelvin::new(self.0.value(t.get())?).ok_or(MaterialError::InvalidCurve)
    }
    pub fn integral(&self, a: Kelvin, b: Kelvin) -> Result<JoulesPerKilogram, MaterialError> {
        JoulesPerKilogram::new(self.0.integral(a.get(), b.get())?)
            .ok_or(MaterialError::InvalidCurve)
    }
    pub fn inverse_integral(&self, h: JoulesPerKilogram) -> Result<Kelvin, MaterialError> {
        Kelvin::new(self.0.inverse(h.get())?).ok_or(MaterialError::InvalidCurve)
    }
    pub fn minimum(&self) -> JoulesPerKilogramKelvin {
        JoulesPerKilogramKelvin::new(
            self.0
                .points
                .iter()
                .map(|p| p.1)
                .fold(f64::INFINITY, f64::min),
        )
        .expect("validated finite positive curve")
    }
    pub fn bounds(&self) -> (Kelvin, Kelvin) {
        let (a, b) = self.0.bounds();
        (Kelvin::new(a).unwrap(), Kelvin::new(b).unwrap())
    }
}
#[derive(Debug, Clone)]
pub struct ThermalConductivityCurve(Curve);
impl ThermalConductivityCurve {
    pub fn new(p: Vec<(Kelvin, WattsPerMeterKelvin)>) -> Result<Self, MaterialError> {
        Ok(Self(Curve::new(
            p.into_iter().map(|(x, y)| (x.get(), y.get())).collect(),
        )?))
    }
    pub fn value(&self, t: Kelvin) -> Result<WattsPerMeterKelvin, MaterialError> {
        WattsPerMeterKelvin::new(self.0.value(t.get())?).ok_or(MaterialError::InvalidCurve)
    }
    pub fn integral(&self, a: Kelvin, b: Kelvin) -> Result<WattsPerMeter, MaterialError> {
        WattsPerMeter::new(self.0.integral(a.get(), b.get())?).ok_or(MaterialError::InvalidCurve)
    }
    pub fn bounds(&self) -> (Kelvin, Kelvin) {
        let (a, b) = self.0.bounds();
        (Kelvin::new(a).unwrap(), Kelvin::new(b).unwrap())
    }
}

/// Source fit error is relative to the underlying data, not a complete uncertainty.
#[derive(Debug, Clone)]
pub struct Provenance {
    pub source_url: &'static str,
    pub fit_error: String,
    pub model: &'static str,
    pub interpolation_step_k: Kelvin,
}
#[derive(Debug, Clone)]
pub struct SourcedCurve<T> {
    pub curve: T,
    pub provenance: Provenance,
}

fn source(id: &str) -> Option<&'static str> {
    match id {
        "ofhc_rrr100" => Some(
            "https://trc.nist.gov/cryogenics/materials/OFHC%20Copper/OFHC_Copper_rev1.htm",
        ),
        "al6061_t6" => Some(
            "https://trc.nist.gov/cryogenics/materials/6061%20Aluminum/6061_T6Aluminum_rev.htm",
        ),
        "al1100" => Some(
            "https://trc.nist.gov/cryogenics/materials/1100%20Aluminum/1100%20Aluminum_rev.htm",
        ),
        "ss304" => Some("https://trc.nist.gov/cryogenics/materials/304Stainless/304Stainless_rev.htm"),
        "g10_cr" | "g10_cr_normal" | "g10_cr_warp" => Some(
            "https://trc.nist.gov/cryogenics/materials/G-10%20CR%20Fiberglass%20Epoxy/G10CRFiberglassEpoxy_rev.htm",
        ),
        _ => None,
    }
}
fn nist(id: &str, property: &str) -> Result<(Curve, Provenance), MaterialError> {
    const STEP_K: f64 = 0.01;
    let url = source(id).ok_or(MaterialError::UnknownMaterial)?;
    let line = include_str!("../../../data/materials/nist.csv")
        .lines()
        .filter(|l| !l.starts_with('#'))
        .find(|l| {
            let mut x = l.split(',');
            x.next() == Some(id) && x.next() == Some(property)
        })
        .ok_or(MaterialError::UnavailableProperty)?;
    let fields: Vec<&str> = line.split(',').collect();
    if fields.len() != 14 {
        return Err(MaterialError::InvalidCurve);
    }
    let parse = |s: &str| s.parse::<f64>().map_err(|_| MaterialError::InvalidCurve);
    let low = parse(fields[2])?;
    let high = parse(fields[3])?;
    let mut coeff = [0.0; 9];
    for (coefficient, field) in coeff.iter_mut().zip(&fields[5..]) {
        *coefficient = parse(field)?;
    }
    if !low.is_finite()
        || !high.is_finite()
        || low <= 0.
        || high <= low
        || high - low > 1000.
        || coeff.iter().any(|c| !c.is_finite())
    {
        return Err(MaterialError::InvalidCurve);
    }
    let copper = id == "ofhc_rrr100" && property == "k";
    let count = ((high - low) / STEP_K).round() as usize;
    let points = (0..=count)
        .map(|i| {
            let t = if i == count {
                high
            } else {
                low + i as f64 * STEP_K
            };
            let log = if copper {
                let s = t.sqrt();
                (coeff[0] + coeff[2] * s + coeff[4] * t + coeff[6] * t * s + coeff[8] * t * t)
                    / (1. + coeff[1] * s + coeff[3] * t + coeff[5] * t * s + coeff[7] * t * t)
            } else {
                let x = t.log10();
                coeff.iter().rev().fold(0., |acc, c| acc * x + c)
            };
            (t, 10_f64.powf(log))
        })
        .collect();
    Ok((
        Curve::new(points)?,
        Provenance {
            source_url: url,
            fit_error: format!("{}% relative to source data", fields[4]),
            model: if copper {
                "NIST OFHC thermal conductivity rational fit, RRR=100"
            } else {
                "NIST log-polynomial fit"
            },
            interpolation_step_k: Kelvin::new(STEP_K).expect("finite sampling interval"),
        },
    ))
}
pub fn nist_heat_capacity(id: &str) -> Result<SourcedCurve<HeatCapacityCurve>, MaterialError> {
    let (curve, provenance) = nist(id, "cp")?;
    Ok(SourcedCurve {
        curve: HeatCapacityCurve(curve),
        provenance,
    })
}
pub fn nist_conductivity(
    id: &str,
) -> Result<SourcedCurve<ThermalConductivityCurve>, MaterialError> {
    let (curve, provenance) = nist(id, "k")?;
    Ok(SourcedCurve {
        curve: ThermalConductivityCurve(curve),
        provenance,
    })
}
