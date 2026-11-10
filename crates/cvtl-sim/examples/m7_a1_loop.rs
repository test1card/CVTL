//! Short closed-loop A1 demonstration on local simulators and the file journal.

use std::{
    error::Error,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cvtl_core::units::WattsPerKelvin;
use cvtl_safety::{machine::OutputState, supervisor::OperatorAction};
use cvtl_sim::{
    keithley::KeithleySimulator,
    lakeshore::LakeShoreSim,
    recording::{RecordingLoop, PERIOD},
};
use cvtl_store::session::CloseResult;

fn main() -> Result<(), Box<dyn Error>> {
    let run = format!(
        "a1-loop-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let path = std::env::temp_dir().join(&run);
    let source = KeithleySimulator::start()?;
    let thermometer = LakeShoreSim::spawn()?;
    let epoch = Instant::now();
    let mut owner = RecordingLoop::new_a1(&source, &thermometer, &run, path.clone(), epoch)?;
    loop {
        let status = owner.poll();
        if let Some(failure) = status.failure {
            return Err(failure.into());
        }
        if status.ready {
            break;
        }
        if epoch.elapsed() >= Duration::from_secs(2) {
            return Err("Нет готовности журнала".into());
        }
        thread::park_timeout(Duration::from_millis(2));
    }
    owner.recover()?;
    owner.operate(OperatorAction::Arm)?;
    owner.operate(OperatorAction::Start)?;

    let start = Instant::now() + PERIOD;
    for index in 0..30 {
        let due = start + PERIOD * index;
        while let Some(remaining) = due.checked_duration_since(Instant::now()) {
            if let Some(failure) = owner.poll().failure {
                return Err(failure.into());
            }
            thread::park_timeout(remaining.min(Duration::from_millis(5)));
        }
        match index {
            7 => owner.change_a1(Some(0.2), None)?,
            15 => owner.change_a1(None, WattsPerKelvin::new(0.04))?,
            _ => {}
        }
        let report = owner.tick(due)?;
        if report.missed || report.command_latency.is_none() {
            return Err("Пропущен такт или нет подтверждения команды".into());
        }
    }

    owner.begin_close()?;
    if owner.output_state() != OutputState::OffVerified {
        return Err("OFF обеих ветвей не подтверждён".into());
    }
    let proof = loop {
        match owner.poll_close() {
            CloseResult::Completed(proof) => break proof,
            CloseResult::Pending => thread::park_timeout(Duration::from_millis(2)),
            result => return Err(format!("Журнал не завершён: {result:?}").into()),
        }
    };
    println!(
        "A1 test_only: 30 тактов, OFF подтверждён для A и B, записей {}",
        proof.last_seq
    );
    println!("Каталог прогона: {}", path.display());
    Ok(())
}
