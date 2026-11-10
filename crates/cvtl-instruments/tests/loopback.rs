use cvtl_instruments::{InstrumentError, LoopbackTcp, MessageTransport, MAX_FRAME_BYTES};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};

fn fixture(
    handler: impl FnOnce(TcpStream) + Send + 'static,
) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut query = [0; 5];
        stream.read_exact(&mut query).unwrap();
        assert_eq!(&query, b"read\n");
        handler(stream);
    });
    (addr, thread)
}

#[test]
fn joins_split_frames_and_accepts_crlf() {
    let (addr, server) = fixture(|mut stream| {
        stream.write_all(b"1.2\t").unwrap();
        thread::sleep(Duration::from_millis(5));
        stream.write_all(b"3.4\r\n").unwrap();
    });
    let mut transport = LoopbackTcp::connect(addr, Duration::from_secs(1)).unwrap();
    assert!(matches!(
        transport.send("read\nextra", Instant::now() + Duration::from_secs(1)),
        Err(InstrumentError::InvalidInput(_))
    ));
    assert!(transport.is_usable());
    assert_eq!(
        transport
            .query("read", Instant::now() + Duration::from_secs(1))
            .unwrap(),
        b"1.2\t3.4"
    );
    server.join().unwrap();
}

#[test]
fn rejects_a_non_loopback_address_before_connecting() {
    let addr = "192.0.2.1:5025".parse().unwrap();
    assert!(matches!(
        LoopbackTcp::connect(addr, Duration::from_secs(1)),
        Err(InstrumentError::InvalidInput(_))
    ));
}

#[test]
fn rejects_oversized_and_incomplete_replies_and_invalidates_the_session() {
    for reply in [vec![b'x'; MAX_FRAME_BYTES + 1], b"partial".to_vec()] {
        let (addr, server) = fixture(move |mut stream| {
            let _ = stream.write_all(&reply);
        });
        let mut transport = LoopbackTcp::connect(addr, Duration::from_secs(1)).unwrap();
        let result = transport.query("read", Instant::now() + Duration::from_secs(1));
        assert!(result.is_err());
        assert!(!transport.is_usable());
        assert!(matches!(
            transport.send("retry", Instant::now() + Duration::from_secs(1)),
            Err(InstrumentError::Disconnected)
        ));
        server.join().unwrap();
    }
}

#[test]
fn slow_bytes_do_not_extend_the_absolute_deadline() {
    let (addr, server) = fixture(|mut stream| {
        for _ in 0..20 {
            if stream.write_all(b"x").is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(15));
        }
        let _ = stream.write_all(b"\n");
    });
    let mut transport = LoopbackTcp::connect(addr, Duration::from_secs(1)).unwrap();
    assert!(matches!(
        transport.query("read", Instant::now() + Duration::from_millis(60)),
        Err(InstrumentError::Timeout)
    ));
    assert!(!transport.is_usable());
    server.join().unwrap();
}

#[test]
fn expired_deadline_closes_the_session_without_writing() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut transport =
        LoopbackTcp::connect(listener.local_addr().unwrap(), Duration::from_secs(1)).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let past = Instant::now() - Duration::from_millis(1);
    assert!(matches!(
        transport.send("read", past),
        Err(InstrumentError::Timeout)
    ));
    assert!(!transport.is_usable());
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
}
