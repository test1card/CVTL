use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use cvtl_core::units::{Kelvin, Kilograms, Seconds, Watts, WattsPerKelvin};
use cvtl_materials::nist_heat_capacity;
use cvtl_physics::{
    scenario::{BodySetup, BridgeSetup},
    Body, Bridge, PortSample,
};
use cvtl_safety::{machine::OutputState, supervisor::OperatorAction};
use cvtl_sim::{
    keithley::KeithleySimulator,
    lakeshore::LakeShoreSim,
    recording::{RecordingLoop, PERIOD},
};
use cvtl_store::{
    record::{encode_line, A1ChangeV1, A1StepV1, Correlation, Demand, Event, Record},
    recovery::{recover, Tail},
    session::CloseResult,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

struct Run(PathBuf);

enum Scenario {
    Nominal,
    SourceLoss,
    StaleTf,
    Configured(BodySetup),
}

impl Run {
    fn empty() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "cvtl-a1-replay-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn record(scenario: Scenario) -> Self {
        let fixture = Self::empty();
        let source = KeithleySimulator::start().unwrap();
        let lake = LakeShoreSim::spawn().unwrap();
        let epoch = Instant::now();
        let mut owner = match &scenario {
            Scenario::Configured(setup) => RecordingLoop::new_a1_configured(
                &source,
                &lake,
                "a1-replay-fixture",
                fixture.path(),
                epoch,
                setup,
            ),
            _ => RecordingLoop::new_a1(&source, &lake, "a1-replay-fixture", fixture.path(), epoch),
        }
        .unwrap();
        wait(|| owner.poll().ready);
        owner.recover().unwrap();
        start(&mut owner);
        tick(&mut owner).unwrap();
        match scenario {
            Scenario::SourceLoss => {
                source.disconnect_next_query();
                assert!(tick(&mut owner).is_err());
                assert_eq!(owner.output_state(), OutputState::Unknown);
                assert!(owner.fallback_requested());
                owner.recover().unwrap();
                assert_eq!(owner.output_state(), OutputState::OffVerified);
                owner.operate(OperatorAction::Reset).unwrap();
                assert!(owner.operate(OperatorAction::Start).is_err());
                start(&mut owner);
                assert!(tick(&mut owner).unwrap().command_latency.is_some());
            }
            Scenario::StaleTf => {
                thread::sleep(Duration::from_millis(1550));
                assert!(owner.tick(Instant::now()).is_err());
                assert_eq!(owner.output_state(), OutputState::OffVerified);
            }
            Scenario::Nominal => {
                assert!(owner
                    .change_a1_parameters(Kilograms::new(10.0), None, None)
                    .is_err());
                owner.change_a1(Some(0.2), None).unwrap();
                tick(&mut owner).unwrap();
                owner.change_a1(None, WattsPerKelvin::new(0.04)).unwrap();
                for _ in 0..3 {
                    tick(&mut owner).unwrap();
                }
            }
            Scenario::Configured(setup) => {
                assert!(owner.change_a1_parameters(None, None, None).is_err());
                assert!(owner
                    .change_a1_parameters(Kilograms::new(0.0), None, None)
                    .is_err());
                assert!(owner.change_a1_parameters(None, Some(1.1), None).is_err());
                assert!(owner
                    .change_a1_parameters(Kilograms::new(f64::MAX), Some(1.0), None)
                    .is_err());
                assert!(owner
                    .change_a1_parameters(Kilograms::new(f64::from_bits(1)), None, None)
                    .is_err());
                assert!(owner
                    .change_a1_parameters(None, None, WattsPerKelvin::new(f64::MAX))
                    .is_err());
                if matches!(setup.bridge, BridgeSetup::Constant { .. }) {
                    for (mass, scale, g) in [
                        (Kilograms::new(10.0), None, None),
                        (None, Some(0.2), None),
                        (Kilograms::new(16.0), Some(0.15), None),
                        (None, None, WattsPerKelvin::new(0.04)),
                    ] {
                        owner.change_a1_parameters(mass, scale, g).unwrap();
                        assert!(owner.change_a1_parameters(None, Some(0.3), None).is_err());
                        assert!(tick(&mut owner).unwrap().command_latency.is_some());
                    }
                } else {
                    assert!(owner
                        .change_a1_parameters(None, None, WattsPerKelvin::new(0.04))
                        .is_err());
                }
                assert!(tick(&mut owner).unwrap().command_latency.is_some());
            }
        }
        owner.begin_close().unwrap();
        assert_eq!(owner.output_state(), OutputState::OffVerified);
        wait(|| matches!(owner.poll_close(), CloseResult::Completed(_)));
        fixture
    }

    fn path(&self) -> PathBuf {
        self.0.join("run")
    }

    fn replay(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cvtl-cli"))
            .arg("replay")
            .arg(self.path())
            .output()
            .unwrap()
    }

    fn digests(&self) -> Vec<[u8; 32]> {
        [
            "config.snapshot.yaml",
            "meta.json",
            "raw.ndjson.zst",
            "events.ndjson",
            "references/nist_csv",
        ]
        .map(|name| Sha256::digest(fs::read(self.path().join(name)).unwrap()).into())
        .into()
    }

    fn alter(&self, mut change: impl FnMut(&mut Record<'_>) -> bool) -> u64 {
        let path = self.path().join("raw.ndjson.zst");
        let mut rows = Vec::new();
        let mut changed = None;
        let result = recover(fs::File::open(&path).unwrap(), |mut row| {
            if changed.is_none() && change(&mut row) {
                changed = Some(row.seq);
            }
            rows.push(encode_line(&row).unwrap());
        })
        .unwrap();
        assert_eq!(result.tail, Tail::FrameBoundary);
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 1).unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.window_log(18).unwrap();
        encoder
            .set_pledged_src_size(Some(rows.iter().map(|row| row.len() as u64).sum()))
            .unwrap();
        for row in rows {
            encoder.write_all(&row).unwrap();
        }
        fs::write(path, encoder.finish().unwrap()).unwrap();
        changed.expect("fixture row to alter")
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn start(owner: &mut RecordingLoop<'_>) {
    owner.operate(OperatorAction::Arm).unwrap();
    owner.operate(OperatorAction::Start).unwrap();
}

fn tick(
    owner: &mut RecordingLoop<'_>,
) -> Result<cvtl_sim::recording::TickReport, cvtl_sim::recording::Error> {
    let due = Instant::now() + PERIOD;
    thread::sleep(PERIOD);
    owner.tick(due)
}

fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready() {
        assert!(Instant::now() < deadline, "writer deadline");
        thread::park_timeout(Duration::from_millis(2));
    }
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "CLI report missing: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    })
}

#[test]
fn loop_recomputes_thermal_steps_and_control_without_writes() {
    let run = Run::record(Scenario::Nominal);
    let hashes = run.digests();
    let first = run.replay();
    let result = report(&first);
    assert!(first.status.success(), "{result}");
    assert_eq!(result["status"], "matched");
    assert_eq!(result["replayed_ticks"], 5);
    assert_eq!(result["a1"]["thermal_steps"], 5);
    assert!(result["a1"]["last_interval_end_ns"].as_u64().unwrap() > 0);
    assert_eq!(result["output_state_at_boundary"], "off_verified");
    assert_eq!(result["historical_completion"], "unknown");
    assert_eq!(run.replay().stdout, first.stdout);
    assert_eq!(run.digests(), hashes);
}

#[test]
fn derived_fields_cannot_substitute_for_the_body_calculation() {
    let run = Run::record(Scenario::Nominal);
    let raw = run.path().join("raw.ndjson.zst");
    let original = fs::read(&raw).unwrap();
    for change_enthalpy in [true, false] {
        fs::write(&raw, &original).unwrap();
        let seq = run.alter(|row| {
            let Event::Inputs { frame } = &mut row.event else {
                return false;
            };
            if change_enthalpy {
                if let Some(dh) = frame
                    .a1_step_v1
                    .as_mut()
                    .and_then(|step| step.change.as_mut())
                    .and_then(|change| change.dh_event_j.as_mut())
                {
                    *dh += 1.0;
                    return true;
                }
            } else if let Demand::VirtualBody { requested_w } = &mut frame.demand {
                *requested_w += 0.1;
                return true;
            }
            false
        });
        let output = run.replay();
        assert!(!output.status.success());
        let result = report(&output);
        assert_eq!(result["status"], "mismatch", "{result}");
        assert_eq!(result["first_diagnostic"]["seq"], seq, "{result}");
    }
}

#[test]
fn source_loss_and_manual_restart_require_causal_evidence() {
    let run = Run::record(Scenario::SourceLoss);
    let output = run.replay();
    let result = report(&output);
    assert!(output.status.success(), "{result}");
    assert_eq!(result["replayed_ticks"], 3);
    assert_eq!(result["a1"]["thermal_steps"], 3);
    assert_eq!(result["output_state_at_boundary"], "off_verified");

    let raw = run.path().join("raw.ndjson.zst");
    let original = fs::read(&raw).unwrap();
    // The fixture's second tick loses the source. Request 6 is the rejected Start.
    for operation in [Correlation::Tick(4), Correlation::Request(6)] {
        let seq = run.alter(|row| {
            if let Event::Dispatch {
                source,
                command,
                status,
                ..
            } = &mut row.event
            {
                if row.correlation == operation && *source == "lakeshore" {
                    *source = "keithley";
                    *command = "smua.source.output=1".into();
                    *status = "sent";
                    return true;
                }
            }
            false
        });
        let output = run.replay();
        assert!(!output.status.success());
        let result = report(&output);
        assert_eq!(result["status"], "mismatch", "{result}");
        assert_eq!(result["first_diagnostic"]["seq"], seq, "{result}");
        fs::write(&raw, &original).unwrap();
    }

    run.alter(|row| {
        if let Event::Response {
            source: "keithley",
            command,
            status,
            ..
        } = &mut row.event
        {
            if *status == "disconnected" && command == "print(smua.measure.iv())" {
                *status = "invalid_response";
                return true;
            }
        }
        false
    });
    let output = run.replay();
    assert!(!output.status.success());
    let result = report(&output);
    assert_eq!(result["status"], "insufficient", "{result}");
    assert_eq!(result["replayed_ticks"], 1);
}

#[test]
fn invalid_model_tick_replays_off_without_fabricating_a_thermal_step() {
    let run = Run::record(Scenario::StaleTf);
    let output = run.replay();
    let result = report(&output);
    assert!(output.status.success(), "{result}");
    assert_eq!(result["replayed_ticks"], 2);
    assert_eq!(result["a1"]["thermal_steps"], 1);
    assert_eq!(result["output_state_at_boundary"], "off_verified");
}

#[test]
fn catalog_profile_and_tail_diagnostics_are_separate() {
    let run = Run::record(Scenario::Nominal);
    let path = run.path();
    let catalog = path.join("references/nist_csv");
    let original_catalog = fs::read(&catalog).unwrap();
    let meta = path.join("meta.json");
    let original_meta = fs::read(&meta).unwrap();
    fs::remove_file(&catalog).unwrap();
    assert!(!run.replay().status.success());
    fs::write(&catalog, b"changed catalog").unwrap();
    let output = run.replay();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("SHA-256"));
    let altered = b"ofhc_rrr100,cp,4,300,0,1,0,0,0,0,0,0,0,0\n";
    fs::write(&catalog, altered).unwrap();
    let mut header: Value = serde_json::from_slice(&original_meta).unwrap();
    header["references"][0]["sha256"] = format!("{:x}", Sha256::digest(altered)).into();
    fs::write(&meta, serde_json::to_vec(&header).unwrap()).unwrap();
    let output = run.replay();
    assert!(!output.status.success());
    assert_eq!(report(&output)["status"], "mismatch");
    fs::write(&catalog, original_catalog).unwrap();

    let mut header: Value = serde_json::from_slice(&original_meta).unwrap();
    header["scenario"] = "unknown_a1_profile".into();
    fs::write(&meta, serde_json::to_vec(&header).unwrap()).unwrap();
    let output = run.replay();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("профиль"));
    fs::write(&meta, original_meta).unwrap();
    let raw = path.join("raw.ndjson.zst");
    let length = fs::metadata(&raw).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&raw)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    let output = run.replay();
    assert!(!output.status.success());
    assert_eq!(report(&output)["raw_tail"], "incomplete_frame");
}

fn configured(bridge: BridgeSetup) -> Scenario {
    Scenario::Configured(BodySetup {
        material: "ofhc_rrr100".into(),
        mass_kg: 20.0,
        temperature_k: 30.0,
        scale: 0.1,
        bridge,
    })
}

fn constant() -> Scenario {
    configured(BridgeSetup::Constant {
        conductance_w_per_k: 0.02,
    })
}

fn thermal_steps(run: &Run) -> Vec<(A1StepV1, f64, f64)> {
    let mut steps = Vec::new();
    let result = recover(
        fs::File::open(run.path().join("raw.ndjson.zst")).unwrap(),
        |row| {
            if let Event::Inputs { frame } = row.event {
                if let (Some(step), Demand::VirtualBody { requested_w }) =
                    (frame.a1_step_v1, frame.demand)
                {
                    steps.push((
                        step,
                        requested_w,
                        frame.temperatures[2].temperature_k.unwrap(),
                    ));
                }
            }
        },
    )
    .unwrap();
    assert_eq!(result.tail, Tail::FrameBoundary);
    steps
}

#[test]
fn configured_mass_scale_and_g_changes_replay_with_signed_enthalpy() {
    let run = Run::record(constant());
    let hashes = run.digests();
    let steps = thermal_steps(&run);
    assert_eq!(steps.len(), 6);
    let cp = nist_heat_capacity("ofhc_rrr100").unwrap().curve;
    let mut prior = (20.0, 0.1);
    for ((step, _, _), (mass, scale)) in steps[1..4].iter().zip([
        (Some(10.0), None),
        (None, Some(0.2)),
        (Some(16.0), Some(0.15)),
    ]) {
        let change = step.change.unwrap();
        assert_eq!(change.mass_kg, mass);
        assert_eq!(change.scale, scale);
        let before = Kelvin::new(change.temperature_before_k).unwrap();
        let after = change.temperature_after_k;
        assert!((before.get() - after).abs() <= 1e-6);
        let current = (mass.unwrap_or(prior.0), scale.unwrap_or(prior.1));
        let integral = cp.integral(cp.bounds().0, before).unwrap().get();
        let expected = (current.0 * current.1 - prior.0 * prior.1) * integral;
        let recorded = change.dh_event_j.unwrap();
        assert_eq!(recorded.is_sign_negative(), expected.is_sign_negative());
        assert!((recorded - expected).abs() <= 1e-6);
        prior = current;
    }
    assert_eq!(steps[4].0.change.unwrap().bridge_w_per_k, Some(0.04));
    assert!(steps[4].0.change.unwrap().dh_event_j.is_none());
    for (index, (step, request, tf)) in steps.iter().enumerate() {
        let g = if index < 4 { 0.02 } else { 0.04 };
        assert!((request - g * (step.body_temperature_k - tf)).abs() <= 1e-12);
        if let Some(change) = step.change.filter(|change| change.bridge_w_per_k.is_none()) {
            assert!((request - g * (change.temperature_before_k - tf)).abs() <= g * 1e-6);
        }
    }

    let mut old_mass = Body::new(
        cp,
        Kilograms::new(20.0).unwrap(),
        0.1,
        Kelvin::new(steps[1].0.body_temperature_k).unwrap(),
    )
    .unwrap();
    let following = steps[2].0;
    old_mass
        .step(
            Seconds::new(Duration::from_nanos(following.interval_ns).as_secs_f64()).unwrap(),
            PortSample {
                temperature: Kelvin::new(following.held_tf_k).unwrap(),
                age: Seconds::new(Duration::from_nanos(following.held_tf_age_ns).as_secs_f64())
                    .unwrap(),
            },
            &Bridge::Constant(WattsPerKelvin::new(0.02).unwrap()),
            Watts::new(0.0).unwrap(),
        )
        .unwrap();
    assert!(
        (old_mass.temperature().get() - following.change.unwrap().temperature_before_k).abs()
            > 1e-6
    );

    let output = run.replay();
    let result = report(&output);
    assert!(output.status.success(), "{result}");
    assert_eq!(result["replayed_ticks"], 6);
    assert_eq!(result["a1"]["thermal_steps"], 6);
    assert_eq!(result["output_state_at_boundary"], "off_verified");
    assert_eq!(run.replay().stdout, output.stdout);
    assert_eq!(run.digests(), hashes);
}

#[test]
fn configured_replay_detects_parameter_substitutions_and_rejects_mass_in_v1() {
    let run = Run::record(constant());
    let raw = run.path().join("raw.ndjson.zst");
    let original = fs::read(&raw).unwrap();
    for field in 0..3 {
        let seq = run.alter(|row| {
            let Event::Inputs { frame } = &mut row.event else {
                return false;
            };
            let Some(change) = frame
                .a1_step_v1
                .as_mut()
                .and_then(|step| step.change.as_mut())
            else {
                return false;
            };
            let value = match field {
                0 => &mut change.mass_kg,
                1 => &mut change.scale,
                _ => &mut change.bridge_w_per_k,
            };
            if let Some(value) = value {
                *value += 0.01;
                return true;
            }
            false
        });
        let out = run.replay();
        assert!(!out.status.success());
        let result = report(&out);
        assert_eq!(result["status"], "mismatch", "{result}");
        assert_eq!(result["first_diagnostic"]["seq"], seq, "{result}");
        fs::write(&raw, &original).unwrap();
    }

    let snapshot = run.path().join("config.snapshot.yaml");
    let meta_path = run.path().join("meta.json");
    let mut config: yaml_serde::Value =
        yaml_serde::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    config["test_only"]["a1"]["setup"]["mass_kg"] = yaml_serde::to_value(22.0).unwrap();
    let altered = yaml_serde::to_string(&config).unwrap();
    fs::write(&snapshot, &altered).unwrap();
    let mut meta: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    meta["config_sha256"] = format!("{:x}", Sha256::digest(altered.as_bytes())).into();
    fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    let out = run.replay();
    assert!(!out.status.success());
    assert_eq!(report(&out)["status"], "mismatch");
    meta["scenario"] = "test_only_a1_loop_v1".into();
    fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    let out = run.replay();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("профиль"));

    let legacy = Run::record(Scenario::Nominal);
    let seq = legacy.alter(|row| {
        if let Event::Inputs { frame } = &mut row.event {
            if let Some(change) = frame
                .a1_step_v1
                .as_mut()
                .and_then(|step| step.change.as_mut())
            {
                if change.scale.is_some() {
                    change.mass_kg = Some(10.0);
                    return true;
                }
            }
        }
        false
    });
    let out = legacy.replay();
    assert!(!out.status.success());
    let result = report(&out);
    assert_eq!(result["status"], "mismatch", "{result}");
    assert_eq!(result["first_diagnostic"]["seq"], seq, "{result}");
}

#[test]
fn conductor_replay_uses_archived_k_with_matching_digest() {
    let run = Run::record(configured(BridgeSetup::Conductor {
        material: "ofhc_rrr100".into(),
        area_m2: 1e-6,
        length_m: 0.10,
    }));
    let out = run.replay();
    let result = report(&out);
    assert!(out.status.success(), "{result}");
    assert_eq!(result["a1"]["thermal_steps"], 2);
    assert!(thermal_steps(&run)
        .iter()
        .all(|(step, _, _)| step.change.is_none()));

    let raw = run.path().join("raw.ndjson.zst");
    let original_raw = fs::read(&raw).unwrap();
    let seq = run.alter(|row| {
        let Event::Inputs { frame } = &mut row.event else {
            return false;
        };
        let Some(step) = &mut frame.a1_step_v1 else {
            return false;
        };
        step.change = Some(A1ChangeV1 {
            mass_kg: None,
            scale: None,
            bridge_w_per_k: Some(0.04),
            dh_event_j: None,
            temperature_before_k: step.body_temperature_k,
            temperature_after_k: step.body_temperature_k,
        });
        true
    });
    let out = run.replay();
    assert!(!out.status.success());
    assert_eq!(report(&out)["first_diagnostic"]["seq"], seq);
    fs::write(&raw, original_raw).unwrap();
    let catalog = run.path().join("references/nist_csv");
    let original = fs::read_to_string(&catalog).unwrap();
    let row = original
        .lines()
        .find(|row| row.starts_with("ofhc_rrr100,k,"))
        .unwrap();
    let altered = original.replace(row, "ofhc_rrr100,k,4,300,0,1,0,0,0,0,0,0,0,0");
    fs::write(&catalog, &altered).unwrap();
    let meta_path = run.path().join("meta.json");
    let mut meta: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    meta["references"][0]["sha256"] = format!("{:x}", Sha256::digest(altered.as_bytes())).into();
    fs::write(meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    let out = run.replay();
    assert!(!out.status.success());
    assert_eq!(report(&out)["status"], "mismatch");
}

#[test]
fn unreplayable_setup_is_rejected_before_creating_the_run() {
    let source = KeithleySimulator::start().unwrap();
    let lake = LakeShoreSim::spawn().unwrap();
    let run = Run::empty();
    let Scenario::Configured(mut setup) = configured(BridgeSetup::Table {
        points: (0..8000)
            .map(|i| [4.0 + f64::from(i) * 0.01, 0.02])
            .collect(),
    }) else {
        unreachable!()
    };
    let create = |setup: &BodySetup| {
        RecordingLoop::new_a1_configured(
            &source,
            &lake,
            "invalid",
            run.path(),
            Instant::now(),
            setup,
        )
    };
    assert!(create(&setup).is_err());
    assert!(!run.path().exists());
    setup.bridge = BridgeSetup::Constant {
        conductance_w_per_k: 0.02,
    };
    setup.mass_kg = 0.0;
    assert!(create(&setup).is_err());
    assert!(!run.path().exists());
}
