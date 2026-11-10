use cvtl_instruments::{
    keithley::{parse_error, parse_identity, parse_iv, parse_number},
    lakeshore::{parse_reading, parse_status},
    MAX_FRAME_BYTES,
};

const CORPUS: &[&[u8]] = &[
    b"+004.200",
    b"+00.681",
    b"0",
    b"128",
    b"-1.23456e-05\t+2.12500e+00",
    b"9.91e37\t0",
    b"0\t9.91e37",
    b"1e-9999\t1",
    b"1.0e+999",
    b"NaN\tinf",
    b"+004.200,+077.100",
    b"KEITHLEY INSTRUMENTS INC.,MODEL 2604B,SIM,1.0",
    b"-2.85000e+02\tProgram syntax\t2.00000e+00\t1.00000e+00",
    b"0\tQueue Is Empty\t0\t1",
    b"\x00\xff\r\n",
    b"",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn index(&mut self, len: usize) -> usize {
        (self.next() % len as u64) as usize
    }
}

fn check_parsers(bytes: &[u8]) {
    if let Ok(value) = parse_number(bytes) {
        assert!(value.is_finite() && value.abs() < 9.9e37);
    }
    if let Ok((i, v)) = parse_iv(bytes) {
        assert!(i.is_finite() && i.abs() < 9.9e37 && v.is_finite() && v.abs() < 9.9e37);
    }
    if let Ok(value) = parse_reading(bytes) {
        assert!(value.is_finite());
    }
    let _ = parse_status(bytes);
    let _ = parse_identity(bytes);
    let _ = parse_error(bytes);
}

#[test]
fn deterministic_byte_fuzz_on_instrument_responses() {
    let mut rng = Rng(0x2182_604B_2021);
    for &seed in CORPUS {
        check_parsers(seed);
    }
    for _ in 0..8_192 {
        let len = rng.index(MAX_FRAME_BYTES + 2);
        let bytes: Vec<_> = (0..len).map(|_| rng.next() as u8).collect();
        check_parsers(&bytes);
    }
    for _ in 0..8_192 {
        let mut bytes = CORPUS[rng.index(CORPUS.len())].to_vec();
        for _ in 0..1 + rng.index(6) {
            let offset = rng.index(bytes.len() + 1);
            if offset < bytes.len() && rng.index(3) == 0 {
                bytes.remove(offset);
            } else {
                bytes.insert(offset, rng.next() as u8);
            }
        }
        check_parsers(&bytes);
    }
}
