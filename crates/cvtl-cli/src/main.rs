// FILE: crates/cvtl-cli/src/main.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Dispatch autonomous CVTL commands
//   SCOPE: Read command arguments and report results
//   DEPENDS: M-CVTL-CORE, M-CVTL-STORE, M-CVTL-CONTROL, M-CVTL-SAFETY
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

use serde::de::DeserializeOwned;
use std::{env, fs, path::Path, process::ExitCode};

const HELP: &str = "CVTL — автономные расчёты и реплей\n\
    cvtl-cli wire <файл.yaml>            тепловые притоки по проводам\n\
    cvtl-cli heater-advisor <файл.yaml>  расчёт мощности для кандидатов\n\
    cvtl-cli replay <каталог>           сверка записанного прогона без приборов\n\
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
