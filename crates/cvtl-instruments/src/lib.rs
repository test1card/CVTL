//! Instrument protocol adapters.
//!
//! Phase A ships the loopback TCP transport used by the simulators. Phase B
//! adds a loopback protocol to a local PyVISA bridge; the driver code and the
//! command/response parsers stay identical.

pub mod keithley;
pub mod lakeshore;
pub mod maisheng;
mod transport;
mod visa_bridge;

pub use transport::{LoopbackTcp, MessageTransport, MAX_FRAME_BYTES};
pub use visa_bridge::VisaBridgeTcp;

use std::{error::Error, fmt, io};

pub type Result<T> = std::result::Result<T, InstrumentError>;

#[derive(Debug)]
pub enum InstrumentError {
    Io(io::Error),
    Timeout,
    InvalidResponse(&'static str),
    Disconnected,
    Unverified,
    InvalidInput(&'static str),
    State(&'static str),
    Bridge(String),
}

impl fmt::Display for InstrumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "Ошибка обмена: {error}"),
            Self::Timeout => f.write_str("Истёк срок обмена с прибором"),
            Self::InvalidResponse(reason) => write!(f, "Некорректный ответ: {reason}"),
            Self::Disconnected => f.write_str("Сеанс закрыт или непригоден для обмена"),
            Self::Unverified => f.write_str("Подключение или протокол не подтверждены"),
            Self::InvalidInput(reason) => write!(f, "Недопустимый параметр: {reason}"),
            Self::State(reason) => write!(f, "Операция недоступна в текущем состоянии: {reason}"),
            Self::Bridge(text) => write!(f, "Ошибка моста VISA: {text}"),
        }
    }
}

impl Error for InstrumentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
