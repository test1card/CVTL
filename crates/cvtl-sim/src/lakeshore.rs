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
        }));
        let shared = Arc::clone(&state);
        let server = LoopbackServer::spawn(move |command| {
            let state = shared.lock().unwrap();
            let fault = state.fault;
            match fault {
                Fault::Disconnect => return Reply::Disconnect,
                Fault::Silence => return Reply::Silence,
                _ => {}
            }
            let Some((op, channel)) = command.split_once(' ') else {
                return Reply::NoResponse;
            };
            let Ok(channel @ 1..=8) = channel.parse::<usize>() else {
                return Reply::NoResponse;
            };
            let response = match op {
                "RDGST?" => format!(
                    "{}\n",
                    match fault {
                        Fault::Status(v) => v,
                        _ => 0,
                    }
                ),
                "KRDG?" | "SRDG?" if matches!(fault, Fault::MalformedReading) => "bad\n".into(),
                "KRDG?" => format!("{:+.3}\n", state.temperatures[channel - 1].get()),
                "SRDG?" => "+1.234\n".into(),
                _ => return Reply::NoResponse,
            };
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
