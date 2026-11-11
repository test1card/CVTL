use std::{
    thread,
    time::{Duration, Instant},
};

use cvtl_core::units::{Amperes, Volts, Watts};
use cvtl_instruments::{
    keithley::{Channel, ChannelConfig, Keithley, SourceChannel},
    LoopbackTcp,
};
use cvtl_sim::keithley::KeithleySimulator;

const TIMEOUT: Duration = Duration::from_secs(2);

fn deadline() -> Instant {
    Instant::now() + TIMEOUT
}

#[test]
fn energy_is_integrated_between_actual_on_and_off_even_without_readback() {
    let source = KeithleySimulator::start().unwrap();
    let connect = || LoopbackTcp::connect(source.addr(), TIMEOUT).unwrap();
    let mut driver = Keithley::new(connect());
    driver.recover(deadline()).unwrap();
    let config = ChannelConfig {
        limit_v: Volts::new(10.0).unwrap(),
        limit_i: Amperes::new(1.0).unwrap(),
        limit_p: Watts::new(4.0).unwrap(),
        source_range_i: Amperes::new(1.0).unwrap(),
        measure_range_i: Amperes::new(1.0).unwrap(),
        measure_range_v: Volts::new(40.0).unwrap(),
        nplc: 0.1,
    };
    for (channel, current) in [(Channel::A, 0.1), (Channel::B, 0.2)] {
        driver.configure(channel, config, deadline()).unwrap();
        driver
            .set_current(channel, Amperes::new(current).unwrap(), deadline())
            .unwrap();
    }
    source.start_energy_tracking().unwrap();
    let on = [Channel::A, Channel::B].map(|channel| {
        let before = Instant::now();
        driver.output_on(channel, deadline()).unwrap();
        let after = Instant::now();
        thread::sleep(Duration::from_millis(10));
        (before, after)
    });
    let power = source.delivered_power().unwrap();

    source.disconnect_next_query();
    assert!(driver.measure(Channel::A, deadline()).is_err());
    assert_eq!(source.delivered_power().unwrap(), power);
    thread::sleep(Duration::from_millis(10));
    drop(driver);

    let mut recovered = Keithley::new(connect());
    let off_before = Instant::now();
    recovered.recover(deadline()).unwrap();
    let off_after = Instant::now();
    assert_eq!(
        source.delivered_power().unwrap(),
        [Watts::new(0.0).unwrap(); 2]
    );
    let interval = source.take_energy_interval().unwrap();
    assert!(interval.duration >= off_after.duration_since(on[0].0));
    for i in 0..2 {
        let lower = power[i].get() * off_before.duration_since(on[i].1).as_secs_f64();
        let upper = power[i].get() * off_after.duration_since(on[i].0).as_secs_f64();
        let energy = interval.energy[i].get();
        assert!(
            energy > 0.0 && energy >= lower - 1e-9 && energy <= upper + 1e-9,
            "{energy} J outside [{lower}, {upper}] for branch {i}"
        );
    }
    assert!(source
        .take_energy_interval()
        .unwrap()
        .energy
        .iter()
        .all(|energy| energy.get() == 0.0));
}

use std::{
    fs::{self, File},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use cvtl_core::units::{Kelvin, WattsPerKelvin};
use cvtl_safety::{
    machine::{MachineState, OutputState},
    supervisor::OperatorAction,
    FaultReason,
};
use cvtl_sim::{
    lakeshore::LakeShoreSim,
    recording::{Error, RecordingLoop, TickReport, PERIOD},
};
use cvtl_store::{
    record::{A1StepV1, Command, Correlation, Demand, Event},
    recovery::{recover, Tail},
    session::CloseResult,
};

struct Fixture {
    root: PathBuf,
    run: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "cvtl-a1-loop-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self {
            run: root.join("run"),
            root,
        }
    }

    fn start<'a>(
        &self,
        source: &'a KeithleySimulator,
        lake: &'a LakeShoreSim,
    ) -> RecordingLoop<'a> {
        let epoch = Instant::now();
        let mut owner =
            RecordingLoop::new_a1(source, lake, "fixture-a1", self.run.clone(), epoch).unwrap();
        wait(|| owner.poll().ready);
        owner.recover().unwrap();
        start(&mut owner);
        owner
    }

    fn ticks(&self) -> Vec<ThermalRow> {
        let mut rows = Vec::new();
        let outcome = recover(
            File::open(self.run.join("raw.ndjson.zst")).unwrap(),
            |row| {
                if let (Correlation::Tick(id), Event::Inputs { frame }) =
                    (row.correlation, row.event)
                {
                    rows.push(ThermalRow {
                        id,
                        step: frame.a1_step_v1,
                        demand: frame.demand,
                        model_valid: frame.model_valid,
                        temperatures: frame.temperatures.map(|value| value.temperature_k.unwrap()),
                    });
                }
            },
        )
        .unwrap();
        assert_eq!(outcome.tail, Tail::FrameBoundary);
        rows
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct ThermalRow {
    id: u64,
    step: Option<A1StepV1>,
    demand: Demand,
    model_valid: bool,
    temperatures: [f64; 4],
}

fn wait(mut condition: impl FnMut() -> bool) {
    let end = deadline();
    while !condition() {
        assert!(Instant::now() < end, "writer deadline");
        thread::park_timeout(Duration::from_millis(2));
    }
}

fn start(owner: &mut RecordingLoop<'_>) {
    owner.operate(OperatorAction::Arm).unwrap();
    owner.operate(OperatorAction::Start).unwrap();
    assert_eq!(owner.state(), MachineState::Running);
}

fn tick_at(owner: &mut RecordingLoop<'_>, due: Instant) -> Result<TickReport, Error> {
    if let Some(delay) = due.checked_duration_since(Instant::now()) {
        thread::sleep(delay);
    }
    owner.tick(due)
}

fn close(owner: &mut RecordingLoop<'_>) {
    owner.begin_close().unwrap();
    assert_eq!(owner.output_state(), OutputState::OffVerified);
    wait(|| owner.poll_close() != CloseResult::Pending);
    assert!(matches!(owner.poll_close(), CloseResult::Completed(_)));
}

fn requested(row: &ThermalRow) -> f64 {
    let Demand::VirtualBody { requested_w } = row.demand else {
        panic!("expected body demand");
    };
    requested_w
}

#[test]
fn closed_loop_records_feedback_scale_bridge_and_off() {
    let fixture = Fixture::new();
    let source = KeithleySimulator::start().unwrap();
    let lake = LakeShoreSim::spawn().unwrap();
    let mut owner = fixture.start(&source, &lake);
    let first = Instant::now() + PERIOD;
    for n in 1..=8_u32 {
        if n == 3 {
            owner.change_a1(Some(0.2), None).unwrap();
        }
        if n == 5 {
            owner
                .change_a1(None, Some(WattsPerKelvin::new(0.04).unwrap()))
                .unwrap();
        }
        assert!(tick_at(&mut owner, first + PERIOD * (n - 1))
            .unwrap()
            .command_latency
            .is_some());
    }
    close(&mut owner);
    let rows = fixture.ticks();
    assert_eq!(rows.len(), 8);
    assert!(rows.iter().all(|row| row.model_valid && row.step.is_some()));
    assert_eq!(rows[0].step.unwrap().heater_energy_j, [0.0; 2]);
    assert!(rows[1]
        .step
        .unwrap()
        .heater_energy_j
        .iter()
        .all(|energy| *energy > 0.0));
    let mut before = [45.0, 15.0, 16.0, 77.0];
    for row in &rows {
        let step = row.step.unwrap();
        let dt = Duration::from_nanos(step.interval_ns).as_secs_f64();
        let stored: f64 = [10.0, 20.0, 5.0]
            .into_iter()
            .enumerate()
            .map(|(i, c)| c * (row.temperatures[i] - before[i]))
            .sum();
        let applied: f64 = step.heater_energy_j.iter().sum();
        assert!((stored - (applied - 6.0 * dt)).abs() <= 0.036);
        before = row.temperatures;
    }
    assert!(rows[7].temperatures[1] < rows[0].temperatures[1]);
    let scale = rows[2].step.unwrap().change.unwrap();
    assert_eq!(scale.scale, Some(0.2));
    assert!((scale.temperature_before_k - scale.temperature_after_k).abs() <= 1e-6);
    let cp = cvtl_materials::nist_heat_capacity("ofhc_rrr100")
        .unwrap()
        .curve;
    let integral = cp
        .integral(
            cp.bounds().0,
            Kelvin::new(scale.temperature_before_k).unwrap(),
        )
        .unwrap();
    let expected_dh = 20.0 * (0.2 - 0.1) * integral.get();
    assert!((scale.dh_event_j.unwrap() - expected_dh).abs() <= 1e-6);
    assert_ne!(requested(&rows[1]), requested(&rows[2]));
    let bridge = rows[4].step.unwrap().change.unwrap();
    assert_eq!(bridge.bridge_w_per_k, Some(0.04));
    assert!(bridge.dh_event_j.is_none());
    assert!(requested(&rows[4]) > requested(&rows[3]));
    assert_eq!(
        rows.iter()
            .filter(|row| row.step.unwrap().change.is_some())
            .count(),
        2
    );

    let config = fs::read_to_string(fixture.run.join("config.snapshot.yaml")).unwrap();
    assert!(config.contains("synthetic: true"));
    assert!(config.contains("body_mass_kg: 20") && config.contains("material: ofhc_rrr100"));
    assert_eq!(
        fs::read(fixture.run.join("references/nist_csv")).unwrap(),
        include_bytes!("../../../data/materials/nist.csv")
    );
    let meta = fs::read_to_string(fixture.run.join("meta.json")).unwrap();
    assert!(meta.contains("test_only_a1_loop_v1") && meta.contains("nist_csv"));
    assert_eq!(source.delivered_power().unwrap().map(|p| p.get()), [0.0; 2]);
}

#[test]
fn source_fault_requires_manual_restart_and_new_a1_controller_time() {
    let fixture = Fixture::new();
    let source = KeithleySimulator::start().unwrap();
    let lake = LakeShoreSim::spawn().unwrap();
    let mut owner = fixture.start(&source, &lake);
    let first = Instant::now() + PERIOD;
    tick_at(&mut owner, first).unwrap();
    source.disconnect_next_query();
    assert!(tick_at(&mut owner, first + PERIOD).is_err());
    assert_eq!(owner.fault_reason(), Some(FaultReason::SourceLinkLost));
    assert_eq!(owner.output_state(), OutputState::Unknown);
    assert!(owner.fallback_requested());
    owner.recover().unwrap();
    assert_eq!(owner.state(), MachineState::FaultLatched);
    assert_eq!(owner.output_state(), OutputState::OffVerified);
    assert_eq!(source.delivered_power().unwrap().map(|p| p.get()), [0.0; 2]);

    owner.operate(OperatorAction::Reset).unwrap();
    assert!(owner.operate(OperatorAction::Start).is_err());
    assert_eq!(source.delivered_power().unwrap().map(|p| p.get()), [0.0; 2]);
    start(&mut owner);
    let resumed = tick_at(&mut owner, Instant::now() + PERIOD).unwrap();
    assert!(resumed.command_latency.is_some());
    assert!(source
        .delivered_power()
        .unwrap()
        .iter()
        .any(|p| p.get() > 0.0));
    close(&mut owner);
    let mut fault = false;
    recover(
        File::open(fixture.run.join("raw.ndjson.zst")).unwrap(),
        |row| {
            if let Event::Observation { faults, .. } = row.event {
                fault |= faults.iter().any(|f| f.code == "source_link_lost");
            }
        },
    )
    .unwrap();
    assert!(fault);
}

#[test]
fn stale_held_tf_rejects_the_step_and_leaves_no_drive() {
    let fixture = Fixture::new();
    let source = KeithleySimulator::start().unwrap();
    let lake = LakeShoreSim::spawn().unwrap();
    let mut owner = fixture.start(&source, &lake);
    tick_at(&mut owner, Instant::now() + PERIOD).unwrap();
    thread::sleep(Duration::from_millis(1550));
    assert!(matches!(owner.tick(Instant::now()), Err(Error::Safety(_))));
    assert_eq!(owner.output_state(), OutputState::OffVerified);
    assert_eq!(source.delivered_power().unwrap().map(|p| p.get()), [0.0; 2]);
    close(&mut owner);
    let rows = fixture.ticks();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].step.is_some());
    assert!(rows[1].step.is_none() && !rows[1].model_valid);
    assert_eq!(rows[1].temperatures, rows[0].temperatures);
    recover(
        File::open(fixture.run.join("raw.ndjson.zst")).unwrap(),
        |row| {
            if row.correlation == Correlation::Tick(rows[1].id) {
                assert!(!matches!(
                    row.event,
                    Event::CommandIntent {
                        command: Command::Drive { .. },
                        ..
                    }
                ));
            }
        },
    )
    .unwrap();
}
