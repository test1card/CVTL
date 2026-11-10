//! Keithley 2600B TSP subset. Rev. F, §§2 and 9: current source and
//! four-wire sensing p. 2-17; limits/ranges p. 2-48; NPLC p. 2-50.
//! `limiti` is voltage-source compliance, so current is also bounded locally.
//! Output readback is the instrument's reported state, not electrical proof.
use crate::{InstrumentError, MessageTransport, Result};
use cvtl_core::units::{Amperes, Volts, Watts};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    A,
    B,
}
impl Channel {
    fn name(self) -> &'static str {
        match self {
            Self::A => "smua",
            Self::B => "smub",
        }
    }
    fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputState {
    On,
    Off,
    Unknown,
}
#[derive(Clone, Copy, Debug)]
pub struct ChannelConfig {
    pub limit_v: Volts,
    pub limit_i: Amperes,
    pub limit_p: Watts,
    pub source_range_i: Amperes,
    pub measure_range_i: Amperes,
    pub measure_range_v: Volts,
    pub nplc: f64,
}
#[derive(Clone, Copy, Debug)]
pub struct Measurement {
    pub current_a: Amperes,
    pub voltage_v: Volts,
    pub nplc: f64,
    pub started: Instant,
    pub finished: Instant,
}

pub trait SourceChannel {
    fn configure(
        &mut self,
        channel: Channel,
        config: ChannelConfig,
        deadline: Instant,
    ) -> Result<()>;
    fn set_current(&mut self, channel: Channel, current: Amperes, deadline: Instant) -> Result<()>;
    fn measure(&mut self, channel: Channel, deadline: Instant) -> Result<Measurement>;
    fn output_on(&mut self, channel: Channel, deadline: Instant) -> Result<()>;
    fn output_off(&mut self, channel: Channel, deadline: Instant) -> Result<()>;
    fn output_state(&mut self, channel: Channel, deadline: Instant) -> Result<OutputState>;
    fn errors(&mut self, deadline: Instant) -> Result<String>;
    fn identity(&mut self, deadline: Instant) -> Result<String>;
}

/// Parse a single finite TSP ASCII number; overflow sentinels are rejected.
pub fn parse_number(bytes: &[u8]) -> Result<f64> {
    if bytes.iter().any(u8::is_ascii_control) {
        return Err(InstrumentError::InvalidResponse("number control byte"));
    }
    let s = std::str::from_utf8(bytes)
        .map_err(|_| InstrumentError::InvalidResponse("non-UTF-8 number"))?;
    let x: f64 = s
        .trim()
        .parse()
        .map_err(|_| InstrumentError::InvalidResponse("invalid number"))?;
    let mantissa = s.trim().split(['e', 'E']).next().unwrap_or("");
    if x == 0. && mantissa.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        return Err(InstrumentError::InvalidResponse("numeric underflow"));
    }
    if !x.is_finite() || x.abs() >= 9.9e37 {
        return Err(InstrumentError::InvalidResponse("nonfinite or overflow"));
    }
    Ok(x)
}
/// `measure.iv()` returns current first, then voltage (Rev. F p. 9-233); print uses tabs (p. 9-170).
/// The M3 wire profile requires unpadded fields. Firmware compatibility is a Phase B check.
pub fn parse_iv(bytes: &[u8]) -> Result<(f64, f64)> {
    let s =
        std::str::from_utf8(bytes).map_err(|_| InstrumentError::InvalidResponse("non-UTF-8 IV"))?;
    let mut parts = s.split('\t');
    let i = parts
        .next()
        .ok_or(InstrumentError::InvalidResponse("missing current"))?;
    let v = parts
        .next()
        .ok_or(InstrumentError::InvalidResponse("missing voltage"))?;
    if parts.next().is_some() {
        return Err(InstrumentError::InvalidResponse("extra IV fields"));
    }
    if i.trim() != i || v.trim() != v {
        return Err(InstrumentError::InvalidResponse("IV whitespace"));
    }
    Ok((parse_number(i.as_bytes())?, parse_number(v.as_bytes())?))
}

/// Validate the four fields of a 2604B SCPI identity response.
pub fn parse_identity(bytes: &[u8]) -> Result<String> {
    if bytes.iter().any(u8::is_ascii_control) {
        return Err(InstrumentError::InvalidResponse("identity control byte"));
    }
    let s = std::str::from_utf8(bytes)
        .map_err(|_| InstrumentError::InvalidResponse("identity encoding"))?;
    let parts: Vec<_> = s.split(',').map(str::trim).collect();
    if parts.len() != 4
        || parts[0] != "KEITHLEY INSTRUMENTS INC."
        || !matches!(parts[1], "2604B" | "MODEL 2604B")
        || parts
            .iter()
            .any(|x| x.is_empty() || x.contains(['\n', '\r', '\t']))
    {
        return Err(InstrumentError::InvalidResponse("identity"));
    }
    Ok(s.to_owned())
}
/// Validate all four TSP error queue fields, with integral scientific notation.
pub fn parse_error(bytes: &[u8]) -> Result<String> {
    if bytes.iter().any(|b| b.is_ascii_control() && *b != b'\t') {
        return Err(InstrumentError::InvalidResponse("error control byte"));
    }
    let s = std::str::from_utf8(bytes)
        .map_err(|_| InstrumentError::InvalidResponse("error encoding"))?;
    let parts: Vec<_> = s.split('\t').collect();
    if parts.len() != 4 || parts[1].is_empty() || parts[1].contains(['\n', '\r']) {
        return Err(InstrumentError::InvalidResponse("error fields"));
    }
    for i in [0, 2, 3] {
        let x = parse_number(parts[i].as_bytes())?;
        if x.fract() != 0. {
            return Err(InstrumentError::InvalidResponse("nonintegral error field"));
        }
    }
    Ok(s.to_owned())
}

pub struct Keithley<T: MessageTransport> {
    transport: T,
    configs: [Option<ChannelConfig>; 2],
    recovered: bool,
}
impl<T: MessageTransport> Keithley<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            configs: [None, None],
            recovered: false,
        }
    }
    fn fail(&mut self) {
        self.recovered = false;
        self.configs = [None, None];
        self.transport.invalidate();
    }
    fn send(&mut self, s: &str, d: Instant) -> Result<()> {
        match self.transport.send(s, d) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.fail();
                Err(e)
            }
        }
    }
    fn query(&mut self, s: &str, d: Instant) -> Result<Vec<u8>> {
        match self.transport.query(s, d) {
            Ok(x) => Ok(x),
            Err(e) => {
                self.fail();
                Err(e)
            }
        }
    }
    fn number(&mut self, s: &str, d: Instant) -> Result<f64> {
        let x = self.query(s, d)?;
        match parse_number(&x) {
            Ok(x) => Ok(x),
            Err(e) => {
                self.fail();
                Err(e)
            }
        }
    }
    fn require_ready(&self) -> Result<()> {
        if self.recovered && self.transport.is_usable() {
            Ok(())
        } else {
            Err(InstrumentError::State("explicit recovery required"))
        }
    }
    /// Recovery never replays configuration or enables either channel.
    pub fn recover(&mut self, d: Instant) -> Result<()> {
        self.recovered = false;
        self.configs = [None, None];
        self.identity(d)?;
        self.off_pair(d)?;
        self.recovered = true;
        Ok(())
    }
    /// Issue OFF on both channels before waiting for either logical readback.
    pub fn disable_both(&mut self, d: Instant) -> Result<()> {
        self.require_ready()?;
        self.off_pair(d)
    }
    fn off_pair(&mut self, d: Instant) -> Result<()> {
        for c in [Channel::A, Channel::B] {
            let n = c.name();
            self.send(&format!("{n}.source.output = {n}.OUTPUT_OFF"), d)?;
        }
        for c in [Channel::A, Channel::B] {
            if self.reported(c, d)? != OutputState::Off {
                self.fail();
                return Err(InstrumentError::Unverified);
            }
        }
        Ok(())
    }
    fn reported(&mut self, c: Channel, d: Instant) -> Result<OutputState> {
        let x = self.number(&format!("print({}.source.output)", c.name()), d)?;
        match x {
            0. => Ok(OutputState::Off),
            1. => Ok(OutputState::On),
            _ => {
                self.fail();
                Err(InstrumentError::InvalidResponse("output state"))
            }
        }
    }
    pub fn measure_pair(&mut self, d: Instant) -> Result<(Measurement, Measurement)> {
        let a = self.measure(Channel::A, d)?;
        let b = self.measure(Channel::B, d)?;
        Ok((a, b))
    }
}
impl<T: MessageTransport> SourceChannel for Keithley<T> {
    fn configure(&mut self, c: Channel, cfg: ChannelConfig, d: Instant) -> Result<()> {
        self.require_ready()?;
        if [
            cfg.limit_v.get(),
            cfg.limit_i.get(),
            cfg.limit_p.get(),
            cfg.source_range_i.get(),
            cfg.measure_range_i.get(),
            cfg.measure_range_v.get(),
        ]
        .iter()
        .any(|x| !x.is_finite() || *x <= 0.)
            || !cfg.nplc.is_finite()
            || !(0.001..=25.).contains(&cfg.nplc)
        {
            return Err(InstrumentError::InvalidInput(
                "invalid limits, ranges or NPLC",
            ));
        }
        // Rev. F pp. 2-44–46, 9-230 and Appendix B: nominal 2604B ranges.
        let current_ranges = [1e-7, 1e-6, 1e-5, 1e-4, 1e-3, 1e-2, 1e-1, 1., 3.];
        let voltage_ranges = [0.1, 1., 6., 40.];
        if !current_ranges.contains(&cfg.source_range_i.get())
            || !voltage_ranges.contains(&cfg.measure_range_v.get())
            || (cfg.source_range_i.get() == 3. && cfg.measure_range_v.get() > 6.)
        {
            return Err(InstrumentError::InvalidInput(
                "unsupported 2604B nominal range",
            ));
        }
        if cfg.source_range_i != cfg.measure_range_i {
            return Err(InstrumentError::InvalidInput(
                "current source and measurement ranges must match",
            ));
        }
        let nominal_v = if cfg.source_range_i.get() == 3. {
            6.
        } else {
            40.
        };
        if cfg.limit_i > cfg.source_range_i
            || cfg.limit_v.get() > nominal_v
            || cfg.limit_p.get() > 40.4
        {
            return Err(InstrumentError::InvalidInput(
                "limits exceed the selected nominal region",
            ));
        }
        if self.reported(c, d)? != OutputState::Off {
            return Err(InstrumentError::State("configure requires output off"));
        }
        self.configs[c.index()] = None;
        let n = c.name();
        let fields = [
            ("source.func", format!("{n}.OUTPUT_DCAMPS")),
            ("sense", format!("{n}.SENSE_REMOTE")),
            ("source.autorangei", format!("{n}.AUTORANGE_OFF")),
            ("measure.autorangei", format!("{n}.AUTORANGE_OFF")),
            ("measure.autorangev", format!("{n}.AUTORANGE_OFF")),
            ("source.limitv", cfg.limit_v.get().to_string()),
            ("source.limiti", cfg.limit_i.get().to_string()),
            ("source.limitp", cfg.limit_p.get().to_string()),
            ("source.rangei", cfg.source_range_i.get().to_string()),
            ("measure.rangev", cfg.measure_range_v.get().to_string()),
            ("measure.nplc", cfg.nplc.to_string()),
            ("source.leveli", "0".to_owned()),
        ];
        for (key, value) in &fields {
            self.send(&format!("{n}.{key} = {value}"), d)?;
        }
        for (key, value) in fields {
            let value_read = self.number(&format!("print({n}.{key})"), d)?;
            let expected = match value.parse::<f64>() {
                Ok(v) => v,
                Err(_) => self.number(&format!("print({value})"), d)?,
            };
            if (value_read - expected).abs() > 1e-9 * expected.abs() {
                self.fail();
                return Err(InstrumentError::Unverified);
            }
        }
        let measure_i = self.number(&format!("print({n}.measure.rangei)"), d)?;
        if (measure_i - cfg.measure_range_i.get()).abs() > 1e-9 * cfg.measure_range_i.get().abs() {
            self.fail();
            return Err(InstrumentError::Unverified);
        }
        self.configs[c.index()] = Some(cfg);
        Ok(())
    }
    fn set_current(&mut self, c: Channel, x: Amperes, d: Instant) -> Result<()> {
        self.require_ready()?;
        let cfg = self.configs[c.index()].ok_or(InstrumentError::Unverified)?;
        let x = x.get();
        if x < 0. || x > cfg.limit_i.get() || x > cfg.source_range_i.get() {
            return Err(InstrumentError::InvalidInput(
                "current exceeds configured bound",
            ));
        }
        let n = c.name();
        self.send(&format!("{n}.source.leveli = {x}"), d)?;
        let read = self.number(&format!("print({n}.source.leveli)"), d)?;
        if (read - x).abs() > 1e-9 * x.abs() {
            self.fail();
            return Err(InstrumentError::Unverified);
        }
        Ok(())
    }
    fn measure(&mut self, c: Channel, d: Instant) -> Result<Measurement> {
        self.require_ready()?;
        let cfg = self.configs[c.index()].ok_or(InstrumentError::Unverified)?;
        let started = Instant::now();
        let bytes = self.query(&format!("print({}.measure.iv())", c.name()), d)?;
        let finished = Instant::now();
        let (current_a, voltage_v) = match parse_iv(&bytes) {
            Ok(v) => v,
            Err(e) => {
                self.fail();
                return Err(e);
            }
        };
        Ok(Measurement {
            current_a: Amperes::new(current_a)
                .ok_or(InstrumentError::InvalidResponse("current"))?,
            voltage_v: Volts::new(voltage_v).ok_or(InstrumentError::InvalidResponse("voltage"))?,
            nplc: cfg.nplc,
            started,
            finished,
        })
    }
    fn output_on(&mut self, c: Channel, d: Instant) -> Result<()> {
        self.require_ready()?;
        if self.configs[c.index()].is_none() {
            return Err(InstrumentError::Unverified);
        }
        let n = c.name();
        self.send(&format!("{n}.source.output = {n}.OUTPUT_ON"), d)?;
        if self.reported(c, d)? != OutputState::On {
            self.fail();
            return Err(InstrumentError::Unverified);
        }
        Ok(())
    }
    fn output_off(&mut self, c: Channel, d: Instant) -> Result<()> {
        self.require_ready()?;
        let n = c.name();
        self.send(&format!("{n}.source.output = {n}.OUTPUT_OFF"), d)?;
        if self.reported(c, d)? != OutputState::Off {
            self.fail();
            return Err(InstrumentError::Unverified);
        }
        Ok(())
    }
    fn output_state(&mut self, c: Channel, d: Instant) -> Result<OutputState> {
        if !self.recovered || !self.transport.is_usable() {
            return Ok(OutputState::Unknown);
        }
        self.reported(c, d)
    }
    fn errors(&mut self, d: Instant) -> Result<String> {
        self.require_ready()?;
        let bytes = self.query("print(errorqueue.next())", d)?;
        match parse_error(&bytes) {
            Ok(x) => Ok(x),
            Err(e) => {
                self.fail();
                Err(e)
            }
        }
    }
    fn identity(&mut self, d: Instant) -> Result<String> {
        let bytes = self.query("*IDN?", d)?;
        match parse_identity(&bytes) {
            Ok(x) => Ok(x),
            Err(e) => {
                self.fail();
                Err(e)
            }
        }
    }
}
