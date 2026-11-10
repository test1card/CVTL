use cvtl_instruments::MAX_FRAME_BYTES;
use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub(crate) enum Reply {
    Bytes(Vec<u8>),
    Silence,
    Disconnect,
    NoResponse,
}

pub(crate) struct LoopbackServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl LoopbackServer {
    pub(crate) fn spawn(
        mut handler: impl FnMut(&str) -> Reply + Send + 'static,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("cvtl-sim-loopback".into())
            .spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = serve(stream, &mut handler, &shutdown);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1))
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self {
            addr,
            stop,
            worker: Some(worker),
        })
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }
}

fn serve(
    mut stream: TcpStream,
    handler: &mut impl FnMut(&str) -> Reply,
    stop: &AtomicBool,
) -> io::Result<()> {
    let poll = Some(Duration::from_millis(20));
    stream.set_read_timeout(poll)?;
    stream.set_write_timeout(poll)?;
    stream.set_nodelay(true)?;
    let mut frame = Vec::new();
    let mut chunk = [0; 256];
    while !stop.load(Ordering::Relaxed) {
        let n = match stream.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(e) => return Err(e),
        };
        for &byte in &chunk[..n] {
            if byte != b'\n' {
                if frame.len() >= MAX_FRAME_BYTES {
                    return Ok(());
                }
                frame.push(byte);
                continue;
            }
            let command = frame.strip_suffix(b"\r").unwrap_or(&frame);
            let Ok(command) = std::str::from_utf8(command) else {
                return Ok(());
            };
            match handler(command) {
                Reply::Bytes(bytes) => stream.write_all(&bytes)?,
                Reply::Disconnect => return Ok(()),
                Reply::NoResponse | Reply::Silence => (),
            }
            frame.clear();
        }
    }
    Ok(())
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
