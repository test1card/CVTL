//! Loopback-only subset of the Model 218 query protocol.
use crate::loopback::{LoopbackServer, Reply};
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
pub struct LakeShoreSim {
    server: LoopbackServer,
    fault: Arc<Mutex<Fault>>,
}
impl LakeShoreSim {
    pub fn spawn() -> io::Result<Self> {
        let fault = Arc::new(Mutex::new(Fault::None));
        let f = Arc::clone(&fault);
        let server = LoopbackServer::spawn(move |command| {
            let fault = *f.lock().unwrap();
            match fault {
                Fault::Disconnect => return Reply::Disconnect,
                Fault::Silence => return Reply::Silence,
                _ => {}
            }
            let Some((op, channel)) = command.split_once(' ') else {
                return Reply::NoResponse;
            };
            if !matches!(channel.parse::<u8>(), Ok(1..=8)) {
                return Reply::NoResponse;
            }
            let response = match op {
                "RDGST?" => format!(
                    "{}\n",
                    match fault {
                        Fault::Status(v) => v,
                        _ => 0,
                    }
                ),
                "KRDG?" | "SRDG?" if matches!(fault, Fault::MalformedReading) => "bad\n".into(),
                "KRDG?" => "+42.000\n".into(),
                "SRDG?" => "+1.234\n".into(),
                _ => return Reply::NoResponse,
            };
            Reply::Bytes(response.into_bytes())
        })?;
        Ok(Self { server, fault })
    }
    pub fn addr(&self) -> SocketAddr {
        self.server.addr()
    }
    pub fn set_fault(&self, fault: Fault) {
        *self.fault.lock().unwrap() = fault;
    }
}
