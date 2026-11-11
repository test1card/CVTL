//! No-heat Phase B bench check through the loopback VISA bridge.
//!
//! Only identity, output state, measurements and thermometer readings are
//! requested. The two Keithley outputs are explicitly commanded OFF before
//! any measurement and their reported state is verified. Nothing enables an
//! output or writes a current setpoint.

use cvtl_instruments::{
    keithley::{parse_identity, parse_iv, parse_number},
    lakeshore::{Channel as LakeChannel, LakeShore218, SensorReading, SensorUnit},
    MessageTransport, VisaBridgeTcp,
};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    net::SocketAddr,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
struct Options {
    keithley: Option<SocketAddr>,
    lake: Option<SocketAddr>,
    channels: [u8; 4],
    channel_count: usize,
    unit: SensorUnit,
    timeout: Duration,
}

fn parse_address(value: &OsString) -> Result<SocketAddr, String> {
    let text = value.to_str().ok_or("Адрес моста должен быть UTF-8")?;
    let address: SocketAddr = text
        .parse()
        .map_err(|_| format!("Ожидается HOST:PORT, получено {text}"))?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err("Мост должен быть на loopback-адресе с ненулевым портом".into());
    }
    Ok(address)
}

fn parse_channels(value: &OsString) -> Result<([u8; 4], usize), String> {
    let text = value.to_str().ok_or("Список каналов должен быть UTF-8")?;
    let mut channels = [0u8; 4];
    let mut count = 0;
    for item in text.split(',') {
        let channel: u8 = item
            .trim()
            .parse()
            .map_err(|_| format!("Недопустимый канал {item}"))?;
        if !(1..=8).contains(&channel) {
            return Err("Канал должен быть от 1 до 8".into());
        }
        if count == channels.len() {
            return Err("Допускается не больше четырёх каналов".into());
        }
        channels[count] = channel;
        count += 1;
    }
    if count == 0 {
        return Err("Укажите хотя бы один канал".into());
    }
    Ok((channels, count))
}

fn parse_options(mut args: impl Iterator<Item = OsString>) -> Result<Options, String> {
    let mut options = Options {
        keithley: None,
        lake: None,
        channels: [1, 2, 3, 4],
        channel_count: 4,
        unit: SensorUnit::Volts,
        timeout: Duration::from_millis(5000),
    };
    let mut channels_seen = false;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("Укажите значение для {}", flag.to_string_lossy()))?;
        match flag.to_str() {
            Some("--keithley") if options.keithley.is_none() => {
                options.keithley = Some(parse_address(&value)?);
            }
            Some("--lake") if options.lake.is_none() => {
                options.lake = Some(parse_address(&value)?);
            }
            Some("--channels") if !channels_seen => {
                let (channels, count) = parse_channels(&value)?;
                options.channels = channels;
                options.channel_count = count;
                channels_seen = true;
            }
            Some("--unit") => {
                options.unit = match value.to_str() {
                    Some("volts") => SensorUnit::Volts,
                    Some("ohms") => SensorUnit::Ohms,
                    _ => return Err("Единица должна быть volts или ohms".into()),
                };
            }
            Some("--timeout-ms") => {
                let ms: u64 = value
                    .to_str()
                    .and_then(|text| text.parse().ok())
                    .filter(|ms| (1..=120_000).contains(ms))
                    .ok_or("Таймаут должен быть 1..120000 мс")?;
                options.timeout = Duration::from_millis(ms);
            }
            _ => return Err(format!("Неизвестный параметр {}", flag.to_string_lossy())),
        }
    }
    if options.keithley.is_none() && options.lake.is_none() {
        return Err("Укажите --keithley и/или --lake".into());
    }
    Ok(options)
}

fn deadline(timeout: Duration) -> Instant {
    Instant::now() + timeout
}

fn millis(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

fn check_keithley(options: &Options, address: SocketAddr) -> Result<Value, String> {
    let mut transport =
        VisaBridgeTcp::connect(address, options.timeout).map_err(|e| e.to_string())?;
    let start = Instant::now();
    let identity = parse_identity(
        &transport
            .query("*IDN?", deadline(options.timeout))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let identity_ms = millis(start);

    let start = Instant::now();
    transport
        .send(
            "smua.source.output = smua.OUTPUT_OFF",
            deadline(options.timeout),
        )
        .map_err(|e| e.to_string())?;
    transport
        .send(
            "smub.source.output = smub.OUTPUT_OFF",
            deadline(options.timeout),
        )
        .map_err(|e| e.to_string())?;
    let a = parse_number(
        &transport
            .query("print(smua.source.output)", deadline(options.timeout))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let b = parse_number(
        &transport
            .query("print(smub.source.output)", deadline(options.timeout))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let off_ms = millis(start);
    if a != 0.0 || b != 0.0 {
        return Err(format!("выход не подтверждён выключенным: A={a}, B={b}"));
    }

    let start = Instant::now();
    let (current_a, voltage_a) = parse_iv(
        &transport
            .query("print(smua.measure.iv())", deadline(options.timeout))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let (current_b, voltage_b) = parse_iv(
        &transport
            .query("print(smub.measure.iv())", deadline(options.timeout))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let measure_ms = millis(start);

    transport
        .send("errorqueue.clear()", deadline(options.timeout))
        .map_err(|e| e.to_string())?;
    let errors = String::from_utf8(
        transport
            .query("print(errorqueue.next())", deadline(options.timeout))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|_| "Очередь ошибок не в UTF-8".to_string())?;

    Ok(json!({
        "identity": identity,
        "outputs": {"a": "off", "b": "off"},
        "branch_a": {
            "current_a": current_a,
            "voltage_v": voltage_a,
            "power_w": current_a * voltage_a,
        },
        "branch_b": {
            "current_a": current_b,
            "voltage_v": voltage_b,
            "power_w": current_b * voltage_b,
        },
        "error_queue": errors,
        "timings_ms": {
            "identity": identity_ms,
            "off_and_verify": off_ms,
            "measure_both": measure_ms,
        },
    }))
}

fn check_lake(options: &Options, address: SocketAddr) -> Result<Value, String> {
    let channels = (0..options.channel_count)
        .map(|index| LakeChannel::new(options.channels[index]))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let numbers: Vec<u8> = channels.iter().map(|channel| channel.number()).collect();
    let transport =
        VisaBridgeTcp::connect(address, options.timeout).map_err(|error| error.to_string())?;
    let mut thermometer = LakeShore218::new(transport);
    let start = Instant::now();
    let readings = thermometer
        .read_channels(&channels, options.unit, deadline(options.timeout))
        .map_err(|error| error.to_string())?;
    let elapsed_ms = millis(start);
    let readings: Vec<Value> = readings
        .into_iter()
        .zip(numbers)
        .map(|(reading, number)| match reading {
            Ok(reading) => {
                let sensor = match reading.sensor {
                    SensorReading::Volts(value) => json!({"unit": "V", "value": value.get()}),
                    SensorReading::Ohms(value) => json!({"unit": "Ohm", "value": value.get()}),
                };
                json!({
                    "channel": number,
                    "kelvin": reading.kelvin.get(),
                    "sensor": sensor,
                })
            }
            Err(error) => json!({"channel": number, "error": error.to_string()}),
        })
        .collect();
    Ok(json!({"channels": readings, "batch_ms": elapsed_ms}))
}

pub fn run(args: impl Iterator<Item = OsString>) -> Result<String, String> {
    let options = parse_options(args)?;
    let mut report = serde_json::Map::new();
    if let Some(address) = options.keithley {
        report.insert("keithley".into(), check_keithley(&options, address)?);
    }
    if let Some(address) = options.lake {
        report.insert("lake".into(), check_lake(&options, address)?);
    }
    serde_json::to_string_pretty(&Value::Object(report))
        .map_err(|error| format!("Ошибка формирования результата: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> std::vec::IntoIter<OsString> {
        values
            .iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn channels_and_addresses_are_validated() {
        assert_eq!(
            parse_channels(&OsString::from("1,2,3,4")).unwrap(),
            ([1, 2, 3, 4], 4)
        );
        assert_eq!(
            parse_channels(&OsString::from("8")).unwrap(),
            ([8, 0, 0, 0], 1)
        );
        assert!(parse_channels(&OsString::from("0")).is_err());
        assert!(parse_channels(&OsString::from("9")).is_err());
        assert!(parse_channels(&OsString::from("1,2,3,4,5")).is_err());
        assert!(parse_address(&OsString::from("127.0.0.1:7001")).is_ok());
        assert!(parse_address(&OsString::from("10.0.0.1:7001")).is_err());
        assert!(parse_address(&OsString::from("127.0.0.1:0")).is_err());
    }

    #[test]
    fn options_require_an_endpoint_and_accept_units() {
        assert!(parse_options(args(&[])).is_err());
        let options = parse_options(args(&[
            "--keithley",
            "127.0.0.1:7001",
            "--lake",
            "127.0.0.1:7002",
            "--channels",
            "2,3",
            "--unit",
            "ohms",
            "--timeout-ms",
            "750",
        ]))
        .unwrap();
        assert!(options.keithley.is_some());
        assert!(options.lake.is_some());
        assert_eq!(options.channel_count, 2);
        assert_eq!(options.channels[..2], [2, 3]);
        assert!(matches!(options.unit, SensorUnit::Ohms));
        assert_eq!(options.timeout, Duration::from_millis(750));
    }

    #[test]
    fn unknown_flags_and_bad_timeouts_are_rejected() {
        assert!(parse_options(args(&["--keithley", "127.0.0.1:1", "--bogus"])).is_err());
        assert!(parse_options(args(&["--keithley", "127.0.0.1:1", "--timeout-ms", "0"])).is_err());
    }
}
