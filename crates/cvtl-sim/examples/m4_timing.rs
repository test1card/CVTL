//! Host timing on a local synthetic TCP instrument. No VISA or hardware access.
//! Run: cargo run -p cvtl-sim --example m4_timing --release
use cvtl_control::{
    allocation::Allocator,
    pi::{PiConfig, PowerSample},
    *,
};
use cvtl_core::units::{Amperes, Ohms, PerSecond, Seconds, Volts, Watts, WattsPerSecond};
use cvtl_envelope::{ChannelSpec, Margins, ResistanceRange};
use cvtl_instruments::{
    keithley::{Channel, ChannelConfig, Keithley, OutputState, SourceChannel},
    LoopbackTcp,
};
use cvtl_sim::keithley::KeithleySimulator;
use std::{
    error::Error,
    thread,
    time::{Duration, Instant},
};

fn w(x: f64) -> Watts {
    Watts::new(x).unwrap()
}
fn a(x: f64) -> Amperes {
    Amperes::new(x).unwrap()
}
fn v(x: f64) -> Volts {
    Volts::new(x).unwrap()
}
fn r(x: f64) -> Ohms {
    Ohms::new(x).unwrap()
}
fn s(x: f64) -> Seconds {
    Seconds::new(x).unwrap()
}
fn rate(x: f64) -> WattsPerSecond {
    WattsPerSecond::new(x).unwrap()
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

fn controller() -> Controller {
    let spec = ChannelSpec {
        resistance: ResistanceRange::new(r(10.0), r(11.0)).unwrap(),
        loop_resistance: r(0.0),
        heater_limit: Some(w(4.0)),
    };
    let pi = PiConfig {
        kp: 0.3,
        ki: PerSecond::new(1.0).unwrap(),
        integral_limit: w(2.0),
        resistance_initial: r(10.0),
        resistance_min: r(10.0),
        resistance_max: r(11.0),
        resistance_tau: s(0.3),
        min_current: a(0.001),
        current_quantum: Some(a(0.0001)),
        derivative: None,
    };
    let config = ControlConfig {
        pi: [pi; 2],
        branches: [spec; 2],
        margins: Margins::new(1.0, 1.0).unwrap(),
        total_limit: w(6.0),
        max_sample_age: s(0.5),
        temperature_hold: None,
    };
    Controller::new(
        config,
        Allocator::new(
            None,
            rate(100.0),
            rate(100.0),
            [rate(100.0); 2],
            [rate(100.0); 2],
        )
        .unwrap(),
    )
    .unwrap()
}
fn stats(xs: &mut [f64]) -> [f64; 4] {
    xs.sort_by(f64::total_cmp);
    let p = |fraction: f64| xs[((fraction * xs.len() as f64).ceil() as usize).saturating_sub(1)];
    [p(0.50), p(0.95), p(0.99), xs[xs.len() - 1]]
}
fn main() -> Result<(), Box<dyn Error>> {
    let simulator = KeithleySimulator::start()?;
    let transport = LoopbackTcp::connect(simulator.addr(), Duration::from_secs(2))?;
    let mut instrument = Keithley::new(transport);
    instrument.recover(deadline())?;
    let cfg = ChannelConfig {
        limit_v: v(10.0),
        limit_i: a(1.0),
        limit_p: w(4.0),
        source_range_i: a(1.0),
        measure_range_i: a(1.0),
        measure_range_v: v(40.0),
        nplc: 1.0,
    };
    for channel in [Channel::A, Channel::B] {
        instrument.configure(channel, cfg, deadline())?;
    }
    let result = (|| -> Result<String, Box<dyn Error>> {
        for channel in [Channel::A, Channel::B] {
            instrument.output_on(channel, deadline())?;
        }
        let mut controller = controller();
        let period = Duration::from_millis(100);
        let epoch = Instant::now();
        let mut previous = None;
        let mut previous_control: Option<Instant> = None;
        let mut jitter = Vec::with_capacity(39);
        let mut latency = Vec::with_capacity(40);
        let mut missed = 0;
        let mut active = false;
        for tick in 0..40_u32 {
            let due = epoch + period * tick;
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
            let wake = Instant::now();
            if wake.saturating_duration_since(due) >= period {
                missed += 1;
            }
            let dt = previous.map_or(period, |last| wake.duration_since(last));
            if previous.is_some() {
                jitter.push((dt.as_secs_f64() - period.as_secs_f64()).abs() * 1000.0);
            }
            previous = Some(wake);
            let (ma, mb) = instrument.measure_pair(deadline())?;
            let now = Instant::now();
            let control_dt = previous_control.map_or(period, |last| now.duration_since(last));
            previous_control = Some(now);
            let samples = [ma, mb].map(|m| {
                Some(TimedPowerSample {
                    at: s(m.started.duration_since(epoch).as_secs_f64()),
                    sample: PowerSample {
                        voltage: m.voltage_v,
                        current: m.current_a,
                    },
                })
            });
            let output = controller
                .tick(Tick {
                    demand: Demand::Power(w(1.0)),
                    permit: HeatPermit::Limit(w(6.0)),
                    branches_available: [true; 2],
                    measurements: samples,
                    now: s(now.duration_since(epoch).as_secs_f64()),
                    dt: s(control_dt.as_secs_f64()),
                })
                .map_err(|e| format!("controller error: {e:?}"))?;
            if let Some(fault) = output.fault {
                return Err(format!("controller fault: {fault:?}").into());
            }
            if let Some(warning) = output.warning {
                eprintln!("Предупреждение контроллера, такт {tick}: {warning:?}");
            }
            let Actuation::Drive(currents) = output.action else {
                return Err("controller did not drive".into());
            };
            active |= currents.iter().any(|x| x.get() > 0.0);
            for (channel, current) in [Channel::A, Channel::B].into_iter().zip(currents) {
                instrument.set_current(channel, current, deadline())?;
            }
            latency.push(ma.started.elapsed().as_secs_f64() * 1000.0);
        }
        if !active {
            return Err("no nonzero command issued".into());
        }
        let jitter = stats(&mut jitter);
        let latency = stats(&mut latency);
        let report = format!("{{\"scenario\":\"host_loopback_tcp_40_ticks\",\"tick_period_ms\":100,\"tick_count\":40,\"missed_ticks\":{missed},\"jitter_abs_ms\":{{\"p50\":{},\"p95\":{},\"p99\":{},\"max\":{}}},\"sample_start_to_last_command_ack_ms\":{{\"p50\":{},\"p95\":{},\"p99\":{},\"max\":{}}}}}", jitter[0], jitter[1], jitter[2], jitter[3], latency[0], latency[1], latency[2], latency[3]);
        Ok(report)
    })();
    // Attempt both OFF commands even if the run fails. Read back each result.
    let mut shutdown = Ok(());
    for channel in [Channel::A, Channel::B] {
        let off = instrument.output_off(channel, deadline()).and_then(|()| {
            match instrument.output_state(channel, deadline())? {
                OutputState::Off => Ok(()),
                _ => Err(cvtl_instruments::InstrumentError::Unverified),
            }
        });
        if let Err(error) = off {
            shutdown = Err(error);
        }
    }
    let report = result?;
    shutdown?;
    println!("{report}");
    Ok(())
}
