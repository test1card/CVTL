//! Model 218 manual rev. 2.5, §6: KRDG?, SRDG? and RDGST?.
//! Sensor units are selected by caller configuration, not returned by SRDG?.
use crate::{InstrumentError, MessageTransport, Result};
use cvtl_core::units::{Kelvin, Ohms, Volts};
use std::time::Instant;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Channel(u8);
impl Channel {
    pub fn new(value: u8) -> Result<Self> {
        if (1..=8).contains(&value) {
            Ok(Self(value))
        } else {
            Err(InstrumentError::InvalidInput("channel must be 1..8"))
        }
    }
    pub fn number(self) -> u8 {
        self.0
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SensorUnit {
    Volts,
    Ohms,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SensorReading {
    Volts(Volts),
    Ohms(Ohms),
}
#[derive(Clone, Copy, Debug)]
pub struct TemperatureReading {
    pub channel: Channel,
    pub kelvin: Kelvin,
    pub sensor: SensorReading,
    pub started: Instant,
    pub finished: Instant,
    /// The instrument does not provide a conversion timestamp in this protocol.
    pub measurement_time: Option<Instant>,
}

/// Parse a single finite decimal response (no lists, exponent or special tokens).
pub fn parse_reading(bytes: &[u8]) -> Result<f64> {
    let s = std::str::from_utf8(bytes)
        .map_err(|_| InstrumentError::InvalidResponse("non-ASCII reading"))?
        .trim();
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    let mut parts = body.split('.');
    let whole = parts.next().unwrap_or("");
    let fraction = parts.next();
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.is_none_or(|v| v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()))
        || parts.next().is_some()
    {
        return Err(InstrumentError::InvalidResponse("malformed reading"));
    }
    let value: f64 = s
        .parse()
        .map_err(|_| InstrumentError::InvalidResponse("malformed reading"))?;
    if !value.is_finite() {
        return Err(InstrumentError::InvalidResponse("nonfinite reading"));
    }
    Ok(value)
}
/// RDGST? is an unsigned byte; unknown/reserved set bits are rejected by read.
pub fn parse_status(bytes: &[u8]) -> Result<u8> {
    let s = std::str::from_utf8(bytes)
        .map_err(|_| InstrumentError::InvalidResponse("invalid status"))?
        .trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(InstrumentError::InvalidResponse("invalid status"));
    }
    s.parse()
        .map_err(|_| InstrumentError::InvalidResponse("invalid status"))
}
pub trait Thermometer {
    fn read(
        &mut self,
        channel: Channel,
        unit: SensorUnit,
        deadline: Instant,
    ) -> Result<TemperatureReading>;
}
pub struct LakeShore218<T> {
    transport: T,
}
impl<T: MessageTransport> LakeShore218<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }
    pub fn is_usable(&self) -> bool {
        self.transport.is_usable()
    }
    /// Recovery requires a fresh, explicitly provided transport.
    pub fn recover(&mut self, transport: T) {
        self.transport = transport;
    }
    fn status(&mut self, channel: Channel, deadline: Instant) -> Result<()> {
        let response = self
            .transport
            .query(&format!("RDGST? {}", channel.0), deadline)?;
        if parse_status(&response)? != 0 {
            return Err(InstrumentError::InvalidResponse("input status not clear"));
        }
        Ok(())
    }
}
impl<T: MessageTransport> Thermometer for LakeShore218<T> {
    fn read(
        &mut self,
        channel: Channel,
        unit: SensorUnit,
        deadline: Instant,
    ) -> Result<TemperatureReading> {
        if !self.transport.is_usable() {
            return Err(InstrumentError::Disconnected);
        }
        let started = Instant::now();
        let result = (|| {
            self.status(channel, deadline)?;
            let kelvin = parse_reading(
                &self
                    .transport
                    .query(&format!("KRDG? {}", channel.0), deadline)?,
            )?;
            let value = parse_reading(
                &self
                    .transport
                    .query(&format!("SRDG? {}", channel.0), deadline)?,
            )?;
            self.status(channel, deadline)?;
            if kelvin <= 0.0 {
                return Err(InstrumentError::InvalidResponse("nonpositive kelvin"));
            }
            Ok(TemperatureReading {
                channel,
                kelvin: Kelvin::new(kelvin)
                    .ok_or(InstrumentError::InvalidResponse("invalid kelvin"))?,
                sensor: match unit {
                    SensorUnit::Volts => SensorReading::Volts(
                        Volts::new(value)
                            .ok_or(InstrumentError::InvalidResponse("invalid voltage"))?,
                    ),
                    SensorUnit::Ohms => SensorReading::Ohms(
                        Ohms::new(value)
                            .ok_or(InstrumentError::InvalidResponse("invalid resistance"))?,
                    ),
                },
                started,
                finished: Instant::now(),
                measurement_time: None,
            })
        })();
        if result.is_err() {
            self.transport.invalidate();
        }
        result
    }
}
