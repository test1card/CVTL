//! Enthalpy model and signed thermal balances, independent of hardware.
use cvtl_core::units::*;
use cvtl_materials::{HeatCapacityCurve, ThermalConductivityCurve};

pub type Result<T> = std::result::Result<T, String>;
fn valid_positive(x: f64) -> Result<()> {
    if x.is_finite() && x > 0.0 {
        Ok(())
    } else {
        Err("expected a positive finite value".into())
    }
}
fn unit<T>(v: Option<T>) -> Result<T> {
    v.ok_or_else(|| "non-finite result".into())
}
fn material<T, E: std::fmt::Debug>(r: std::result::Result<T, E>) -> Result<T> {
    r.map_err(|e| format!("material: {e:?}"))
}

fn series_body_temperature(
    conductivity: &ThermalConductivityCurve,
    port: Kelvin,
    interface: Kelvin,
    area_per_length: f64,
    contact: WattsPerKelvin,
) -> Result<f64> {
    let flow = area_per_length * material(conductivity.integral(port, interface))?.get();
    Ok(unit(Kelvin::new(interface.get() + flow / contact.get()))?.get())
}

#[derive(Clone)]
pub enum Bridge {
    Constant(WattsPerKelvin),
    Conductor {
        conductivity: ThermalConductivityCurve,
        area: SquareMeters,
        length: Meters,
    },
    /// Conductance is linearly interpolated at the mean end temperature.
    Table(Vec<(Kelvin, WattsPerKelvin)>),
    Series {
        contact: WattsPerKelvin,
        conductivity: ThermalConductivityCurve,
        area: SquareMeters,
        length: Meters,
    },
}
impl Bridge {
    pub fn flow(&self, body: Kelvin, port: Kelvin) -> Result<Watts> {
        let a = body.get();
        let b = port.get();
        valid_positive(a)?;
        valid_positive(b)?;
        let q = match self {
            Self::Constant(g) => {
                if g.get() < 0.0 {
                    return Err("negative conductance".into());
                }
                g.get() * (a - b)
            }
            Self::Conductor {
                conductivity,
                area,
                length,
            } => {
                valid_positive(area.get())?;
                valid_positive(length.get())?;
                area.get() / length.get() * material(conductivity.integral(port, body))?.get()
            }
            Self::Table(rows) => {
                if rows.len() < 2
                    || rows.iter().any(|(t, g)| t.get() <= 0.0 || g.get() <= 0.0)
                    || rows.windows(2).any(|w| w[0].0.get() >= w[1].0.get())
                {
                    return Err("invalid conductance table".into());
                }
                let t = a.midpoint(b);
                let w = rows
                    .windows(2)
                    .find(|w| w[0].0.get() <= t && t <= w[1].0.get())
                    .ok_or("conductance outside table")?;
                let f = (t - w[0].0.get()) / (w[1].0.get() - w[0].0.get());
                (w[0].1.get() + f * (w[1].1.get() - w[0].1.get())) * (a - b)
            }
            Self::Series {
                contact,
                conductivity,
                area,
                length,
            } => {
                valid_positive(contact.get())?;
                valid_positive(area.get())?;
                valid_positive(length.get())?;
                // The conductor spans port to interface; only the interface
                // and port must lie in the conductivity curve domain.
                let (lower, upper) = conductivity.bounds();
                let area_per_length = area.get() / length.get();
                let body_at =
                    |x| series_body_temperature(conductivity, port, x, area_per_length, *contact);
                let mut lo = lower.get();
                let mut hi = upper.get();
                if body_at(lower)? > a || body_at(upper)? < a {
                    return Err("series interface outside conductivity range".into());
                }
                for _ in 0..80 {
                    let mid = lo.midpoint(hi);
                    if body_at(unit(Kelvin::new(mid))?)? < a {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let interface = unit(Kelvin::new(lo.midpoint(hi)))?;
                area_per_length * material(conductivity.integral(port, interface))?.get()
            }
        };
        unit(Watts::new(q))
    }
}

#[derive(Clone, Copy)]
pub struct PortSample {
    pub temperature: Kelvin,
    pub age: Seconds,
}
/// The final flow is available via `bridge.flow(body.temperature(), sample.temperature)`.
pub struct StepReport {
    pub net_energy: Joules,
    pub port_age: Seconds,
}
pub struct Body {
    cp: HeatCapacityCurve,
    mass: Kilograms,
    scale: f64,
    h: f64,
}
impl Body {
    pub fn new(
        cp: HeatCapacityCurve,
        mass: Kilograms,
        scale: f64,
        temperature: Kelvin,
    ) -> Result<Self> {
        valid_positive(mass.get())?;
        valid_positive(scale)?;
        if scale > 1.0 {
            return Err("scale exceeds one".into());
        }
        let effective_mass = mass.get() * scale;
        valid_positive(effective_mass)?;
        let h = effective_mass * material(cp.integral(cp.bounds().0, temperature))?.get();
        unit(Joules::new(h))?;
        if temperature.get() > cp.bounds().0.get() && h <= 0.0 {
            return Err("enthalpy underflow".into());
        }
        // Ensure the public inverse is representable before committing a state.
        material(cp.inverse_integral(unit(JoulesPerKilogram::new(h / effective_mass))?))?;
        Ok(Self { cp, mass, scale, h })
    }
    pub fn enthalpy(&self) -> Joules {
        Joules::new(self.h).expect("validated enthalpy")
    }
    pub fn temperature(&self) -> Kelvin {
        self.temperature_for(self.h).expect("validated enthalpy")
    }
    fn temperature_for(&self, h: f64) -> Result<Kelvin> {
        material(self.cp.inverse_integral(unit(JoulesPerKilogram::new(
            h / (self.mass.get() * self.scale),
        ))?))
    }
    /// Enthalpy references each curve’s lower bound. Changing curves therefore
    /// records the corresponding reference reset in the signed event as well.
    pub fn reconfigure(
        &mut self,
        cp: HeatCapacityCurve,
        mass: Kilograms,
        scale: f64,
    ) -> Result<Joules> {
        let replacement = Self::new(cp, mass, scale, self.temperature())?;
        let event = unit(Joules::new(replacement.h - self.h))?;
        *self = replacement;
        Ok(event)
    }
    fn implicit(
        &self,
        h0: f64,
        dt: f64,
        port: Kelvin,
        bridge: &Bridge,
        parasitic: Watts,
    ) -> Result<f64> {
        let (min_t, max_t) = self.cp.bounds();
        let (mut lo, mut hi) = (min_t.get(), max_t.get());
        match bridge {
            Bridge::Conductor { conductivity, .. } => {
                let (a, b) = conductivity.bounds();
                lo = lo.max(a.get());
                hi = hi.min(b.get());
            }
            Bridge::Series {
                conductivity,
                contact,
                area,
                length,
            } => {
                valid_positive(contact.get())?;
                valid_positive(area.get())?;
                valid_positive(length.get())?;
                let (a, b) = conductivity.bounds();
                let body_at = |x| {
                    series_body_temperature(
                        conductivity,
                        port,
                        x,
                        area.get() / length.get(),
                        *contact,
                    )
                };
                lo = lo.max(body_at(a)?);
                hi = hi.min(body_at(b)?);
            }
            Bridge::Table(rows) => {
                if rows.len() < 2 {
                    return Err("invalid conductance table".into());
                }
                lo = lo.max(2.0 * rows[0].0.get() - port.get());
                hi = hi.min(2.0 * rows[rows.len() - 1].0.get() - port.get());
            }
            Bridge::Constant(_) => {}
        }
        if lo > hi {
            return Err("empty bridge domain".into());
        }
        let h_at = |t: f64| -> Result<f64> {
            Ok(self.mass.get()
                * self.scale
                * material(self.cp.integral(min_t, unit(Kelvin::new(t))?))?.get())
        };
        let f = |t: f64| -> Result<f64> {
            let residual = h_at(t)?
                - h0
                - dt * (parasitic.get() - bridge.flow(unit(Kelvin::new(t))?, port)?.get());
            if !residual.is_finite() {
                return Err("non-finite implicit residual".into());
            }
            Ok(residual)
        };
        if f(lo)? > 0.0 || f(hi)? < 0.0 {
            return Err("integration leaves material or bridge range".into());
        }
        for _ in 0..80 {
            let mid = lo.midpoint(hi);
            if f(mid)? > 0.0 {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        h_at(lo.midpoint(hi))
    }
    pub fn step(
        &mut self,
        duration: Seconds,
        sample: PortSample,
        bridge: &Bridge,
        parasitic: Watts,
    ) -> Result<StepReport> {
        valid_positive(duration.get())?;
        valid_positive(sample.temperature.get())?;
        if sample.age.get() < 0.0 {
            return Err("negative sample age".into());
        }
        bridge.flow(self.temperature(), sample.temperature)?;
        let original = self.h;
        let mut h = original;
        let mut elapsed = 0.0;
        let mut integrated = 0.0;
        let mut dt = duration.get();
        // For tabulated G, q'(T)=G+(T-port)*G'/2. Bound its
        // negative part globally; dt <= C_min/(2*bound) makes the implicit
        // residual strictly increasing on every linear segment.
        let max_step = if let Bridge::Table(rows) = bridge {
            if rows.len() < 2 || rows.windows(2).any(|w| w[0].0.get() >= w[1].0.get()) {
                return Err("invalid conductance table".into());
            }
            let (low, high) = self.cp.bounds();
            let distance = (low.get() - sample.temperature.get())
                .abs()
                .max((high.get() - sample.temperature.get()).abs());
            let slope = rows
                .windows(2)
                .map(|w| ((w[1].1.get() - w[0].1.get()) / (w[1].0.get() - w[0].0.get())).abs())
                .fold(0.0_f64, f64::max);
            let bound = slope * distance / 2.0;
            if !bound.is_finite() {
                return Err("conductance slope overflow".into());
            }
            if bound > 0.0 {
                self.mass.get() * self.scale * self.cp.minimum().get() / (2.0 * bound)
            } else {
                f64::INFINITY
            }
        } else {
            f64::INFINITY
        };
        // Backward Euler with step doubling; absolute temperature error 1e-6 K plus
        // relative 1e-7. Work is bounded to 200,000 attempts and failures are atomic.
        for _ in 0..200_000 {
            if elapsed >= duration.get() {
                let net_energy = unit(Joules::new(integrated))?;
                self.temperature_for(h)?;
                self.h = h;
                return Ok(StepReport {
                    net_energy,
                    port_age: sample.age,
                });
            }
            dt = dt.min(duration.get() - elapsed).min(max_step);
            let full = self.implicit(h, dt, sample.temperature, bridge, parasitic)?;
            let half = self.implicit(h, dt / 2.0, sample.temperature, bridge, parasitic)?;
            let fine = self.implicit(half, dt / 2.0, sample.temperature, bridge, parasitic)?;
            let t = self.temperature_for(fine)?.get();
            let error = (t - self.temperature_for(full)?.get()).abs();
            let tol = 1e-6 + 1e-7 * t.abs();
            if error <= tol {
                let q_half = bridge
                    .flow(self.temperature_for(half)?, sample.temperature)?
                    .get();
                let q_fine = bridge
                    .flow(self.temperature_for(fine)?, sample.temperature)?
                    .get();
                integrated += dt * (parasitic.get() - (q_half + q_fine) / 2.0);
                h = fine;
                elapsed += dt;
            }
            let factor = if error == 0.0 {
                2.0
            } else {
                (0.9 * (tol / error).sqrt()).clamp(0.1, 2.0)
            };
            dt *= factor;
            if dt <= f64::EPSILON * duration.get().max(1.0) {
                return Err("integrator step underflow".into());
            }
        }
        Err("integrator iteration limit".into())
    }
}

pub struct InventoryPart {
    pub mass: Kilograms,
    pub temperature: Kelvin,
    pub heat_capacity: HeatCapacityCurve,
}
pub fn fixture_heat_capacity(parts: &[InventoryPart]) -> Result<JoulesPerKelvin> {
    let mut c = 0.0;
    for p in parts {
        valid_positive(p.mass.get())?;
        c += p.mass.get() * material(p.heat_capacity.value(p.temperature))?.get();
    }
    unit(JoulesPerKelvin::new(c))
}
#[derive(Clone, Copy)]
pub enum EmulationMode {
    AdditionalLoad,
    FixtureCompensation {
        capacity: JoulesPerKelvin,
        temperature_rate: KelvinPerSecond,
        parasitic: Watts,
    },
}
pub struct PowerRequest {
    pub requested: Watts,
    pub nonnegative_target: Watts,
    pub negative_clamped: bool,
}
pub fn request(mode: EmulationMode, flow: Watts) -> Result<PowerRequest> {
    let p = match mode {
        EmulationMode::AdditionalLoad => flow.get(),
        EmulationMode::FixtureCompensation {
            capacity,
            temperature_rate,
            parasitic,
        } => {
            if capacity.get() < 0.0 {
                return Err("negative fixture capacity".into());
            }
            flow.get() + capacity.get() * temperature_rate.get() - parasitic.get()
        }
    };
    Ok(PowerRequest {
        requested: unit(Watts::new(p))?,
        nonnegative_target: unit(Watts::new(p.max(0.0)))?,
        negative_clamped: p < 0.0,
    })
}
pub fn balance(
    heaters: &[Watts],
    wire: Watts,
    radiation: Watts,
    other: Watts,
    cf: JoulesPerKelvin,
    derivative: KelvinPerSecond,
) -> Result<Watts> {
    if cf.get() < 0.0 {
        return Err("negative fixture capacity".into());
    }
    unit(Watts::new(
        heaters.iter().map(|p| p.get()).sum::<f64>() + wire.get() + radiation.get() + other.get()
            - cf.get() * derivative.get(),
    ))
}
