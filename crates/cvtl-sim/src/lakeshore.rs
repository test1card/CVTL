// FILE: crates/cvtl-sim/src/lakeshore.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Emulate Lake Shore 218 thermometry
//   SCOPE: Loopback replies, injectable faults and atomic test temperature frames
//   DEPENDS: M-CVTL-CORE, loopback
//   LINKS: M-CVTL-SIM
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   LakeShoreSim - loopback temperature emulator
//   Fault - synthetic response and connection fault
// END_MODULE_MAP
//! Loopback-only subset of the Model 218 query protocol.
use crate::loopback::{LoopbackServer, Reply};
use cvtl_core::units::Kelvin;
use std::{
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

#[derive(Clone, Copy, Debug)]
pub enum Fault {
    None,
    Status(u8),
    MalformedReading,
    Disconnect,
    Silence,
}

struct State {
    fault: Fault,
    temperatures: [Kelvin; 8],
    next_kelvin: Option<(u8, Kelvin)>,
    next_status_delay: Option<(u8, Duration)>,
}

pub struct LakeShoreSim {
    server: LoopbackServer,
    state: Arc<Mutex<State>>,
}
impl LakeShoreSim {
    pub fn spawn() -> io::Result<Self> {
        let state = Arc::new(Mutex::new(State {
            fault: Fault::None,
            temperatures: [Kelvin::new(42.0).unwrap(); 8],
            next_kelvin: None,
            next_status_delay: None,
        }));
        let shared = Arc::clone(&state);
        let server = LoopbackServer::spawn(move |command| {
            let mut state = shared.lock().unwrap();
            let fault = state.fault;
            match fault {
                Fault::Disconnect => return Reply::Disconnect,
                Fault::Silence => return Reply::Silence,
                _ => {}
            }
            let (op, channel) = match command.split_once(' ') {
                Some((op, channel)) => {
                    let Ok(channel @ 1..=8) = channel.parse::<u8>() else {
                        return Reply::NoResponse;
                    };
                    (op, Some(channel))
                }
                None => (command, None),
            };
            let mut delay = None;
            let response = match op {
                "RDGST?" => {
                    let status = match fault {
                        Fault::Status(v) => v,
                        _ => 0,
                    };
                    match channel {
                        Some(channel) => {
                            if status == 0 {
                                delay = state
                                    .next_status_delay
                                    .take_if(|(target, _)| *target == channel)
                                    .map(|(_, delay)| delay);
                            }
                            format!("{status}\n")
                        }
                        None => {
                            delay = state.next_status_delay.take().map(|(_, delay)| delay);
                            format!(
                                "{}\n",
                                std::iter::repeat_n(format!("{status:03}"), 8)
                                    .collect::<Vec<_>>()
                                    .join(",")
                            )
                        }
                    }
                }
                "KRDG?" | "SRDG?" if matches!(fault, Fault::MalformedReading) => "bad\n".into(),
                "KRDG?" => match channel {
                    Some(channel) => {
                        let value = state
                            .next_kelvin
                            .take_if(|(target, _)| *target == channel)
                            .map_or(
                                state.temperatures[usize::from(channel) - 1],
                                |(_, value)| value,
                            );
                        format!("{:+.3}\n", value.get())
                    }
                    None => {
                        let mut values = state
                            .temperatures
                            .iter()
                            .map(|value| format!("{:+.3}", value.get()))
                            .collect::<Vec<_>>();
                        if let Some((target, value)) = state.next_kelvin.take() {
                            values[usize::from(target) - 1] = format!("{:+.3}", value.get());
                        }
                        format!("{}\n", values.join(","))
                    }
                },
                "SRDG?" => match channel {
                    Some(_) => "+1.234\n".into(),
                    None => format!("{}\n", ["+1.234"; 8].join(",")),
                },
                _ => return Reply::NoResponse,
            };
            drop(state);
            if let Some(delay) = delay {
                thread::sleep(delay);
            }
            Reply::Bytes(response.into_bytes())
        })?;
        Ok(Self { server, state })
    }
    pub fn addr(&self) -> SocketAddr {
        self.server.addr()
    }
    pub fn set_fault(&self, fault: Fault) {
        self.state.lock().unwrap().fault = fault;
    }

    /// Replace the next numeric `KRDG?` response on this channel.
    pub fn override_next_kelvin(&self, channel: u8, value: Kelvin) -> io::Result<()> {
        validate_channel(channel)?;
        if value.get() < 0.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "negative simulated temperature",
            ));
        }
        self.state.lock().unwrap().next_kelvin = Some((channel, value));
        Ok(())
    }

    /// Delay the next zero-status `RDGST?` response on this channel.
    pub fn delay_next_status(&self, channel: u8, delay: Duration) -> io::Result<()> {
        validate_channel(channel)?;
        self.state.lock().unwrap().next_status_delay = Some((channel, delay));
        Ok(())
    }

    /// Publish one synthetic temperature frame in channel order 1–8.
    pub fn set_temperatures(&self, temperatures: [Kelvin; 8]) -> io::Result<()> {
        if temperatures.iter().any(|t| t.get() < 0.0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "negative simulated temperature",
            ));
        }
        self.state.lock().unwrap().temperatures = temperatures;
        Ok(())
    }
}

fn validate_channel(channel: u8) -> io::Result<()> {
    if (1..=8).contains(&channel) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "channel must be 1..8",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cvtl_instruments::{LoopbackTcp, MessageTransport};
    use std::time::{Duration, Instant};

    fn query(transport: &mut LoopbackTcp, command: &str) -> Vec<u8> {
        transport
            .query(command, Instant::now() + Duration::from_secs(2))
            .unwrap()
    }

    #[test]
    fn kelvin_override_applies_once_to_a_matching_reading() {
        let sim = LakeShoreSim::spawn().unwrap();
        let mut transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(2)).unwrap();
        sim.override_next_kelvin(2, Kelvin::new(47.0).unwrap())
            .unwrap();
        assert_eq!(query(&mut transport, "KRDG? 1"), b"+42.000");
        assert_eq!(query(&mut transport, "SRDG? 2"), b"+1.234");
        sim.set_fault(Fault::MalformedReading);
        assert_eq!(query(&mut transport, "KRDG? 2"), b"bad");
        sim.set_fault(Fault::None);
        sim.set_temperatures([Kelvin::new(39.0).unwrap(); 8])
            .unwrap();
        assert_eq!(query(&mut transport, "KRDG? 2"), b"+47.000");
        assert_eq!(query(&mut transport, "KRDG? 2"), b"+39.000");
    }

    #[test]
    fn status_delay_applies_once_to_a_matching_healthy_status() {
        let sim = LakeShoreSim::spawn().unwrap();
        let mut transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(2)).unwrap();
        let delay = Duration::from_millis(40);
        sim.delay_next_status(2, delay).unwrap();
        assert_eq!(query(&mut transport, "RDGST? 1"), b"0");
        assert_eq!(query(&mut transport, "KRDG? 2"), b"+42.000");
        sim.set_fault(Fault::Status(16));
        assert_eq!(query(&mut transport, "RDGST? 2"), b"16");
        sim.set_fault(Fault::None);
        let started = Instant::now();
        assert_eq!(query(&mut transport, "RDGST? 2"), b"0");
        assert!(started.elapsed() >= delay);
        assert!(sim.state.lock().unwrap().next_status_delay.is_none());
        assert_eq!(query(&mut transport, "RDGST? 2"), b"0");
    }

    #[test]
    fn batch_queries_return_all_eight_channels() {
        let sim = LakeShoreSim::spawn().unwrap();
        let mut transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(2)).unwrap();
        assert_eq!(
            query(&mut transport, "KRDG?"),
            b"+42.000,+42.000,+42.000,+42.000,+42.000,+42.000,+42.000,+42.000"
        );
        assert_eq!(
            query(&mut transport, "RDGST?"),
            b"000,000,000,000,000,000,000,000"
        );
        assert_eq!(
            query(&mut transport, "SRDG?"),
            b"+1.234,+1.234,+1.234,+1.234,+1.234,+1.234,+1.234,+1.234"
        );
    }

    #[test]
    fn one_shots_validate_channel_and_temperature() {
        let sim = LakeShoreSim::spawn().unwrap();
        let kelvin = Kelvin::new(47.0).unwrap();
        let delay = Duration::from_millis(1);
        for channel in [0, 9] {
            assert_eq!(
                sim.override_next_kelvin(channel, kelvin)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                sim.delay_next_status(channel, delay).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(
            sim.override_next_kelvin(2, Kelvin::new(-1.0).unwrap())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
