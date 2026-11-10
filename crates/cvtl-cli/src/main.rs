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
mod replay;
mod wire;

use cvtl_analysis::stage_i::{StageI, MAX_LEGACY_BYTES};
use serde::de::DeserializeOwned;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{env, fs, io::Read, path::Path, process::ExitCode};

const HELP: &str = "CVTL — автономные расчёты и реплей\n\
    cvtl-cli wire <файл.yaml>            тепловые притоки по проводам\n\
    cvtl-cli heater-advisor <файл.yaml>  расчёт мощности для кандидатов\n\
    cvtl-cli replay <каталог>           сверка записанного прогона без приборов\n\
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

fn quantity<T>(value: Option<T>, field: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("Неконечное значение поля {field}"))
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
    if args.next().is_some() {
        return Err("Ожидается один входной путь".into());
    }
    match command.to_str() {
        Some("wire") => wire::run(Path::new(&path)).map(|out| (out, ExitCode::SUCCESS)),
        Some("heater-advisor") => {
            advisor::run(Path::new(&path)).map(|out| (out, ExitCode::SUCCESS))
        }
        Some("replay") => replay::run(Path::new(&path)),
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
