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
pub fn parse_reading_list(bytes: &[u8]) -> Result<[f64; 8]> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| InstrumentError::InvalidResponse("non-ASCII reading list"))?;
    let mut values = [0.0f64; 8];
    let mut count = 0;
    for field in text.split(',') {
        if count == values.len() {
            return Err(InstrumentError::InvalidResponse("too many readings"));
        }
        values[count] = parse_reading(field.as_bytes())?;
        count += 1;
    }
    if count != values.len() {
        return Err(InstrumentError::InvalidResponse("expected eight readings"));
    }
    Ok(values)
}

pub fn parse_status_list(bytes: &[u8]) -> Result<[u8; 8]> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| InstrumentError::InvalidResponse("non-ASCII status list"))?;
    let mut values = [0u8; 8];
    let mut count = 0;
    for field in text.split(',') {
        if count == values.len() {
            return Err(InstrumentError::InvalidResponse("too many statuses"));
        }
        values[count] = parse_status(field.as_bytes())?;
        count += 1;
    }
    if count != values.len() {
        return Err(InstrumentError::InvalidResponse("expected eight statuses"));
    }
    Ok(values)
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
    fn batch_status(&mut self, deadline: Instant) -> Result<[u8; 8]> {
        let response = self.transport.query("RDGST?", deadline)?;
        parse_status_list(&response)
    }
    fn batch_values(&mut self, command: &str, deadline: Instant) -> Result<[f64; 8]> {
        let response = self.transport.query(command, deadline)?;
        parse_reading_list(&response)
    }
    /// Read all requested channels from one batch status/reading/status exchange.
    ///
    /// The real 218S returns the same first-channel value for `KRDG? <n>` but
    /// returns all eight values for a bare `KRDG?`. Batch is therefore the only
    /// correct way to address an individual input on this firmware.
    pub fn read_channels(
        &mut self,
        channels: &[Channel],
        unit: SensorUnit,
        deadline: Instant,
    ) -> Result<Vec<Result<TemperatureReading>>> {
        if !self.transport.is_usable() {
            return Err(InstrumentError::Disconnected);
        }
        if channels.is_empty() {
            return Ok(Vec::new());
        }
        let started = Instant::now();
        let result = (|| {
            let before = self.batch_status(deadline)?;
            let kelvin = self.batch_values("KRDG?", deadline)?;
            let sensor = self.batch_values("SRDG?", deadline)?;
            let after = self.batch_status(deadline)?;
            Ok((before, kelvin, sensor, after))
        })();
        let (before, kelvin, sensor, after) = match result {
            Ok(value) => value,
            Err(error) => {
                self.transport.invalidate();
                return Err(error);
            }
        };
        let finished = Instant::now();
        Ok(channels
            .iter()
            .map(|channel| {
                let index = usize::from(channel.0 - 1);
                if before[index] != 0 || after[index] != 0 {
                    return Err(InstrumentError::InvalidResponse("input status not clear"));
                }
                if kelvin[index] <= 0.0 {
                    return Err(InstrumentError::InvalidResponse("nonpositive kelvin"));
                }
                Ok(TemperatureReading {
                    channel: *channel,
                    kelvin: Kelvin::new(kelvin[index])
                        .ok_or(InstrumentError::InvalidResponse("invalid kelvin"))?,
                    sensor: match unit {
                        SensorUnit::Volts => SensorReading::Volts(
                            Volts::new(sensor[index])
                                .ok_or(InstrumentError::InvalidResponse("invalid voltage"))?,
                        ),
                        SensorUnit::Ohms => SensorReading::Ohms(
                            Ohms::new(sensor[index])
                                .ok_or(InstrumentError::InvalidResponse("invalid resistance"))?,
                        ),
                    },
                    started,
                    finished,
                    measurement_time: None,
                })
            })
            .collect())
    }
}
impl<T: MessageTransport> Thermometer for LakeShore218<T> {
    fn read(
        &mut self,
        channel: Channel,
        unit: SensorUnit,
        deadline: Instant,
    ) -> Result<TemperatureReading> {
        let mut readings = self.read_channels(&[channel], unit, deadline)?;
        match readings.pop() {
            Some(Ok(reading)) => Ok(reading),
            Some(Err(error)) => {
                // The one-channel API keeps the original contract: a rejected
                // input makes the whole session unusable. `read_channels`
                // reports per-channel failures for a multi-channel frame.
                self.transport.invalidate();
                Err(error)
            }
            None => Err(InstrumentError::Disconnected),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Fake {
        responses: VecDeque<Vec<u8>>,
        invalidated: bool,
    }

    impl Fake {
        fn new(responses: &[&[u8]]) -> Self {
            Self {
                responses: responses.iter().map(|value| value.to_vec()).collect(),
                invalidated: false,
            }
        }
    }

    impl MessageTransport for Fake {
        fn send(&mut self, _command: &str, _deadline: Instant) -> Result<()> {
            Ok(())
        }
        fn query(&mut self, _command: &str, _deadline: Instant) -> Result<Vec<u8>> {
            Ok(self.responses.pop_front().expect("scripted response"))
        }
        fn invalidate(&mut self) {
            self.invalidated = true;
        }
        fn is_usable(&self) -> bool {
            !self.invalidated
        }
    }

    const STATUS: &[u8] = b"000,000,000,032,000,000,000,000\r";
    const KELVIN: &[u8] = b"+298.03,+297.97,+301.92,+380.00,+323.23,+298.01,+298.61,+00.000\r";
    const SENSOR: &[u8] = b"+0.5629,+0.5649,+0.5552,+0.0232,+0.5059,+0.5642,+0.5611,+0.0550\r";

    fn deadline() -> Instant {
        Instant::now() + std::time::Duration::from_secs(1)
    }

    #[test]
    fn batch_lists_parse_eight_values() {
        let kelvin = parse_reading_list(KELVIN).unwrap();
        assert_eq!(kelvin[1], 297.97);
        let statuses = parse_status_list(STATUS).unwrap();
        assert_eq!(statuses[3], 32);
        assert!(parse_reading_list(b"+1,+2").is_err());
        assert!(parse_status_list(b"0,0,0").is_err());
    }

    #[test]
    fn one_batch_selects_channels_and_rejects_a_bad_status() {
        let mut driver = LakeShore218::new(Fake::new(&[STATUS, KELVIN, SENSOR, STATUS]));
        let readings = driver
            .read_channels(
                &[Channel::new(2).unwrap(), Channel::new(4).unwrap()],
                SensorUnit::Volts,
                deadline(),
            )
            .unwrap();
        let second = readings[0].as_ref().unwrap();
        assert_eq!(second.channel.number(), 2);
        assert_eq!(second.kelvin.get(), 297.97);
        match second.sensor {
            SensorReading::Volts(value) => assert_eq!(value.get(), 0.5649),
            SensorReading::Ohms(_) => panic!("expected volts"),
        }
        assert!(readings[1].is_err());
        assert!(!driver.transport.invalidated);
    }

    #[test]
    fn malformed_batch_invalidates_the_session() {
        let mut driver = LakeShore218::new(Fake::new(&[STATUS, b"bad\r"]));
        let error = driver
            .read_channels(&[Channel::new(1).unwrap()], SensorUnit::Volts, deadline())
            .unwrap_err();
        assert!(matches!(error, InstrumentError::InvalidResponse(_)));
        assert!(driver.transport.invalidated);
    }
}
