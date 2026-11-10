//! Real host timing on locally launched simulators and the file-backed journal.

use std::{
    error::Error,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cvtl_safety::{machine::OutputState, supervisor::OperatorAction};
use cvtl_sim::{
    keithley::KeithleySimulator,
    lakeshore::LakeShoreSim,
    recording::{RecordingLoop, PERIOD},
};
use cvtl_store::{session::CloseResult, writer::spawn_file};

fn p99(samples: &mut [Duration]) -> f64 {
    samples.sort_unstable();
    let index = (samples.len() * 99).div_ceil(100).saturating_sub(1);
    samples[index].as_secs_f64() * 1000.0
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let ticks = match args.as_slice() {
        [] => 600_u32,
        [option, count] if option == "--ticks" => count.parse()?,
        _ => return Err("Использование: m6_recording [--ticks ЧИСЛО]".into()),
    };
    if !(2..=10_000).contains(&ticks) {
        return Err("Число тактов должно быть от 2 до 10000".into());
    }
    let run = format!(
        "m6-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let directory = std::env::temp_dir().join(&run);
    let source = KeithleySimulator::start()?;
    let thermometer = LakeShoreSim::spawn()?;
    let epoch = Instant::now();
    let session = spawn_file(
        &run,
        1,
        directory.clone(),
        RecordingLoop::snapshot(),
        move || epoch.elapsed(),
    )?;
    let mut owner = RecordingLoop::new(&source, &thermometer, session, epoch);
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
    let mut jitter = Vec::with_capacity(ticks as usize - 1);
    let mut latency = Vec::with_capacity(ticks as usize);
    let mut previous = None;
    let mut missed = 0;
    let mut last_power = None;
    for index in 0..ticks {
        let due = start + PERIOD * index;
        while let Some(remaining) = due.checked_duration_since(Instant::now()) {
            if let Some(failure) = owner.poll().failure {
                return Err(failure.into());
            }
            thread::park_timeout(remaining.min(Duration::from_millis(5)));
        }
        let report = owner.tick(due)?;
        if let Some(prior) = previous {
            jitter.push(report.started.duration_since(prior).abs_diff(PERIOD));
        }
        previous = Some(report.started);
        latency.push(
            report
                .command_latency
                .ok_or("Нет подтверждения команды на такте")?,
        );
        missed += u32::from(report.missed);
        last_power = report.measured_w;
    }
    owner.begin_close()?;
    let completion = loop {
        match owner.poll_close() {
            CloseResult::Completed(proof) => break proof,
            CloseResult::Pending => thread::park_timeout(Duration::from_millis(2)),
            result => return Err(format!("Журнал не завершён: {result:?}").into()),
        }
    };
    let status = owner.poll();
    let jitter_ms = p99(&mut jitter);
    let latency_ms = p99(&mut latency);
    println!(
        concat!(
            "{{\"scenario\":\"test_only_m6_recording_v1\",\"ticks\":{},",
            "\"period_ms\":100,\"missed\":{},\"faults\":{},\"journal_overdue\":{},",
            "\"p99_jitter_ms\":{:.6},\"p99_sample_start_to_last_ack_ms\":{:.6},",
            "\"last_measured_w\":{:.6},\"durable_records\":{}}}"
        ),
        ticks,
        missed,
        u8::from(owner.fault_reason().is_some()),
        u8::from(status.progress_overdue || status.expired.is_some()),
        jitter_ms,
        latency_ms,
        last_power.ok_or("Нет последнего измерения мощности")?,
        completion.last_seq,
    );
    println!("Каталог прогона: {}", directory.display());
    if missed != 0
        || jitter_ms > 20.0
        || latency_ms > 300.0
        || owner.fault_reason().is_some()
        || owner.output_state() != OutputState::OffVerified
        || status.failure.is_some()
        || status.events_failed
    {
        return Err("Штатный прогон не выдержал критерии".into());
    }
    Ok(())
}
