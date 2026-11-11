// FILE: crates/cvtl-cli/src/main.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Dispatch autonomous CVTL commands
//   SCOPE: Read command arguments and report results
//   DEPENDS: M-CVTL-CORE, M-CVTL-STORE, M-CVTL-CONTROL, M-CVTL-SAFETY, M-CVTL-ANALYSIS
//   LINKS: M-CVTL-CLI
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   main - CLI entry point
// END_MODULE_MAP
mod advisor;
mod wire;

use cvtl_analysis::{
    m8::Options,
    stage_i::{StageI, MAX_LEGACY_BYTES},
};
use cvtl_cli::{analyze_archive, quantity, run_replay};
use serde::de::DeserializeOwned;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::ExitCode,
};

const HELP: &str = "CVTL — автономные расчёты и реплей\n\
    cvtl-cli wire <файл.yaml>            тепловые притоки по проводам\n\
    cvtl-cli heater-advisor <файл.yaml>  расчёт мощности для кандидатов\n\
    cvtl-cli replay <каталог>           сверка записанного прогона без приборов\n\
    cvtl-cli analyze <каталог>          Q2_eff, производные и неопределённость A1\n\
      [--start СЕК] [--window СЕК] [--seed ЧИСЛО] [--pair КАТАЛОГ]\n\
    cvtl-cli import-stage-i <файл.csv>  импорт стационарных данных этапа I\n\
    Примеры входа находятся в data/scenarios/.";

fn read_yaml<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Не удалось прочитать {}: {error}", path.display()))?;
    yaml_serde::from_str(&contents).map_err(|error| format!("Ошибка YAML: {error}"))
}

fn check_version(version: u32) -> Result<(), String> {
    if version == 1 {
        Ok(())
    } else {
        Err(format!("Неподдерживаемая версия схемы {version}"))
    }
}

fn import_stage_i(path: &Path) -> Result<String, String> {
    if !fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file()) {
        return Err(format!(
            "Ожидается обычный локальный файл {}",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_LEGACY_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| format!("Не удалось прочитать {}: {error}", path.display()))?;
    let csv = std::str::from_utf8(&bytes).map_err(|_| "Ожидается таблица в UTF-8")?;
    let stage_i = StageI::from_legacy_csv(csv).map_err(|error| error.to_string())?;
    let points = stage_i.points().map(|point| {
        json!({
            "row": point.number,
            "stage1_k": point.stage1.get(),
            "stage2_k": point.stage2.get(),
            "stage1_questioned": point.stage1_questioned,
            "inverted": point.inverted(),
            "nominal_anchor": point.nominal_anchor(),
            "branches": point.branches.map(|branch| json!({
                "current_a": branch.current.map(|current| current.get()),
                "voltage_v": branch.voltage.get(),
                "calculated_power_w": branch.calculated_power.map(|power| power.get()),
            })),
            "published_power_w": point.published_power.get(),
            "calculated_power_w": point.calculated_power.map(|power| power.get()),
        })
    });
    serde_json::to_string(&json!({
        "dataset_format": "kde420sa_stage_i_legacy_v1",
        "format_reference": "TZ.md §2.3",
        "source_sha256": format!("{:x}", Sha256::digest(&bytes)),
        "reference_conditions": {"liquid_nitrogen": false, "radiation_shield": false},
        "points": points,
        "nominal_anchor_rows": stage_i.nominal_points().map(|point| point.number).collect::<Vec<_>>(),
    }))
    .map_err(|error| format!("Ошибка формирования результата импорта: {error}"))
}

fn analysis(path: &Path, mut args: impl Iterator<Item = OsString>) -> Result<String, String> {
    let mut options = Options::default();
    let mut window = None;
    let mut seed = None;
    let mut pair = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("Укажите значение параметра анализа")?;
        match flag.to_str() {
            Some("--start") if options.start_s.is_none() => {
                options.start_s = Some(
                    value
                        .to_str()
                        .and_then(|value| value.parse::<f64>().ok())
                        .filter(|value| value.is_finite() && *value >= 0.0)
                        .ok_or("Начало окна должно быть неотрицательным числом секунд")?,
                );
            }
            Some("--window") if window.is_none() => {
                window = Some(
                    value
                        .to_str()
                        .and_then(|value| value.parse::<f64>().ok())
                        .filter(|value| (10.0..=30.0).contains(value))
                        .ok_or("Длительность окна должна быть от 10 до 30 с")?,
                );
            }
            Some("--seed") if seed.is_none() => {
                seed = Some(
                    value
                        .to_str()
                        .and_then(|value| value.parse::<u64>().ok())
                        .ok_or("Seed должен быть целым беззнаковым числом")?,
                );
            }
            Some("--pair") if pair.is_none() => pair = Some(PathBuf::from(value)),
            _ => {
                return Err(format!(
                    "Неизвестный или повторный параметр {}",
                    flag.to_string_lossy()
                ))
            }
        }
    }
    options.window_s = window.unwrap_or(options.window_s);
    options.seed = seed.unwrap_or(options.seed);
    let report = analyze_archive(path, options, pair.as_deref())?;
    serde_json::to_string(&report).map_err(|error| format!("Ошибка формирования анализа ({error})"))
}

fn run() -> Result<(String, ExitCode), String> {
    let mut args = env::args_os().skip(1);
    let Some(command) = args.next() else {
        return Ok((HELP.into(), ExitCode::SUCCESS));
    };
    if command == "--help" || command == "-h" {
        return Ok((HELP.into(), ExitCode::SUCCESS));
    }
    let path = args
        .next()
        .ok_or("Укажите входной файл или каталог прогона")?;
    if command == "analyze" {
        return analysis(Path::new(&path), args).map(|out| (out, ExitCode::SUCCESS));
    }
    if args.next().is_some() {
        return Err("Ожидается один входной путь".into());
    }
    match command.to_str() {
        Some("wire") => wire::run(Path::new(&path)).map(|out| (out, ExitCode::SUCCESS)),
        Some("heater-advisor") => {
            advisor::run(Path::new(&path)).map(|out| (out, ExitCode::SUCCESS))
        }
        Some("replay") => run_replay(Path::new(&path)),
        Some("import-stage-i") => {
            import_stage_i(Path::new(&path)).map(|out| (out, ExitCode::SUCCESS))
        }
        _ => Err(format!("Неизвестная команда {}", command.to_string_lossy())),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok((output, code)) => {
            println!("{}", output.trim_end());
            code
        }
        Err(error) => {
            eprintln!("{error}\nСправка: cvtl-cli --help");
            ExitCode::FAILURE
        }
    }
}
