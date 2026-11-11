//! Record the M8 calibration cases on local emulators.

use std::{
    env,
    error::Error,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cvtl_core::units::{Watts, WattsPerKelvin};
use cvtl_physics::scenario::{BodySetup, BridgeSetup};
use cvtl_safety::{machine::OutputState, supervisor::OperatorAction};
use cvtl_sim::{
    keithley::KeithleySimulator,
    lakeshore::LakeShoreSim,
    recording::{A1Calibration, A1OtherHeat, RecordingLoop, PERIOD},
};
use cvtl_store::session::CloseResult;
use serde_json::{json, Value};

const TICKS: u32 = 300;

struct Case {
    run: &'static str,
    cooling_w: f64,
    unknown_heat_w: Option<f64>,
    pair: bool,
    initial_g: f64,
    changed_g: Option<f64>,
}

const CASES: [Case; 4] = [
    Case {
        run: "run-01",
        cooling_w: 3.0,
        unknown_heat_w: None,
        pair: true,
        initial_g: 0.02,
        changed_g: Some(0.04),
    },
    Case {
        run: "run-02",
        cooling_w: 5.0,
        unknown_heat_w: None,
        pair: false,
        initial_g: 0.02,
        changed_g: Some(0.04),
    },
    Case {
        run: "run-03",
        cooling_w: 3.0,
        unknown_heat_w: None,
        pair: true,
        initial_g: 0.20,
        changed_g: None,
    },
    Case {
        run: "run-04",
        cooling_w: 5.0,
        unknown_heat_w: Some(1.0),
        pair: false,
        initial_g: 0.02,
        changed_g: Some(0.04),
    },
];

fn main() -> Result<(), Box<dyn Error>> {
    let root = output_dir()?;
    fs::create_dir(&root)?;
    let root = root.canonicalize()?;
    let truth = CASES
        .iter()
        .map(|case| {
            json!({
                "run": case.run,
                "q2_w": case.cooling_w,
                "other_w": case.unknown_heat_w.unwrap_or(0.0),
                "other_measured": case.unknown_heat_w.is_none(),
            })
        })
        .collect::<Vec<_>>();
    write_json(
        &root.join("truth.json"),
        &json!({
            "profile": "synthetic_test_only",
            "start_s": 8.0,
            "window_s": 12.0,
            "seed": 20261111,
            "cf_j_per_k": 5.0,
            "cases": truth,
        }),
    )?;
    println!("{}", json!({"archive_root": root}));
    for case in &CASES {
        record(&root, case)?;
    }
    Ok(())
}

fn output_dir() -> Result<PathBuf, Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let root = match args.next() {
        Some(path) => PathBuf::from(path),
        None => {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            env::temp_dir().join(format!("cvtl-m8-{}-{now}", std::process::id()))
        }
    };
    if args.next().is_some() {
        return Err("Укажите один новый каталог для калибровочных прогонов".into());
    }
    Ok(root)
}

fn record(root: &Path, case: &Case) -> Result<(), Box<dyn Error>> {
    let path = root.join(case.run);
    let source = KeithleySimulator::start()?;
    let thermometer = LakeShoreSim::spawn()?;
    let setup = BodySetup {
        material: "ofhc_rrr100".into(),
        mass_kg: 20.0,
        temperature_k: 30.0,
        scale: 0.1,
        bridge: BridgeSetup::Constant {
            conductance_w_per_k: case.initial_g,
        },
    };
    let calibration = A1Calibration {
        cold_removal: Watts::new(case.cooling_w).unwrap(),
        other_heat: match case.unknown_heat_w {
            Some(w) => A1OtherHeat::Unmeasured(Watts::new(w).unwrap()),
            None => A1OtherHeat::Known(Watts::new(0.0).unwrap()),
        },
        wire: case.pair.then(|| BridgeSetup::Conductor {
            material: "ofhc_rrr100".into(),
            area_m2: 1e-7,
            length_m: 0.20,
        }),
        group: case.pair.then(|| "calibration-pair".into()),
    };
    let epoch = Instant::now();
    let mut owner = RecordingLoop::new_a1_calibration(
        &source,
        &thermometer,
        case.run,
        path.clone(),
        epoch,
        &setup,
        &calibration,
    )?;
    loop {
        let status = owner.poll();
        if let Some(failure) = status.failure {
            return Err(failure.into());
        }
        if status.ready {
            break;
        }
        if epoch.elapsed() >= Duration::from_secs(2) {
            return Err("Нет готовности журнала калибровки".into());
        }
        thread::park_timeout(Duration::from_millis(2));
    }
    owner.recover()?;
    owner.operate(OperatorAction::Arm)?;
    owner.operate(OperatorAction::Start)?;

    let start = Instant::now() + PERIOD;
    let mut worst_command_latency = Duration::ZERO;
    for index in 0..TICKS {
        let due = start + PERIOD * index;
        while let Some(remaining) = due.checked_duration_since(Instant::now()) {
            if let Some(failure) = owner.poll().failure {
                return Err(failure.into());
            }
            thread::park_timeout(remaining.min(Duration::from_millis(5)));
        }
        if index + 1 == 50 {
            if let Some(g) = case.changed_g {
                owner.change_a1(None, WattsPerKelvin::new(g))?;
            }
        }
        let report = owner.tick(due)?;
        let Some(latency) = report.command_latency.filter(|_| !report.missed) else {
            return Err(format!(
                "Такт {} прогона {} пропущен или команда не подтверждена",
                index + 1,
                case.run
            )
            .into());
        };
        worst_command_latency = worst_command_latency.max(latency);
    }
    let host_duration = start.elapsed();

    owner.begin_close()?;
    if owner.output_state() != OutputState::OffVerified {
        return Err("OFF обеих ветвей не подтверждён".into());
    }
    let proof = loop {
        match owner.poll_close() {
            CloseResult::Completed(proof) => break proof,
            CloseResult::Pending => thread::park_timeout(Duration::from_millis(2)),
            result => return Err(format!("Журнал не завершён ({result:?})").into()),
        }
    };
    write_json(
        &path.join("completed.json"),
        &json!({
            "receipt": {
                "run": case.run,
                "generation": proof.generation,
                "last_seq": proof.last_seq,
                "output_state": "off_verified",
            },
            "diagnostics": {"wiring": [], "fixture": []},
        }),
    )?;
    println!(
        "{}",
        json!({
            "run": case.run,
            "ticks": TICKS,
            "last_seq": proof.last_seq,
            "generation": proof.generation,
            "output_state": "off_verified",
            "host_duration_s": host_duration.as_secs_f64(),
            "max_command_latency_ms": worst_command_latency.as_secs_f64() * 1000.0,
        })
    );
    Ok(())
}

fn write_json(path: &Path, value: &Value) -> Result<(), Box<dyn Error>> {
    let pending = path.with_extension("pending");
    let mut file = File::create_new(&pending)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(pending, path)?;
    File::open(path.parent().ok_or("Нет каталога результата")?)?.sync_all()?;
    Ok(())
}
