//! Loopback bridge to a local PyVISA process (Phase B).
//!
//! The bridge speaks a small request/response protocol so that a write that
//! expects no answer is distinct from a query:
//!
//! ```text
//! client -> bridge   W <command>\n   |  Q <command>\n
//! bridge -> client   OK\n            |  R <length>\n<payload>\n  |  ERR <text>\n
//! ```
//!
//! One absolute monotonic deadline covers the request and its answer. Any
//! protocol or I/O failure permanently invalidates this session.

use crate::{InstrumentError, MessageTransport, Result, MAX_FRAME_BYTES};
use std::{
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const MAX_STATUS_BYTES: usize = 256;

/// Exclusive transport to one locally running VISA bridge.
pub struct VisaBridgeTcp {
    stream: Option<TcpStream>,
}

impl VisaBridgeTcp {
    pub fn connect(addr: SocketAddr, timeout: Duration) -> Result<Self> {
        if !addr.ip().is_loopback() || addr.port() == 0 || timeout.is_zero() {
            return Err(InstrumentError::InvalidInput(
                "ожидается loopback-адрес, порт и положительный таймаут",
            ));
        }
        let stream = TcpStream::connect_timeout(&addr, timeout).map_err(io_error)?;
        stream.set_nodelay(true).map_err(io_error)?;
        Ok(Self {
            stream: Some(stream),
        })
    }

    fn validate(command: &str) -> Result<()> {
        if command.is_empty()
            || command.len() > MAX_FRAME_BYTES
            || !command
                .bytes()
                .all(|b| (b' '..=b'~').contains(&b) || b == b'\t')
        {
            return Err(InstrumentError::InvalidInput(
                "ожидается одна строка ASCII без окончания",
            ));
        }
        Ok(())
    }

    fn write_request(&mut self, kind: u8, command: &str, deadline: Instant) -> Result<()> {
        Self::validate(command)?;
        let stream = self.stream.as_mut().ok_or(InstrumentError::Disconnected)?;
        let mut frame = Vec::with_capacity(command.len() + 3);
        frame.push(kind);
        frame.push(b' ');
        frame.extend_from_slice(command.as_bytes());
        frame.push(b'\n');
        let mut unwritten = frame.as_slice();
        while !unwritten.is_empty() {
            stream
                .set_write_timeout(Some(remaining(deadline)?))
                .map_err(io_error)?;
            match stream.write(unwritten) {
                Ok(0) => return Err(InstrumentError::Disconnected),
                Ok(n) => unwritten = &unwritten[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_error(e)),
            }
        }
        remaining(deadline)?;
        Ok(())
    }

    fn read_byte(&mut self, deadline: Instant) -> Result<u8> {
        let stream = self.stream.as_mut().ok_or(InstrumentError::Disconnected)?;
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(io_error)?;
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => return Err(InstrumentError::Disconnected),
                Ok(_) => return Ok(byte[0]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_error(e)),
            }
        }
    }

    fn read_status(&mut self, deadline: Instant) -> Result<String> {
        let mut bytes = Vec::new();
        loop {
            remaining(deadline)?;
            let byte = self.read_byte(deadline)?;
            if byte == b'\n' {
                break;
            }
            if bytes.len() == MAX_STATUS_BYTES {
                return Err(InstrumentError::InvalidResponse(
                    "слишком длинный статус моста",
                ));
            }
            bytes.push(byte);
        }
        String::from_utf8(bytes)
            .map_err(|_| InstrumentError::InvalidResponse("статус моста не в UTF-8"))
    }

    fn read_payload(&mut self, deadline: Instant, length: usize) -> Result<Vec<u8>> {
        if length > MAX_FRAME_BYTES {
            return Err(InstrumentError::InvalidResponse(
                "ответ моста слишком длинный",
            ));
        }
        let mut payload = vec![0u8; length];
        let mut filled = 0;
        while filled < length {
            remaining(deadline)?;
            let stream = self.stream.as_mut().ok_or(InstrumentError::Disconnected)?;
            stream
                .set_read_timeout(Some(remaining(deadline)?))
                .map_err(io_error)?;
            match stream.read(&mut payload[filled..]) {
                Ok(0) => return Err(InstrumentError::Disconnected),
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_error(e)),
            }
        }
        if self.read_byte(deadline)? != b'\n' {
            return Err(InstrumentError::InvalidResponse(
                "нет окончания после ответа моста",
            ));
        }
        Ok(payload)
    }

    fn write_ack(&mut self, deadline: Instant) -> Result<()> {
        let status = self.read_status(deadline)?;
        if status == "OK" {
            return Ok(());
        }
        self.bridge_status(&status)
    }

    fn read_reply(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        let status = self.read_status(deadline)?;
        if let Some(length) = status.strip_prefix("R ") {
            let length: usize = length
                .parse()
                .map_err(|_| InstrumentError::InvalidResponse("длина ответа моста"))?;
            return self.read_payload(deadline, length);
        }
        self.bridge_status(&status)
    }

    fn bridge_status<T>(&self, status: &str) -> Result<T> {
        if let Some(text) = status.strip_prefix("ERR ") {
            return Err(InstrumentError::Bridge(text.to_owned()));
        }
        Err(InstrumentError::InvalidResponse("неизвестный статус моста"))
    }

    fn invalidate_on<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.invalidate();
        }
        result
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|time| !time.is_zero())
        .ok_or(InstrumentError::Timeout)
}

fn io_error(error: io::Error) -> InstrumentError {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => InstrumentError::Timeout,
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::NotConnected
        | io::ErrorKind::UnexpectedEof => InstrumentError::Disconnected,
        _ => InstrumentError::Io(error),
    }
}

impl MessageTransport for VisaBridgeTcp {
    fn send(&mut self, command: &str, deadline: Instant) -> Result<()> {
        let result = (|| {
            self.write_request(b'W', command, deadline)?;
            self.write_ack(deadline)
        })();
        self.invalidate_on(result)
    }

    fn query(&mut self, command: &str, deadline: Instant) -> Result<Vec<u8>> {
        let result = (|| {
            self.write_request(b'Q', command, deadline)?;
            self.read_reply(deadline)
        })();
        self.invalidate_on(result)
    }

    fn invalidate(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn is_usable(&self) -> bool {
        self.stream.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread,
    };

    fn serve<F>(handler: F) -> (SocketAddr, thread::JoinHandle<()>)
    where
        F: FnOnce(TcpStream) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handler(stream);
        });
        (addr, handle)
    }

    #[test]
    fn writes_and_queries_follow_the_bridge_protocol() {
        let (addr, handle) = serve(|stream| {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "W smua.source.output = smua.OUTPUT_OFF\n");
            writer.write_all(b"OK\n").unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "Q *IDN?\n");
            writer.write_all(b"R 5\nhello\n").unwrap();
        });
        let mut transport = VisaBridgeTcp::connect(addr, Duration::from_secs(1)).unwrap();
        transport
            .send("smua.source.output = smua.OUTPUT_OFF", deadline())
            .unwrap();
        assert_eq!(transport.query("*IDN?", deadline()).unwrap(), b"hello");
        assert!(transport.is_usable());
        handle.join().unwrap();
    }

    #[test]
    fn bridge_error_is_reported_and_invalidates_the_session() {
        let (addr, handle) = serve(|stream| {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            writer.write_all(b"ERR resource unavailable\n").unwrap();
        });
        let mut transport = VisaBridgeTcp::connect(addr, Duration::from_secs(1)).unwrap();
        let error = transport.query("*IDN?", deadline()).unwrap_err();
        assert!(
            matches!(error, InstrumentError::Bridge(ref text) if text == "resource unavailable")
        );
        assert!(!transport.is_usable());
        handle.join().unwrap();
    }

    #[test]
    fn oversized_reply_and_malformed_status_are_rejected() {
        let (addr, handle) = serve(|stream| {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            writer.write_all(b"R 100000\n").unwrap();
        });
        let mut transport = VisaBridgeTcp::connect(addr, Duration::from_secs(1)).unwrap();
        assert!(transport.query("*IDN?", deadline()).is_err());
        assert!(!transport.is_usable());
        handle.join().unwrap();
    }

    #[test]
    fn a_silent_bridge_times_out_and_invalidates() {
        let (addr, handle) = serve(|stream| {
            thread::sleep(Duration::from_millis(300));
            drop(stream);
        });
        let mut transport = VisaBridgeTcp::connect(addr, Duration::from_millis(50)).unwrap();
        let short = Instant::now() + Duration::from_millis(50);
        assert!(matches!(
            transport.query("*IDN?", short),
            Err(InstrumentError::Timeout)
        ));
        assert!(!transport.is_usable());
        handle.join().unwrap();
    }

    #[test]
    fn command_with_a_terminator_is_rejected_before_io() {
        let mut transport = VisaBridgeTcp { stream: None };
        assert!(matches!(
            transport.send("bad\n", deadline()),
            Err(InstrumentError::InvalidInput(_))
        ));
        assert!(matches!(
            transport.query("x".repeat(MAX_FRAME_BYTES + 1).as_str(), deadline()),
            Err(InstrumentError::InvalidInput(_))
        ));
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(1)
    }
}
