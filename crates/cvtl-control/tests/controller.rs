use cvtl_control::{
    allocation::Allocator,
    pi::{PiConfig, PowerSample},
    Actuation, ControlConfig, Controller, Demand, Equivalence, Fault, HeatPermit,
    TemperatureHoldConfig, Tick, TimedPowerSample,
};
use cvtl_core::units::{
    Amperes, Kelvin, Ohms, PerSecond, Seconds, Volts, Watts, WattsPerKelvin, WattsPerKelvinSecond,
    WattsPerSecond,
};
use cvtl_envelope::{check_current, ChannelSpec, Margins, ResistanceRange};

fn w(x: f64) -> Watts {
    Watts::new(x).unwrap()
}
fn a(x: f64) -> Amperes {
    Amperes::new(x).unwrap()
}
fn r(x: f64) -> Ohms {
    Ohms::new(x).unwrap()
}
fn s(x: f64) -> Seconds {
    Seconds::new(x).unwrap()
}
fn k(x: f64) -> Kelvin {
    Kelvin::new(x).unwrap()
}

fn branch(low: f64, high: f64, wire: f64, cap: f64) -> ChannelSpec {
    ChannelSpec {
        resistance: ResistanceRange::new(r(low), r(high)).unwrap(),
        loop_resistance: r(wire),
        heater_limit: Some(w(cap)),
    }
}

fn config(
    branches: [ChannelSpec; 2],
    margins: Margins,
    total: f64,
    quantum: Option<Amperes>,
) -> ControlConfig {
    ControlConfig {
        pi: branches.map(|branch| {
            let (min, max) = branch.resistance.bounds();
            PiConfig {
                kp: 0.3,
                ki: PerSecond::new(1.0).unwrap(),
                integral_limit: w(40.0),
                resistance_initial: r(min.get().midpoint(max.get())),
                resistance_min: min,
                resistance_max: max,
                resistance_tau: s(0.3),
                min_current: a(0.001),
                current_quantum: quantum,
                derivative: None,
            }
        }),
        branches,
        margins,
        total_limit: w(total),
        max_sample_age: s(0.5),
        temperature_hold: Some(TemperatureHoldConfig {
            kp: WattsPerKelvin::new(1.0).unwrap(),
            ki: WattsPerKelvinSecond::new(1.0).unwrap(),
        }),
    }
}

fn fixture() -> ControlConfig {
    config(
        [branch(10.0, 11.0, 0.2, 8.0); 2],
        Margins::new(0.9, 0.9).unwrap(),
        12.0,
        Some(a(0.0001)),
    )
}

fn controller(cfg: ControlConfig) -> Controller {
    let rate = WattsPerSecond::new(10.0).unwrap();
    Controller::new(
        cfg,
        Allocator::new(None, rate, rate, [rate; 2], [rate; 2]).unwrap(),
    )
    .unwrap()
}

fn samples(
    at: f64,
    currents: [Amperes; 2],
    resistances: [f64; 2],
) -> [Option<TimedPowerSample>; 2] {
    [0, 1].map(|i| {
        Some(TimedPowerSample {
            at: s(at),
            sample: PowerSample {
                current: currents[i],
                voltage: Volts::new(currents[i].get() * resistances[i]).unwrap(),
            },
        })
    })
}

fn tick(now: f64, demand: Demand, permit: HeatPermit) -> Tick {
    Tick {
        demand,
        permit,
        branches_available: [true; 2],
        measurements: samples(now, [a(0.0); 2], [10.0; 2]),
        now: s(now),
        dt: s(0.1),
    }
}

#[test]
fn modes_preserve_command_on_transfer_and_distinguish_off() {
    let mut c = controller(fixture());
    let permit = HeatPermit::Limit(w(12.0));
    let first = c.tick(tick(0.0, Demand::Power(w(2.0)), permit)).unwrap();
    assert!(matches!(first.action, Actuation::Drive(i) if i[0] > a(0.0)));
    let switched = c
        .tick(tick(0.1, Demand::VirtualBody(w(3.0)), permit))
        .unwrap();
    assert_eq!(switched.action, first.action);
    let negative = c
        .tick(tick(0.2, Demand::VirtualBody(w(-0.5)), permit))
        .unwrap();
    assert_eq!(negative.requested, Some(w(-0.5)));
    assert_eq!(negative.limited, w(0.0));
    assert_eq!(negative.equivalence, Equivalence::Limited);
    assert_eq!(negative.action, Actuation::Off);
    for (time, mode) in [(0.3, Demand::Observe), (0.4, Demand::Replay)] {
        assert_eq!(
            c.tick(tick(time, mode, permit)).unwrap().action,
            Actuation::Off
        );
    }
    assert_eq!(
        c.tick(tick(0.5, Demand::Power(w(2.0)), HeatPermit::Inhibit))
            .unwrap()
            .action,
        Actuation::Off
    );
}

#[test]
fn setpoint_change_with_a_small_integrator_bound_is_bumpless() {
    let mut cfg = fixture();
    for pi in &mut cfg.pi {
        pi.integral_limit = w(0.02);
    }
    let mut c = controller(cfg);
    let permit = HeatPermit::Limit(w(12.0));
    let first = c.tick(tick(0.0, Demand::Power(w(0.1)), permit)).unwrap();
    let changed = c.tick(tick(0.1, Demand::Power(w(10.0)), permit)).unwrap();
    assert_eq!(changed.action, first.action);
    assert_eq!(changed.requested, Some(w(10.0)));
    assert!(changed.slew_limited);
    let next = c.tick(tick(0.2, Demand::Power(w(10.0)), permit)).unwrap();
    assert!(next.limited > changed.limited);
    assert!(next.limited.get() <= changed.limited.get() + 1.0 + 1e-12);
    let transfer = c
        .tick(tick(0.3, Demand::VirtualBody(w(0.1)), permit))
        .unwrap();
    assert_eq!(transfer.action, next.action);
    assert_eq!(transfer.fault, None);
}

#[test]
fn failures_stop_both_branches_and_do_not_resume_on_recovery() {
    let mut c = controller(fixture());
    let permit = HeatPermit::Limit(w(12.0));
    let demand = Demand::Power(w(2.0));
    c.tick(tick(0.0, demand, permit)).unwrap();
    let mut failed = tick(0.1, demand, permit);
    failed.branches_available[1] = false;
    failed.measurements[1] = None;
    let out = c.tick(failed).unwrap();
    assert_eq!(out.fault, Some(Fault::BranchLost));
    assert_eq!(out.measured, None);
    assert_eq!(out.action, Actuation::Off);
    assert_eq!(
        c.tick(tick(0.2, demand, permit)).unwrap().action,
        Actuation::Off
    );
    c.reset_fault();
    let mut stale = tick(0.3, demand, permit);
    stale.measurements[1].as_mut().unwrap().at = s(0.4);
    let out = c.tick(stale).unwrap();
    assert_eq!(out.fault, Some(Fault::MeasurementStale));
    assert_eq!(out.measured, None);
    assert_eq!(out.action, Actuation::Off);
}

#[test]
fn temperature_hold_requires_permit_fresh_temperatures_and_no_inversion() {
    let mut c = controller(fixture());
    let hold = |at, stage1| Demand::TemperatureHold {
        setpoint: k(5.0),
        stage1: k(stage1),
        stage2: k(4.0),
        at: s(at),
    };
    let permit = HeatPermit::Limit(w(12.0));
    assert_eq!(
        c.tick(tick(0.0, hold(0.0, 20.0), HeatPermit::Inhibit))
            .unwrap()
            .action,
        Actuation::Off
    );
    c.tick(tick(0.1, hold(0.1, 20.0), permit)).unwrap();
    assert!(
        matches!(c.tick(tick(0.2, hold(0.2, 20.0), permit)).unwrap().action,
        Actuation::Drive(i) if i[0] > a(0.0))
    );
    let out = c.tick(tick(0.3, hold(0.3, 3.0), permit)).unwrap();
    assert_eq!(out.fault, Some(Fault::TemperatureInterlock));
    assert_eq!(out.action, Actuation::Off);
}

#[test]
fn invalid_clock_and_stale_observation_fail_closed() {
    let mut c = controller(fixture());
    let demand = Demand::Power(w(1.0));
    let permit = HeatPermit::Limit(w(12.0));
    let mut first = tick(1.0, Demand::Observe, permit);
    first.measurements = samples(0.0, [a(0.1); 2], [10.0; 2]);
    assert_eq!(c.tick(first).unwrap().measured, None);
    c.tick(tick(1.1, demand, permit)).unwrap();
    let out = c.tick(tick(1.1, demand, permit)).unwrap();
    assert_eq!(out.fault, Some(Fault::Clock));
    assert_eq!(out.action, Actuation::Off);
}

#[test]
fn missing_heater_limit_refuses_configuration() {
    let mut cfg = fixture();
    cfg.branches[0].heater_limit = None;
    let rate = WattsPerSecond::new(10.0).unwrap();
    let allocator = Allocator::new(None, rate, rate, [rate; 2], [rate; 2]).unwrap();
    assert!(Controller::new(cfg, allocator).is_err());
}

#[test]
fn one_hundred_thousand_final_commands_respect_source_heaters_and_total() {
    let mut state = 0x2090_0064_2026_u64;
    let mut unit = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (state >> 11) as f64 / ((1_u64 << 53) as f64)
    };
    let mut nonzero = 0;
    for case in 0..100_000 {
        let branches = std::array::from_fn(|_| {
            let min = 1.0 + 50.0 * unit();
            branch(
                min,
                min * (1.0 + 0.5 * unit()),
                2.0 * unit(),
                0.25 + 20.0 * unit(),
            )
        });
        let margins = Margins::new(0.6 + 0.4 * unit(), 0.6 + 0.4 * unit()).unwrap();
        let total = 0.5 + 40.0 * unit();
        let quantum = (case % 2 == 0).then(|| a(0.0001));
        let mut c = controller(config(branches, margins, total, quantum));
        let requested = w(0.1 + 50.0 * unit());
        let mut input = tick(0.0, Demand::Power(requested), HeatPermit::Limit(w(total)));
        let mut cap = total;
        for step in 0..2 {
            let out = c.tick(input).unwrap();
            let Actuation::Drive(currents) = out.action else {
                panic!("case {case}, step {step} failed to produce a command: {out:?}");
            };
            let mut worst_total = 0.0;
            let mut actual_r = [0.0; 2];
            for (i, current) in currents.into_iter().enumerate() {
                let branch = branches[i];
                let (min, max) = branch.resistance.bounds();
                actual_r[i] = min.get() + unit() * (max.get() - min.get());
                assert!(
                    current.get() >= 0.0
                        && check_current(max, branch.loop_resistance, current, margins).unwrap()
                );
                assert!(
                    check_current(r(actual_r[i]), branch.loop_resistance, current, margins)
                        .unwrap()
                );
                let worst = current.get() * (current.get() * max.get());
                assert!(worst <= branch.heater_limit.unwrap().get(), "case {case}");
                worst_total += worst;
                if let Some(q) = quantum {
                    let steps = current.get() / q.get();
                    assert!(
                        (steps - steps.round()).abs() < 1e-9,
                        "case {case}: off-grid"
                    );
                }
            }
            assert!(worst_total <= cap, "case {case}: {worst_total} > {cap}");
            assert!(
                out.limited.get() <= cap,
                "case {case}: target sum exceeds cap"
            );
            assert!(out.targets.iter().all(|p| p.get() >= 0.0));
            nonzero += usize::from(currents.iter().any(|i| i.get() > 0.0));
            cap = total * (0.2 + 0.7 * unit());
            input = tick(
                0.1,
                Demand::VirtualBody(requested),
                HeatPermit::Limit(w(cap)),
            );
            input.measurements = samples(0.1, currents, actual_r);
        }
    }
    assert!(nonzero >= 190_000, "{nonzero} active outputs");
}
