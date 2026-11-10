use crate::{InstrumentError, Result};
use std::{
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    time::{Duration, Instant},
};

pub const MAX_FRAME_BYTES: usize = 1024;

/// Exclusive session. The same absolute deadline covers the write and read.
/// A query returns one frame without its terminator. Invalidation permanently
/// disables further exchange on this session.
pub trait MessageTransport {
    fn send(&mut self, command: &str, deadline: Instant) -> Result<()>;
    fn query(&mut self, command: &str, deadline: Instant) -> Result<Vec<u8>>;
    fn invalidate(&mut self);
    fn is_usable(&self) -> bool;
}

/// Phase A transport for local simulators. No DNS or non-loopback dialing.
pub struct LoopbackTcp {
    stream: Option<TcpStream>,
}

impl LoopbackTcp {
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

    fn read_frame(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        let stream = self.stream.as_mut().ok_or(InstrumentError::Disconnected)?;
        let mut chunk = [0; 256];
        let mut frame = Vec::new();
        loop {
            stream
                .set_read_timeout(Some(remaining(deadline)?))
                .map_err(io_error)?;
            let n = match stream.read(&mut chunk) {
                Ok(0) => return Err(InstrumentError::Disconnected),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_error(e)),
            };
            remaining(deadline)?;
            let line_end = line_end(&chunk[..n])?;
            let len = line_end.unwrap_or(n);
            if frame.len() + len > MAX_FRAME_BYTES {
                return Err(InstrumentError::InvalidResponse("слишком длинный кадр"));
            }
            frame.extend_from_slice(&chunk[..len]);
            if line_end.is_some() {
                if frame.last() == Some(&b'\r') {
                    frame.pop();
                }
                return Ok(frame);
            }
        }
    }
}

fn line_end(chunk: &[u8]) -> Result<Option<usize>> {
    let end = chunk.iter().position(|&b| b == b'\n');
    if end.is_some_and(|pos| pos + 1 != chunk.len()) {
        return Err(InstrumentError::InvalidResponse(
            "лишние данные после ответа",
        ));
    }
    Ok(end)
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
        | io::ErrorKind::NotConnected => InstrumentError::Disconnected,
        _ => InstrumentError::Io(error),
    }
}

impl MessageTransport for LoopbackTcp {
    fn send(&mut self, command: &str, deadline: Instant) -> Result<()> {
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
        let stream = self.stream.as_mut().ok_or(InstrumentError::Disconnected)?;
        let mut frame = Vec::with_capacity(command.len() + 1);
        frame.extend_from_slice(command.as_bytes());
        frame.push(b'\n');
        let result = (|| {
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
        })();
        if result.is_err() {
            self.invalidate();
        }
        result
    }

    fn query(&mut self, command: &str, deadline: Instant) -> Result<Vec<u8>> {
        self.send(command, deadline)?;
        let result = self.read_frame(deadline);
        if result.is_err() {
            self.invalidate();
        }
        result
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

    #[test]
    fn detects_coalesced_lines_and_bytes_after_a_reply() {
        assert_eq!(line_end(b"partial").unwrap(), None);
        assert_eq!(line_end(b"complete\r\n").unwrap(), Some(9));
        assert!(line_end(b"one\ntwo\n").is_err());
        assert!(line_end(b"one\npartial").is_err());
    }
}
