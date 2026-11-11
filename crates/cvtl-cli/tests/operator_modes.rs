use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use cvtl_cli::{check_archive, export_archive, inspect_archive, CompletionReceipt};
use cvtl_core::units::Kelvin;
use cvtl_materials::nist_heat_capacity_from_catalog;
use cvtl_physics::scenario::{BodySetup, BridgeSetup};
use cvtl_safety::{
    machine::{MachineState, OutputState},
    supervisor::OperatorAction,
    FaultReason,
};
use cvtl_sim::{
    keithley::KeithleySimulator,
    lakeshore::{Fault as LakeShoreFault, LakeShoreSim},
    modes::{
        BodyEdit, BodyPort, CompensationSetup, FixturePartSetup, ModeSetup, ModeSnapshot,
        PowerPoint,
    },
    recording::{Error, RecordingLoop, TickReport, PERIOD},
};
use cvtl_store::{
    record::{
        encode_line, Channel, Command, Correlation, Demand, Event, ModeBodyStepV1,
        ModeChangeOutcomeV1, ModeCompensationIssue, ModeCompensationV1, Record, ThermalPoint,
    },
    recovery::{recover, Tail},
    session::CloseResult,
};
use parquet::{
    file::reader::{FileReader, SerializedFileReader},
    record::{Field, RowAccessor},
};
use serde_json::Value;
use sha2::{Digest, Sha256};

struct Fixture {
    root: PathBuf,
    source: KeithleySimulator,
    thermometer: LakeShoreSim,
}

const RUN_NAME: &str = "operator-modes";

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "cvtl-operator-modes-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let source = KeithleySimulator::start().unwrap();
        let thermometer = LakeShoreSim::spawn().unwrap();
        fs::create_dir(&root).unwrap();
        Self {
            root,
            source,
            thermometer,
        }
    }

    fn path(&self) -> PathBuf {
        self.root.join("run")
    }

    fn open(&self, mode: ModeSetup) -> RecordingLoop<'_> {
        self.open_snapshot(ModeSnapshot::new(mode).unwrap())
    }

    fn open_body(&self, port: BodyPort, conductance_w_per_k: f64) -> RecordingLoop<'_> {
        self.open_snapshot(body_snapshot(port, conductance_w_per_k, None))
    }

    fn open_compensated(&self, port: BodyPort, conductance_w_per_k: f64) -> RecordingLoop<'_> {
        self.open_snapshot(body_snapshot(
            port,
            conductance_w_per_k,
            Some(compensation(0.2)),
        ))
    }

    fn open_snapshot(&self, snapshot: ModeSnapshot) -> RecordingLoop<'_> {
        let mut owner = RecordingLoop::new_operator_mode(
            &self.source,
            &self.thermometer,
            RUN_NAME,
            self.path(),
            Instant::now(),
            snapshot,
        )
        .unwrap();
        wait(|| owner.poll().ready);
        owner.recover().unwrap();
        self.assert_off(&owner);
        owner
    }

    fn assert_off(&self, owner: &RecordingLoop<'_>) {
        assert_eq!(owner.output_state(), OutputState::OffVerified);
        assert_eq!(
            self.source.delivered_power().unwrap().map(|w| w.get()),
            [0.0; 2]
        );
    }

    fn close(&self, mut owner: RecordingLoop<'_>) -> CompletionReceipt {
        owner.begin_close().unwrap();
        self.assert_off(&owner);
        let mut completed = None;
        wait(|| match owner.poll_close() {
            CloseResult::Completed(receipt) => {
                completed = Some(receipt);
                true
            }
            CloseResult::Pending => false,
            unexpected => panic!("{unexpected:?}"),
        });
        let completed = completed.unwrap();
        CompletionReceipt {
            run: RUN_NAME.into(),
            generation: completed.generation,
            last_seq: completed.last_seq,
            output_state: owner.output_state().into(),
        }
    }

    fn scan(&self, visit: impl FnMut(Record<'_>)) {
        let raw = fs::File::open(self.path().join("raw.ndjson.zst")).unwrap();
        let result = recover(raw, visit).unwrap();
        assert_eq!(result.tail, Tail::FrameBoundary);
        assert!(result.records > 0);
    }

    fn assert_replays(&self, mode: &str) {
        let archive = check_archive(&self.path()).unwrap();
        let report = serde_json::to_value(&archive.report).unwrap();
        assert_eq!(report["status"], "matched", "{report:#}");
        assert_eq!(report["mode"], mode, "{report:#}");
        assert_eq!(report["output_state_at_boundary"], "off_verified");
        assert_eq!(
            archive.last_verified_seq,
            archive.report.raw_verified_records
        );
    }

    fn reject_raw_edit(&self, mut edit: impl FnMut(&mut Record<'_>) -> bool) {
        let path = self.path().join("raw.ndjson.zst");
        let original = fs::read(&path).unwrap();
        let mut lines = Vec::new();
        let mut changed = None;
        self.scan(|mut row| {
            if changed.is_none() && edit(&mut row) {
                changed = Some(row.seq);
            }
            lines.push(encode_line(&row).unwrap());
        });
        let changed = changed.expect("fixture contains target input");
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 1).unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.window_log(18).unwrap();
        encoder
            .set_pledged_src_size(Some(lines.iter().map(|line| line.len() as u64).sum()))
            .unwrap();
        for line in lines {
            encoder.write_all(&line).unwrap();
        }
        fs::write(&path, encoder.finish().unwrap()).unwrap();
        let report = serde_json::to_value(check_archive(&self.path()).unwrap().report).unwrap();
        fs::write(path, original).unwrap();
        assert_eq!(report["status"], "mismatch", "{report:#}");
        assert_eq!(report["first_diagnostic"]["seq"], changed, "{report:#}");
    }

    fn reject_snapshot_edit(&self, edit: impl FnOnce(&mut yaml_serde::Value)) {
        let config_path = self.path().join("config.snapshot.yaml");
        let meta_path = self.path().join("meta.json");
        let original_config = fs::read(&config_path).unwrap();
        let original_meta = fs::read(&meta_path).unwrap();
        let mut config: yaml_serde::Value = yaml_serde::from_slice(&original_config).unwrap();
        edit(&mut config["test_only"]["modes"]["mode"]);
        let config = yaml_serde::to_string(&config).unwrap();
        let mut meta: Value = serde_json::from_slice(&original_meta).unwrap();
        meta["config_sha256"] = format!("{:x}", Sha256::digest(config.as_bytes())).into();
        fs::write(&config_path, config).unwrap();
        fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        let report = serde_json::to_value(check_archive(&self.path()).unwrap().report).unwrap();
        fs::write(config_path, original_config).unwrap();
        fs::write(meta_path, original_meta).unwrap();
        assert_eq!(report["status"], "mismatch", "{report:#}");
    }
}

fn compensation(mass_kg: f64) -> CompensationSetup {
    CompensationSetup {
        fixture_parasitic_w: 0.05,
        fixture: vec![FixturePartSetup {
            material: "ofhc_rrr100".into(),
            mass_kg,
        }],
    }
}

fn body_snapshot(
    port: BodyPort,
    conductance_w_per_k: f64,
    compensation: Option<CompensationSetup>,
) -> ModeSnapshot {
    ModeSnapshot::with_body(
        ModeSetup::VirtualBody {
            port,
            body_heat_w: 0.125,
            compensation,
        },
        BodySetup {
            material: "ofhc_rrr100".into(),
            mass_kg: 0.2,
            temperature_k: 30.0,
            scale: 1.0,
            bridge: BridgeSetup::Constant {
                conductance_w_per_k,
            },
        },
    )
    .unwrap()
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready() {
        assert!(Instant::now() < deadline, "writer deadline");
        thread::park_timeout(Duration::from_millis(2));
    }
}

fn start(owner: &mut RecordingLoop<'_>) {
    owner.operate(OperatorAction::Arm).unwrap();
    owner.operate(OperatorAction::Start).unwrap();
    assert_eq!(owner.state(), MachineState::Running);
    assert_eq!(owner.output_state(), OutputState::OnVerified);
}

fn tick_at(owner: &mut RecordingLoop<'_>, due: Instant) -> Result<TickReport, Error> {
    thread::sleep(due.saturating_duration_since(Instant::now()));
    let report = owner.tick(due)?;
    assert!(!report.missed);
    Ok(report)
}

fn heat(owner: &mut RecordingLoop<'_>) {
    start(owner);
    let first = Instant::now() + PERIOD;
    for index in 0..10 {
        let report = tick_at(owner, first + PERIOD * index).unwrap();
        assert!(report.command_latency.is_some());
        if report.measured_w.unwrap() > 0.0 {
            return;
        }
    }
    panic!("no measured heating in the first ten ticks");
}

#[test]
fn observe_records_measurements_with_verified_off() {
    let fixture = Fixture::new();
    let mut owner = fixture.open(ModeSetup::Observe);
    for action in [OperatorAction::Arm, OperatorAction::Start] {
        assert!(owner.operate(action).is_err());
        fixture.assert_off(&owner);
    }
    let tick = tick_at(&mut owner, Instant::now() + PERIOD).unwrap();
    assert_eq!(tick.measured_w, Some(0.0));
    assert!(tick.command_latency.is_none());
    fixture.close(owner);

    let mut observed = 0;
    fixture.scan(|row| match row.event {
        Event::Inputs { frame } => {
            assert!(matches!(row.correlation, Correlation::Tick(_)));
            assert_eq!(frame.demand, Demand::Observe);
            assert!(frame.source_connected && frame.thermometer_connected);
            assert!(frame.mode_step_v1.is_none());
            observed += 1;
        }
        Event::Operator { action, .. } => assert!(!matches!(action, "arm" | "start")),
        Event::CommandIntent { command, .. } => assert_eq!(command, Command::Off),
        Event::Dispatch { command, .. } => assert!(!command.contains("OUTPUT_ON")),
        _ => {}
    });
    assert_eq!(observed, 1);
    fixture.assert_replays("observe");
}

#[test]
fn constant_request_records_actual_heater_energy() {
    let fixture = Fixture::new();
    let mut owner = fixture.open(ModeSetup::Constant { power_w: 0.75 });
    heat(&mut owner);
    fixture.close(owner);

    let mut energies = Vec::new();
    fixture.scan(|row| {
        if let (Correlation::Tick(_), Event::Inputs { frame }) = (row.correlation, row.event) {
            assert_eq!(frame.demand, Demand::Power { requested_w: 0.75 });
            assert!(frame.a1_step_v1.is_none());
            energies.push(frame.mode_step_v1.unwrap().heater_energy_j);
        }
    });
    assert!(energies.len() >= 2);
    assert!(energies.last().unwrap().iter().all(|joules| *joules > 0.0));
    fixture.assert_replays("constant");
}

#[test]
fn profile_uses_raw_time_from_each_confirmed_start() {
    let fixture = Fixture::new();
    let mut owner = fixture.open(ModeSetup::PowerProfile {
        points: [(0, 0.75), (200, 1.25), (400, 0.75)]
            .map(|(elapsed_ms, power_w)| PowerPoint {
                elapsed_ms,
                power_w,
            })
            .to_vec(),
    });
    for ticks in [6, 2] {
        start(&mut owner);
        let first = Instant::now() + PERIOD;
        for index in 0..ticks {
            let tick = tick_at(&mut owner, first + PERIOD * index).unwrap();
            assert!(tick.command_latency.is_some());
        }
        owner.operate(OperatorAction::Stop).unwrap();
        fixture.assert_off(&owner);
    }
    fixture.close(owner);

    let mut request = None;
    let mut epochs = Vec::<(u64, Vec<u64>)>::new();
    fixture.scan(|row| match (row.correlation, row.event) {
        (Correlation::Request(id), Event::Inputs { frame }) => {
            request = Some((id, frame.measured_ns));
        }
        (
            Correlation::Request(id),
            Event::Operator {
                action: "start",
                at_ns,
            },
        ) => {
            assert_eq!(request, Some((id, at_ns)));
            epochs.push((at_ns, Vec::new()));
        }
        (Correlation::Tick(_), Event::Inputs { frame }) => {
            let (start_ns, elapsed) = epochs.last_mut().expect("accepted Start before profile");
            let elapsed_ns = frame.measured_ns.checked_sub(*start_ns).unwrap();
            let expected = match elapsed_ns {
                0..=200_000_000 => 0.75 + (elapsed_ns as f64 / 200_000_000.0) * 0.5,
                200_000_001..=400_000_000 => {
                    1.25 + ((elapsed_ns - 200_000_000) as f64 / 200_000_000.0) * -0.5
                }
                _ => 0.75,
            };
            assert_eq!(
                frame.demand,
                Demand::PowerProfile {
                    requested_w: expected
                }
            );
            elapsed.push(elapsed_ns);
        }
        _ => {}
    });
    assert_eq!(epochs.len(), 2);
    assert_eq!(epochs[0].1.len(), 6);
    assert_eq!(epochs[1].1.len(), 2);
    assert!(epochs[0].1[5] > 400_000_000 && epochs[1].1[0] < 200_000_000);
    fixture.assert_replays("power_profile");

    fixture.reject_raw_edit(|row| {
        let Event::Inputs { frame } = &mut row.event else {
            return false;
        };
        let Demand::PowerProfile { requested_w } = &mut frame.demand else {
            return false;
        };
        *requested_w += 0.125;
        true
    });
    fixture.reject_snapshot_edit(|mode| {
        mode["points"][1]["power_w"] = yaml_serde::to_value(1.5).unwrap();
    });
    fixture.reject_snapshot_edit(|mode| {
        *mode = yaml_serde::to_value(ModeSetup::Constant { power_w: 0.75 }).unwrap();
    });
}

#[test]
fn virtual_body_uses_exact_prior_measurement_for_either_port() {
    for (port, point, index) in [
        (BodyPort::Stage2, ThermalPoint::Stage2, 1),
        (BodyPort::Tf, ThermalPoint::Tf, 2),
    ] {
        let fixture = Fixture::new();
        let mut owner = fixture.open_body(port, 0.02);
        start(&mut owner);
        let first = Instant::now() + PERIOD;
        for tick in 0..3 {
            tick_at(&mut owner, first + PERIOD * tick).unwrap();
        }
        fixture.close(owner);

        let mut prior_reading = None;
        let mut started = false;
        let mut intervals = 0;
        fixture.scan(|row| match row.event {
            Event::OutputEvidence {
                state: "on_verified",
                ..
            } => started = true,
            Event::Inputs { frame } => {
                let current = frame.temperatures[index];
                let reading = current.temperature_k.zip(current.read_started_ns);
                if let Some(step) = frame.mode_step_v1 {
                    let body = step.body.unwrap();
                    assert!(started);
                    assert_eq!(body.port, point);
                    let (temperature, started_ns): (f64, u64) = prior_reading.unwrap();
                    assert_eq!(body.held_k.to_bits(), temperature.to_bits());
                    assert_eq!(body.held_read_started_ns, started_ns);
                    assert_eq!(body.held_age_ns, step.interval_end_ns - started_ns);
                    if matches!(row.correlation, Correlation::Tick(_)) {
                        let expected_q = 0.02 * (body.temperature_k - reading.unwrap().0);
                        assert_near(body.q_v_w.unwrap(), expected_q, 1.0);
                        assert_eq!(
                            frame.demand,
                            Demand::VirtualBody {
                                requested_w: body.q_v_w.unwrap(),
                            }
                        );
                        intervals += 1;
                    }
                }
                if reading.is_some() {
                    prior_reading = reading;
                }
            }
            _ => {}
        });
        assert_eq!(intervals, 3);
        fixture.assert_replays("virtual_body");
    }
}

fn assert_near(actual: f64, expected: f64, scale: f64) {
    let tolerance = 64.0 * f64::EPSILON * actual.abs().max(expected.abs()).max(scale).max(1.0);
    assert!(
        (actual - expected).abs() <= tolerance,
        "actual {actual}, expected {expected}, tolerance {tolerance}",
    );
}

#[test]
fn virtual_body_edits_are_atomic_and_checked_in_replay() {
    let fixture = Fixture::new();
    let mut owner = fixture.open_body(BodyPort::Tf, 0.02);
    start(&mut owner);
    let first = Instant::now() + PERIOD;
    tick_at(&mut owner, first).unwrap();

    let edit = BodyEdit {
        material: Some("al6061_t6".into()),
        mass_kg: Some(0.1),
        scale: Some(0.4),
        bridge: Some(BridgeSetup::Table {
            points: vec![[5.0, 0.02], [75.0, 0.03]],
        }),
        ..BodyEdit::default()
    };
    let bridge_only = BodyEdit {
        g_w_per_k: Some(0.032),
        ..BodyEdit::default()
    };
    for invalid in [
        BodyEdit {
            material: Some("unknown".into()),
            ..BodyEdit::default()
        },
        BodyEdit {
            g_w_per_k: Some(0.03),
            ..edit.clone()
        },
    ] {
        assert!(owner.change_operator_body(invalid).is_err());
    }
    owner.change_operator_body(edit.clone()).unwrap();
    assert!(owner.change_operator_body(bridge_only.clone()).is_err());
    tick_at(&mut owner, first + PERIOD).unwrap();
    owner.change_operator_body(bridge_only.clone()).unwrap();
    tick_at(&mut owner, first + PERIOD * 2).unwrap();

    let late_rejection = BodyEdit {
        bridge: Some(BridgeSetup::Table {
            points: vec![[22.0, 0.02], [35.0, 0.02]],
        }),
        ..BodyEdit::default()
    };
    owner.change_operator_body(late_rejection.clone()).unwrap();
    fixture
        .thermometer
        .override_next_kelvin(3, Kelvin::new(13.0).unwrap())
        .unwrap();
    tick_at(&mut owner, first + PERIOD * 3).unwrap();
    fixture.close(owner);

    let catalog = fs::read_to_string(fixture.path().join("references/nist_csv")).unwrap();
    let enthalpy = |id, effective_mass, temperature| {
        let curve = nist_heat_capacity_from_catalog(id, &catalog).unwrap().curve;
        effective_mass * curve.integral(curve.bounds().0, temperature).unwrap().get()
    };
    let expected_requests =
        [edit, bridge_only, late_rejection].map(|edit| serde_json::to_value(edit).unwrap());
    let mut changes = 0;
    fixture.scan(|mut row| {
        let Some(body) = recorded_body(&mut row) else {
            return;
        };
        let Some(change) = &body.change else {
            return;
        };
        assert_eq!(change.request, expected_requests[changes]);
        match (changes, change.outcome) {
            (
                0,
                ModeChangeOutcomeV1::Applied {
                    temperature_before_k,
                    temperature_after_k,
                    dh_event_j: Some(dh),
                },
            ) => {
                assert_near(temperature_after_k, temperature_before_k, 1.0);
                assert_near(body.temperature_k, temperature_after_k, 1.0);
                let t = Kelvin::new(temperature_before_k).unwrap();
                let h_before: f64 = enthalpy("ofhc_rrr100", 0.2, t);
                let h_after: f64 = enthalpy("al6061_t6", 0.1 * 0.4, t);
                assert!(dh < 0.0);
                assert_near(dh, h_after - h_before, h_before.abs().max(h_after.abs()));
            }
            (
                1,
                ModeChangeOutcomeV1::Applied {
                    temperature_before_k,
                    temperature_after_k,
                    dh_event_j: None,
                },
            ) => assert_eq!(
                temperature_before_k.to_bits(),
                temperature_after_k.to_bits()
            ),
            (2, ModeChangeOutcomeV1::Rejected) => {
                assert_near(
                    body.q_v_w.unwrap(),
                    0.032 * (body.temperature_k - 13.0),
                    1.0,
                );
            }
            (_, outcome) => panic!("unexpected change {changes}: {outcome:?}"),
        }
        changes += 1;
    });
    assert_eq!(changes, 3);
    fixture.assert_replays("virtual_body");

    fixture.reject_raw_edit(|row| {
        let Some(body) = recorded_body(row) else {
            return false;
        };
        body.port = ThermalPoint::Stage2;
        true
    });
    fixture.reject_raw_edit(|row| {
        let Some(body) = recorded_body(row) else {
            return false;
        };
        body.held_read_started_ns += 1;
        body.held_age_ns -= 1;
        true
    });
    for field in ["material", "bridge", "enthalpy"] {
        fixture.reject_raw_edit(|row| {
            let Some(change) = recorded_body(row).and_then(|body| body.change.as_mut()) else {
                return false;
            };
            match field {
                "material" => change.request["material"] = "ss304".into(),
                "bridge" => change.request["bridge"]["points"][0][1] = 0.025.into(),
                _ => {
                    let ModeChangeOutcomeV1::Applied {
                        dh_event_j: Some(dh),
                        ..
                    } = &mut change.outcome
                    else {
                        unreachable!()
                    };
                    *dh += 0.125;
                }
            }
            true
        });
    }
}

fn recorded_body<'a>(row: &'a mut Record<'_>) -> Option<&'a mut ModeBodyStepV1> {
    let Event::Inputs { frame } = &mut row.event else {
        return None;
    };
    frame.mode_step_v1.as_mut()?.body.as_mut()
}

#[test]
fn virtual_body_thermal_time_continues_during_saturation() {
    let fixture = Fixture::new();
    let mut owner = fixture.open_body(BodyPort::Tf, 0.60);
    start(&mut owner);
    let first = Instant::now() + PERIOD;
    for tick in 0..4 {
        tick_at(&mut owner, first + PERIOD * tick).unwrap();
    }
    fixture.close(owner);

    let mut end_ns = None;
    let mut temperature_k = 30.0;
    let mut powered = false;
    let mut limited = 0;
    fixture.scan(|row| {
        if !matches!(row.correlation, Correlation::Tick(_)) {
            return;
        }
        match row.event {
            Event::Inputs { frame } => {
                let step = frame.mode_step_v1.unwrap();
                let body = step.body.unwrap();
                if let Some(previous) = end_ns {
                    assert_eq!(step.interval_end_ns - step.interval_ns, previous);
                }
                assert!(body.temperature_k < temperature_k);
                assert!(body.q_v_w.unwrap() > 6.0);
                end_ns = Some(step.interval_end_ns);
                temperature_k = body.temperature_k;
                powered |= step.heater_energy_j.iter().any(|&joules| joules > 0.0);
            }
            Event::Control {
                requested_w: Some(requested),
                limited_w: Some(limit),
                power_limited,
                fault,
                ..
            } => {
                assert!(power_limited && requested > limit && limit > 0.0);
                assert_eq!(fault, None);
                limited += 1;
            }
            _ => {}
        }
    });
    assert!(powered);
    assert_eq!(limited, 4);
    fixture.assert_replays("virtual_body");
}

#[test]
fn virtual_body_expired_hold_after_stop_cannot_fabricate_an_interval() {
    let fixture = Fixture::new();
    let mut owner = fixture.open_body(BodyPort::Tf, 0.02);
    start(&mut owner);
    tick_at(&mut owner, Instant::now() + PERIOD).unwrap();
    owner.operate(OperatorAction::Stop).unwrap();
    fixture.assert_off(&owner);

    // Schedule the next tick normally after the held reading has expired.
    thread::sleep(Duration::from_millis(1550));
    assert!(matches!(
        tick_at(&mut owner, Instant::now() + PERIOD),
        Err(Error::Safety(FaultReason::ModelFailure))
    ));
    fixture.assert_off(&owner);
    fixture.close(owner);

    let mut failure = None;
    let mut fault_recorded = false;
    fixture.scan(|row| {
        if let Event::Inputs { frame } = &row.event {
            if matches!(row.correlation, Correlation::Tick(_)) && !frame.model_valid {
                assert_eq!(frame.demand, Demand::Observe);
                assert!(frame.mode_step_v1.is_none());
                assert!(!frame.missed_tick && frame.thermometer_connected);
                assert!(failure.replace(row.correlation).is_none());
            }
        }
        if failure == Some(row.correlation) {
            match row.event {
                Event::Observation { faults, .. } => {
                    fault_recorded = faults.iter().any(|fault| fault.code == "model_failure");
                }
                Event::CommandIntent { command, .. } => assert_eq!(command, Command::Off),
                _ => {}
            }
        }
    });
    assert!(fault_recorded);
    fixture.assert_replays("virtual_body");
}

#[derive(Debug, PartialEq)]
enum OffEvidence {
    Fault,
    Intent(Channel),
    Dispatch(Channel),
    Query(Channel),
    Readback(Channel),
    Verified,
}

const OFF_COMMANDS: [(Channel, &str, &str); 2] = [
    (
        Channel::A,
        "smua.source.output = smua.OUTPUT_OFF",
        "print(smua.source.output)",
    ),
    (
        Channel::B,
        "smub.source.output = smub.OUTPUT_OFF",
        "print(smub.source.output)",
    ),
];

const VERIFIED_OFF: [OffEvidence; 9] = [
    OffEvidence::Intent(Channel::A),
    OffEvidence::Intent(Channel::B),
    OffEvidence::Dispatch(Channel::A),
    OffEvidence::Dispatch(Channel::B),
    OffEvidence::Query(Channel::A),
    OffEvidence::Readback(Channel::A),
    OffEvidence::Query(Channel::B),
    OffEvidence::Readback(Channel::B),
    OffEvidence::Verified,
];

fn push_off_evidence(event: Event<'_>, expected_fault: Option<&str>, steps: &mut Vec<OffEvidence>) {
    match event {
        Event::CommandIntent {
            channel, command, ..
        } => {
            assert_eq!(command, Command::Off);
            steps.push(OffEvidence::Intent(channel));
        }
        Event::Dispatch {
            source: "keithley",
            command,
            status: "sent",
            ..
        } => {
            for (channel, off, query) in OFF_COMMANDS {
                if command == off {
                    steps.push(OffEvidence::Dispatch(channel));
                } else if command == query {
                    steps.push(OffEvidence::Query(channel));
                }
            }
        }
        Event::Response {
            source: "keithley",
            command,
            status: "received",
            bytes: Some(bytes),
            ..
        } => {
            if let Some((channel, _, _)) =
                OFF_COMMANDS.iter().find(|(_, _, query)| command == *query)
            {
                let output: f64 = std::str::from_utf8(&bytes).unwrap().trim().parse().unwrap();
                assert_eq!(output, 0.0);
                steps.push(OffEvidence::Readback(*channel));
            }
        }
        Event::OutputEvidence {
            state,
            fault,
            fallback_requested,
            ..
        } => {
            assert_eq!(state, "off_verified");
            assert_eq!(fault, expected_fault);
            assert!(!fallback_requested);
            steps.push(OffEvidence::Verified);
        }
        _ => {}
    }
}

fn hold_fault(reason: FaultReason, code: &str, inject: impl FnOnce(&LakeShoreSim)) {
    let fixture = Fixture::new();
    let mut owner = fixture.open(ModeSetup::TemperatureHold { setpoint_k: 15.2 });
    heat(&mut owner);
    inject(&fixture.thermometer);
    let due = Instant::now() + PERIOD;
    assert!(matches!(tick_at(&mut owner, due), Err(Error::Safety(actual)) if actual == reason));
    assert_eq!(owner.fault_reason(), Some(reason));
    assert_eq!(owner.state(), MachineState::FaultLatched);
    fixture.assert_off(&owner);
    fixture.close(owner);

    let mut last_tick = None;
    let mut fault_tick = None;
    let mut steps = Vec::new();
    fixture.scan(|row| {
        if let (Correlation::Tick(_), Event::Inputs { frame }) = (row.correlation, &row.event) {
            let Demand::TemperatureHold {
                stage1_k,
                stage2_k,
                at_ns,
                ..
            } = frame.demand
            else {
                panic!("expected same-frame Hold pair");
            };
            let first = frame.temperatures[0].read_started_ns.unwrap();
            let second = frame.temperatures[1].read_started_ns.unwrap();
            assert_eq!(at_ns, first.min(second));
            assert!(frame.source_connected && frame.thermometer_connected && !frame.missed_tick);
            last_tick = Some((
                row.correlation,
                stage1_k,
                stage2_k,
                frame.measured_ns - at_ns,
            ));
        }
        if let (Correlation::Tick(_), Event::Observation { faults, .. }) =
            (row.correlation, &row.event)
        {
            if !faults.is_empty() {
                let (correlation, stage1_k, stage2_k, age_ns) = last_tick.unwrap();
                assert_eq!(row.correlation, correlation);
                assert_eq!(fault_tick.replace(correlation), None);
                assert_eq!(faults.len(), 1);
                assert_eq!(faults[0].code, code);
                if reason == FaultReason::TemperatureStale {
                    assert!(age_ns > 150_000_000 && stage1_k > stage2_k);
                } else {
                    assert_eq!((stage1_k, stage2_k), (20.0, 22.0));
                    assert!(age_ns <= 150_000_000);
                }
                steps.push(OffEvidence::Fault);
            }
        }
        if fault_tick != Some(row.correlation) {
            return;
        }
        assert!(!matches!(row.event, Event::Control { .. }));
        push_off_evidence(row.event, Some(code), &mut steps);
    });
    assert_eq!(steps[0], OffEvidence::Fault);
    assert_eq!(&steps[1..], VERIFIED_OFF);
    fixture.assert_replays("temperature_hold");

    if reason == FaultReason::TemperatureStale {
        fixture.reject_raw_edit(|row| {
            let Event::Inputs { frame } = &mut row.event else {
                return false;
            };
            let Demand::TemperatureHold { at_ns, .. } = &mut frame.demand else {
                return false;
            };
            *at_ns += 1;
            true
        });
    }
}

#[test]
fn hold_stale_pair_is_recorded_as_temperature_stale() {
    hold_fault(FaultReason::TemperatureStale, "temperature_stale", |lake| {
        lake.delay_next_status(2, Duration::from_millis(200))
            .unwrap();
    });
}

#[test]
fn hold_inversion_is_recorded_as_head_inversion() {
    hold_fault(FaultReason::HeadInversion, "head_inversion", |lake| {
        lake.override_next_kelvin(2, Kelvin::new(22.0).unwrap())
            .unwrap();
    });
}

fn warm_compensation(owner: &mut RecordingLoop<'_>) {
    let first = Instant::now() + PERIOD;
    for (tick, ready) in [false, true].into_iter().enumerate() {
        assert!(!owner.compensation_ready());
        for action in [OperatorAction::Arm, OperatorAction::Start] {
            assert!(owner.operate(action).is_err());
        }
        let report = tick_at(owner, first + PERIOD * tick as u32).unwrap();
        assert_eq!(report.measured_w, Some(0.0));
        assert!(report.command_latency.is_none());
        assert_eq!(owner.output_state(), OutputState::OffVerified);
        assert_eq!(owner.compensation_ready(), ready);
    }
}

#[test]
fn compensation_negative_request_stops_both_branches_and_requires_a_fresh_start() {
    let fixture = Fixture::new();
    let mut owner = fixture.open_compensated(BodyPort::Tf, 0.05);
    warm_compensation(&mut owner);
    start(&mut owner);
    let first = Instant::now() + PERIOD;
    assert!(tick_at(&mut owner, first)
        .unwrap()
        .command_latency
        .is_some());
    fixture
        .thermometer
        .override_next_kelvin(3, Kelvin::new(15.8).unwrap())
        .unwrap();
    let negative = tick_at(&mut owner, first + PERIOD).unwrap();
    assert!(negative.measured_w.unwrap() > 0.0);
    assert!(negative.command_latency.is_none());
    fixture.assert_off(&owner);

    warm_compensation(&mut owner);
    start(&mut owner);
    let receipt = fixture.close(owner);

    let mut previous_tf = None;
    let mut negative_tick = None;
    let mut limited = false;
    let mut starts = 0;
    let mut positive_controls = 0;
    let mut off = Vec::new();
    fixture.scan(|row| {
        match &row.event {
            Event::Inputs { frame } => {
                let tf = frame.temperatures[2];
                let reading = (
                    tf.temperature_k.unwrap().to_bits(),
                    tf.read_started_ns.unwrap(),
                );
                if let Some(body) = frame
                    .mode_step_v1
                    .as_ref()
                    .and_then(|step| step.body.as_ref())
                {
                    if let Some(ModeCompensationV1::Ready {
                        previous_tf_k,
                        previous_read_started_ns,
                        current_tf_k,
                        current_read_started_ns,
                        d_tf_k_per_s,
                        fixture_capacity_j_per_k,
                        fixture_parasitic_w,
                    }) = body.compensation
                    {
                        assert_eq!(
                            previous_tf,
                            Some((previous_tf_k.to_bits(), previous_read_started_ns))
                        );
                        assert_eq!(reading, (current_tf_k.to_bits(), current_read_started_ns));
                        let dt = Duration::from_nanos(
                            current_read_started_ns - previous_read_started_ns,
                        )
                        .as_secs_f64();
                        assert_near(d_tf_k_per_s, (current_tf_k - previous_tf_k) / dt, 1.0);
                        assert_eq!(fixture_parasitic_w, 0.05);
                        if let Demand::VirtualBody { requested_w } = frame.demand {
                            let q = body.q_v_w.unwrap();
                            assert_near(q, 0.05 * (body.temperature_k - current_tf_k), 1.0);
                            assert_near(
                                requested_w,
                                q + fixture_capacity_j_per_k * d_tf_k_per_s - 0.05,
                                1.0,
                            );
                            if current_tf_k == 15.8 {
                                assert!(q > 0.0 && requested_w < 0.0);
                                assert!(negative_tick.replace(row.correlation).is_none());
                            }
                        }
                    }
                }
                previous_tf = Some(reading);
            }
            Event::Operator {
                action: "start", ..
            } => starts += 1,
            Event::OutputEvidence {
                state: "off_verified",
                ..
            } => previous_tf = None,
            Event::Control {
                requested_w: Some(requested),
                limited_w,
                targets_w,
                equivalence,
                fault,
                ..
            } => {
                assert_eq!(*fault, None);
                if Some(row.correlation) == negative_tick {
                    assert!(*requested < 0.0);
                    assert_eq!(*limited_w, Some(0.0));
                    assert_eq!(*targets_w, Some([0.0; 2]));
                    assert_eq!(*equivalence, Some("limited"));
                    limited = true;
                } else {
                    assert!(*requested > 0.0 && limited_w.unwrap() > 0.0);
                    positive_controls += 1;
                }
            }
            _ => {}
        }
        if Some(row.correlation) == negative_tick {
            push_off_evidence(row.event, None, &mut off);
        }
    });
    assert!(limited);
    assert_eq!(positive_controls, 1);
    assert_eq!(starts, 2);
    assert_eq!(off, VERIFIED_OFF);
    fixture.assert_replays("virtual_body");
    let (_, analysis) = inspect_archive(&fixture.path(), &receipt).unwrap();
    assert!(analysis
        .reason
        .unwrap()
        .contains("переменной теплоёмкости насадки"));
}

#[test]
fn compensation_lost_reading_preserves_m5_fault_and_clears_history() {
    let fixture = Fixture::new();
    let mut owner = fixture.open_compensated(BodyPort::Tf, 0.05);
    warm_compensation(&mut owner);
    start(&mut owner);
    let first = Instant::now() + PERIOD;
    tick_at(&mut owner, first).unwrap();
    fixture
        .thermometer
        .set_fault(LakeShoreFault::MalformedReading);
    assert!(matches!(
        tick_at(&mut owner, first + PERIOD),
        Err(Error::Safety(FaultReason::ThermometerLinkLost))
    ));
    fixture.assert_off(&owner);
    assert!(!owner.compensation_ready());

    fixture.thermometer.set_fault(LakeShoreFault::None);
    owner.recover().unwrap();
    assert!(!owner.compensation_ready());
    owner.operate(OperatorAction::Reset).unwrap();
    warm_compensation(&mut owner);
    start(&mut owner);
    fixture.close(owner);

    let code = "thermometer_link_lost";
    let mut fault_tick = None;
    let mut fault = false;
    let mut off = Vec::new();
    fixture.scan(|row| {
        if let Event::Inputs { frame } = &row.event {
            if !frame.thermometer_connected {
                assert_eq!(frame.demand, Demand::Observe);
                let body = frame.mode_step_v1.as_ref().unwrap().body.as_ref().unwrap();
                assert_eq!(
                    body.compensation,
                    Some(ModeCompensationV1::Unavailable {
                        reason: ModeCompensationIssue::MissingTf
                    })
                );
                assert!(fault_tick.replace(row.correlation).is_none());
            }
        }
        if Some(row.correlation) == fault_tick {
            if let Event::Observation { faults, .. } = &row.event {
                assert_eq!(faults[0].code, code);
                fault = true;
            }
            assert!(!matches!(row.event, Event::Control { .. }));
            push_off_evidence(row.event, Some(code), &mut off);
        }
    });
    assert!(fault);
    assert_eq!(off, VERIFIED_OFF);
    fixture.assert_replays("virtual_body");
}

#[test]
fn compensation_saturates_with_continuous_intervals_and_replays_evidence() {
    let fixture = Fixture::new();
    let mut owner = fixture.open_compensated(BodyPort::Stage2, 0.60);
    warm_compensation(&mut owner);
    start(&mut owner);
    let first = Instant::now() + PERIOD;
    for tick in 0..4 {
        tick_at(&mut owner, first + PERIOD * tick).unwrap();
    }
    fixture.close(owner);

    let mut end_ns = None;
    let mut temperature_k = 30.0;
    let mut powered = false;
    let mut limited = 0;
    fixture.scan(|row| match row.event {
        Event::Inputs { frame } if matches!(frame.demand, Demand::VirtualBody { .. }) => {
            let step = frame.mode_step_v1.unwrap();
            let body = step.body.unwrap();
            if let Some(previous) = end_ns {
                assert_eq!(step.interval_end_ns - step.interval_ns, previous);
            }
            assert_eq!(body.port, ThermalPoint::Stage2);
            assert!(body.temperature_k < temperature_k);
            let stage2 = frame.temperatures[1].temperature_k.unwrap();
            assert_near(
                body.q_v_w.unwrap(),
                0.60 * (body.temperature_k - stage2),
                1.0,
            );
            assert!(matches!(
                body.compensation,
                Some(ModeCompensationV1::Ready { .. })
            ));
            end_ns = Some(step.interval_end_ns);
            temperature_k = body.temperature_k;
            powered |= step.heater_energy_j.iter().any(|&joules| joules > 0.0);
        }
        Event::Control {
            requested_w: Some(requested),
            limited_w: Some(limit),
            power_limited,
            fault,
            ..
        } => {
            assert!(power_limited && requested > limit && limit > 0.0);
            assert_eq!(fault, None);
            limited += 1;
        }
        _ => {}
    });
    assert!(powered);
    assert_eq!(limited, 4);
    fixture.assert_replays("virtual_body");

    for field in [
        "previous_timestamp",
        "current_temperature",
        "derivative",
        "capacity",
    ] {
        fixture.reject_raw_edit(|row| {
            let Some(ModeCompensationV1::Ready {
                previous_read_started_ns,
                current_tf_k,
                d_tf_k_per_s,
                fixture_capacity_j_per_k,
                ..
            }) = recorded_body(row).and_then(|body| body.compensation.as_mut())
            else {
                return false;
            };
            match field {
                "previous_timestamp" => *previous_read_started_ns += 1,
                "current_temperature" => *current_tf_k += 0.001,
                "derivative" => *d_tf_k_per_s += 0.05,
                _ => *fixture_capacity_j_per_k += 0.01,
            }
            true
        });
    }
    let mut intervals = 0;
    fixture.reject_raw_edit(|row| {
        let Event::Inputs { frame } = &mut row.event else {
            return false;
        };
        let Some(step) = frame.mode_step_v1.as_mut() else {
            return false;
        };
        intervals += 1;
        if intervals != 2 {
            return false;
        }
        step.interval_ns += 1;
        true
    });
    fixture.reject_snapshot_edit(|mode| {
        mode["compensation"]["fixture_parasitic_w"] = yaml_serde::to_value(0.06).unwrap();
    });
    fixture.reject_snapshot_edit(|mode| {
        mode["compensation"]["fixture"][0]["mass_kg"] = yaml_serde::to_value(0.21).unwrap();
    });
}

#[test]
fn export_reads_back_compensation_and_accepted_edit_in_both_tables() {
    let fixture = Fixture::new();
    let mut owner = fixture.open_compensated(BodyPort::Tf, 0.05);
    warm_compensation(&mut owner);
    start(&mut owner);
    let first = Instant::now() + PERIOD;
    tick_at(&mut owner, first).unwrap();
    let bridge = BridgeSetup::Table {
        points: vec![[5.0, 0.05], [75.0, 0.05]],
    };
    owner
        .change_operator_body(BodyEdit {
            material: Some("al6061_t6".into()),
            mass_kg: Some(0.1),
            scale: Some(0.4),
            bridge: Some(bridge.clone()),
            ..BodyEdit::default()
        })
        .unwrap();
    fixture
        .thermometer
        .override_next_kelvin(3, Kelvin::new(15.8).unwrap())
        .unwrap();
    tick_at(&mut owner, first + PERIOD).unwrap();
    fixture.assert_off(&owner);
    let receipt = fixture.close(owner);
    let path = fixture.path();
    fs::write(
        path.join("completed.json"),
        serde_json::to_vec(&serde_json::json!({"receipt": receipt})).unwrap(),
    )
    .unwrap();

    let mut accepted = None;
    fixture.scan(|row| {
        let Event::Inputs { frame } = row.event else {
            return;
        };
        let Some(body) = frame.mode_step_v1.and_then(|step| step.body) else {
            return;
        };
        let Some(ModeChangeOutcomeV1::Applied {
            dh_event_j: Some(dh),
            ..
        }) = body.change.as_ref().map(|change| change.outcome)
        else {
            return;
        };
        let Some(ModeCompensationV1::Ready {
            d_tf_k_per_s,
            fixture_capacity_j_per_k,
            fixture_parasitic_w,
            ..
        }) = body.compensation
        else {
            panic!("applied compensation edit without a pair");
        };
        let Demand::VirtualBody { requested_w } = frame.demand else {
            panic!("expected signed request")
        };
        assert!(requested_w < 0.0 && body.q_v_w.unwrap() > 0.0);
        assert!(accepted
            .replace((
                row.seq,
                body.held_age_ns,
                [
                    ("requested_w", requested_w),
                    ("bridge_flow_w", body.q_v_w.unwrap()),
                    ("limited_w", 0.0),
                    ("d_tf_k_per_s", d_tf_k_per_s),
                    ("fixture_capacity_j_per_k", fixture_capacity_j_per_k),
                    ("fixture_parasitic_w", fixture_parasitic_w),
                    ("changed_mass_kg", 0.1),
                    ("changed_scale", 0.4),
                    ("dh_event_j", dh),
                ]
            ))
            .is_none());
    });
    let (seq, held_age_ns, numbers) = accepted.unwrap();
    let report: Value =
        serde_json::from_str(&export_archive(&path, Default::default()).unwrap()).unwrap();
    assert_eq!(report["status"], "complete");
    assert_eq!(report["rows"], 4);
    let derived = path.join(report["derived"].as_str().unwrap());
    let summary: Value =
        serde_json::from_slice(&fs::read(derived.join("summary.json")).unwrap()).unwrap();
    for (field, file) in [
        ("raw_sha256", "raw.ndjson.zst"),
        ("meta_sha256", "meta.json"),
    ] {
        assert_eq!(
            summary["source"][field],
            format!("{:x}", Sha256::digest(fs::read(path.join(file)).unwrap()))
        );
    }

    let csv = fs::read_to_string(derived.join("ticks.csv")).unwrap();
    let mut lines = csv.lines();
    let columns = lines.next().unwrap().split(',').collect::<Vec<_>>();
    let index = |name| columns.iter().position(|column| *column == name).unwrap();
    let reader =
        SerializedFileReader::new(fs::File::open(derived.join("ticks.parquet")).unwrap()).unwrap();
    let parquet = reader
        .get_row_iter(None)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(parquet.len(), 4);
    assert_eq!(
        parquet[0]
            .get_column_iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        columns
    );
    let mut checked_edit = false;
    for (line, row) in lines.zip(parquet) {
        let cells = line.splitn(columns.len(), ',').collect::<Vec<_>>();
        let fields = row
            .get_column_iter()
            .map(|(_, field)| field)
            .collect::<Vec<_>>();
        assert_eq!(cells[index("mode")], "virtual_body");
        assert_eq!(row.get_string(index("mode")).unwrap(), "virtual_body");
        assert_eq!(cells[index("hold_setpoint_k")], "");
        assert!(matches!(fields[index("hold_setpoint_k")], Field::Null));
        if row.get_ulong(index("seq")).unwrap() != seq {
            for name in ["changed_material", "changed_bridge_json", "dh_event_j"] {
                assert_eq!(cells[index(name)], "");
                assert!(matches!(fields[index(name)], Field::Null));
            }
            continue;
        }
        checked_edit = true;
        assert_eq!(cells[index("body_port")], "tf");
        assert_eq!(row.get_string(index("body_port")).unwrap(), "tf");
        assert_eq!(
            cells[index("held_age_ns")].parse::<u64>().unwrap(),
            held_age_ns
        );
        assert_eq!(row.get_ulong(index("held_age_ns")).unwrap(), held_age_ns);
        for (name, expected) in numbers {
            assert_eq!(
                cells[index(name)].parse::<f64>().unwrap().to_bits(),
                expected.to_bits(),
                "{name}"
            );
            assert_eq!(
                row.get_double(index(name)).unwrap().to_bits(),
                expected.to_bits(),
                "{name}"
            );
        }
        assert_eq!(cells[index("changed_material")], "al6061_t6");
        assert_eq!(
            row.get_string(index("changed_material")).unwrap(),
            "al6061_t6"
        );
        let bridge_json = row.get_string(index("changed_bridge_json")).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(bridge_json).unwrap(),
            serde_json::to_value(&bridge).unwrap()
        );
        assert_eq!(
            cells[index("changed_bridge_json")],
            format!("\"{}\"", bridge_json.replace('"', "\"\""))
        );
        assert_eq!(cells[index("changed_bridge_w_per_k")], "");
        assert!(matches!(
            fields[index("changed_bridge_w_per_k")],
            Field::Null
        ));
    }
    assert!(checked_edit);
}

#[test]
fn export_uses_modes_schema_even_for_empty_verified_profile() {
    let fixture = Fixture::new();
    let owner = fixture.open_compensated(BodyPort::Tf, 0.05);
    fixture.close(owner);
    let path = fixture.path();
    fs::write(path.join("raw.ndjson.zst"), []).unwrap();
    let archived = check_archive(&path).unwrap();
    assert!(archived.samples.is_empty());
    let replay = serde_json::to_value(archived.report).unwrap();
    assert_eq!(replay["mode"], "virtual_body");
    assert_eq!(replay["fixture_compensation"], true);
    let report: Value =
        serde_json::from_str(&export_archive(&path, Default::default()).unwrap()).unwrap();
    assert_eq!(report["status"], "partial");
    assert_eq!(report["rows"], 0);
    let derived = path.join(report["derived"].as_str().unwrap());
    let csv = fs::read_to_string(derived.join("ticks.csv")).unwrap();
    assert_eq!(csv.lines().count(), 1);
    let columns = csv.trim_end().split(',').collect::<Vec<_>>();
    assert!(columns.contains(&"mode") && columns.contains(&"hold_setpoint_k"));
    assert!(columns.contains(&"changed_bridge_json"));
    let reader =
        SerializedFileReader::new(fs::File::open(derived.join("ticks.parquet")).unwrap()).unwrap();
    let schema = reader.metadata().file_metadata().schema_descr();
    assert_eq!(
        schema
            .columns()
            .iter()
            .map(|column| column.name())
            .collect::<Vec<_>>(),
        columns
    );
    assert_eq!(reader.metadata().num_row_groups(), 0);
}

#[test]
fn source_loss_is_unknown_until_explicit_recovery_and_start() {
    let fixture = Fixture::new();
    let mut owner = fixture.open(ModeSetup::Constant { power_w: 0.75 });
    heat(&mut owner);
    fixture.source.disconnect_next_query();
    assert!(tick_at(&mut owner, Instant::now() + PERIOD).is_err());
    assert_eq!(owner.fault_reason(), Some(FaultReason::SourceLinkLost));
    assert_eq!(owner.output_state(), OutputState::Unknown);
    assert!(owner.fallback_requested());
    owner.recover().unwrap();
    fixture.assert_off(&owner);
    owner.operate(OperatorAction::Reset).unwrap();
    assert!(owner.operate(OperatorAction::Start).is_err());
    fixture.assert_off(&owner);
    heat(&mut owner);
    fixture.close(owner);

    let mut saw_unknown = false;
    fixture.scan(|row| {
        if let Event::OutputEvidence {
            state: "unknown",
            fault: Some("source_link_lost"),
            fallback_requested: true,
            ..
        } = row.event
        {
            saw_unknown = true;
        }
    });
    assert!(saw_unknown);
    fixture.assert_replays("constant");
}

#[test]
#[ignore = "20-second loopback acceptance run"]
fn hold_tracks_stage2_on_loopback() {
    let fixture = Fixture::new();
    let mut owner = fixture.open(ModeSetup::TemperatureHold { setpoint_k: 15.2 });
    start(&mut owner);
    let first = Instant::now() + PERIOD;
    let mut measured = Vec::with_capacity(200);
    for index in 0..200 {
        let tick = tick_at(&mut owner, first + PERIOD * index).unwrap();
        assert!(tick.command_latency.is_some());
        measured.push(tick.measured_w.unwrap());
    }
    fixture.close(owner);

    let mut stage2 = Vec::with_capacity(200);
    fixture.scan(|row| {
        if let (Correlation::Tick(_), Event::Inputs { frame }) = (row.correlation, row.event) {
            stage2.push(frame.temperatures[1].temperature_k.unwrap());
        }
    });
    assert_eq!(stage2.len(), 200);
    for (&temperature, &power) in stage2[180..].iter().zip(&measured[180..]) {
        assert!(
            (temperature - 15.2).abs() <= 0.05,
            "stage2: {temperature} K"
        );
        assert!(
            (power - 0.75).abs() <= 0.1,
            "actual heater power: {power} W"
        );
    }
    println!("Hold final 20 stage2 K: {:?}", &stage2[180..]);
    println!("Hold final 20 actual power W: {:?}", &measured[180..]);
    fixture.assert_replays("temperature_hold");
}
