//! Small deterministic TSP subset for offline tests; no hardware access.
use crate::loopback::{LoopbackServer, Reply};
use cvtl_core::units::Ohms;
use cvtl_instruments::keithley::Channel;
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

// These are the only writable fields implemented by the simulated TSP subset.
const FIELDS: &[&str] = &[
    "source.output",
    "source.func",
    "sense",
    "source.autorangei",
    "measure.autorangei",
    "measure.autorangev",
    "source.limitv",
    "source.limiti",
    "source.limitp",
    "source.rangei",
    "measure.rangev",
    "measure.nplc",
    "source.leveli",
];

fn channel_field(text: &str) -> Option<(usize, &str)> {
    text.strip_prefix("smua.")
        .map(|field| (0, field))
        .or_else(|| text.strip_prefix("smub.").map(|field| (1, field)))
}

#[derive(Default)]
struct State {
    fields: [HashMap<String, f64>; 2],
    loads: [f64; 2],
    disconnect_next_query: bool,
    override_query: Option<(String, Vec<u8>)>,
    query_counts: HashMap<String, usize>,
}
pub struct KeithleySimulator {
    server: LoopbackServer,
    state: Arc<Mutex<State>>,
}
impl KeithleySimulator {
    pub fn start() -> io::Result<Self> {
        let state = Arc::new(Mutex::new(State {
            loads: [10., 10.],
            ..State::default()
        }));
        let shared = Arc::clone(&state);
        let server = LoopbackServer::spawn(move |command| {
            let mut s = shared.lock().unwrap();
            let query = command
                .strip_prefix("print(")
                .and_then(|text| text.strip_suffix(')'));
            let is_query = command == "*IDN?" || query.is_some();
            if is_query {
                if !matches!(command, "*IDN?" | "print(errorqueue.next())")
                    && !query.and_then(channel_field).is_some_and(|(_, key)| {
                        FIELDS.contains(&key)
                            || matches!(
                                key,
                                "measure.rangei"
                                    | "measure.iv()"
                                    | "OUTPUT_DCAMPS"
                                    | "AUTORANGE_OFF"
                                    | "SENSE_REMOTE"
                            )
                    })
                {
                    return Reply::Disconnect;
                }
                *s.query_counts.entry(command.to_owned()).or_default() += 1;
                if s.override_query
                    .as_ref()
                    .is_some_and(|(target, _)| target == command)
                {
                    let (_, bytes) = s.override_query.take().unwrap();
                    return Reply::Bytes(bytes);
                }
            }
            if s.disconnect_next_query && is_query {
                s.disconnect_next_query = false;
                return Reply::Disconnect;
            }
            if command == "print(errorqueue.next())" {
                return Reply::Bytes(b"0\tQueue Is Empty\t0\t1\n".to_vec());
            }
            if command == "*IDN?" {
                return Reply::Bytes(b"KEITHLEY INSTRUMENTS INC.,MODEL 2604B,SIM,1.0\n".to_vec());
            }
            let text = query.unwrap_or(command);
            let Some((idx, rest)) = channel_field(text) else {
                return Reply::Disconnect;
            };
            if query.is_some() {
                if rest == "OUTPUT_DCAMPS" || rest == "AUTORANGE_OFF" {
                    return Reply::Bytes(b"0\n".to_vec());
                }
                if rest == "SENSE_REMOTE" {
                    return Reply::Bytes(b"1\n".to_vec());
                }
                if rest == "measure.iv()" {
                    let fields = &s.fields[idx];
                    let on = *fields.get("source.output").unwrap_or(&0.) == 1.;
                    let set = *fields.get("source.leveli").unwrap_or(&0.);
                    let r = s.loads[idx];
                    let vlim = *fields.get("source.limitv").unwrap_or(&f64::INFINITY);
                    let plim = *fields.get("source.limitp").unwrap_or(&f64::INFINITY);
                    let i = if on {
                        set.min(vlim / r).min((plim / r).sqrt())
                    } else {
                        0.
                    };
                    return Reply::Bytes(format!("{i:.12e}\t{:.12e}\n", i * r).into_bytes());
                }
                return Reply::Bytes(
                    format!("{:.12e}\n", s.fields[idx].get(rest).unwrap_or(&0.)).into_bytes(),
                );
            }
            if let Some((key, value)) = rest.split_once(" = ") {
                if !FIELDS.contains(&key) {
                    return Reply::Disconnect;
                }
                let value: f64 = match value
                    .strip_prefix("smua.")
                    .or_else(|| value.strip_prefix("smub."))
                    .unwrap_or(value)
                {
                    "OUTPUT_ON" => 1.,
                    "OUTPUT_OFF" | "AUTORANGE_OFF" | "OUTPUT_DCAMPS" => 0.,
                    "SENSE_REMOTE" => 1.,
                    x => match x.parse() {
                        Ok(x) => x,
                        Err(_) => return Reply::Disconnect,
                    },
                };
                if !value.is_finite() {
                    return Reply::Disconnect;
                }
                s.fields[idx].insert(key.to_owned(), value);
                if key == "source.rangei" {
                    s.fields[idx].insert("measure.rangei".to_owned(), value);
                }
                return Reply::NoResponse;
            }
            Reply::Disconnect
        })?;
        Ok(Self { server, state })
    }
    pub fn addr(&self) -> SocketAddr {
        self.server.addr()
    }
    pub fn disconnect_next_query(&self) {
        self.state.lock().unwrap().disconnect_next_query = true;
    }
    pub fn override_next_query(&self, command: &str, response: &[u8]) {
        self.state.lock().unwrap().override_query = Some((command.to_owned(), response.to_vec()));
    }
    pub fn query_count(&self, command: &str) -> usize {
        *self
            .state
            .lock()
            .unwrap()
            .query_counts
            .get(command)
            .unwrap_or(&0)
    }
    pub fn set_load(&self, channel: Channel, resistance: Ohms) -> io::Result<()> {
        if resistance.get() <= 0. {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "load must be positive and finite",
            ));
        }
        let i = match channel {
            Channel::A => 0,
            Channel::B => 1,
        };
        self.state.lock().unwrap().loads[i] = resistance.get();
        Ok(())
    }
}
