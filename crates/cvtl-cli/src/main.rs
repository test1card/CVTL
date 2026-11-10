mod advisor;
mod wire;

use serde::de::DeserializeOwned;
use std::{env, fs, path::Path, process::ExitCode};

const HELP: &str = "CVTL — автономные расчёты\n\
    cvtl-cli wire <файл.yaml>            тепловые притоки по проводам\n\
    cvtl-cli heater-advisor <файл.yaml>  расчёт мощности для кандидатов\n\
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

fn run() -> Result<String, String> {
    let mut args = env::args_os().skip(1);
    let Some(command) = args.next() else {
        return Ok(HELP.into());
    };
    if command == "--help" || command == "-h" {
        return Ok(HELP.into());
    }
    let path = args.next().ok_or("Укажите входной YAML-файл")?;
    if args.next().is_some() {
        return Err("Ожидается один входной YAML-файл".into());
    }
    match command.to_str() {
        Some("wire") => wire::run(Path::new(&path)),
        Some("heater-advisor") => advisor::run(Path::new(&path)),
        _ => Err(format!("Неизвестная команда {}", command.to_string_lossy())),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(output) => {
            println!("{}", output.trim_end());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}\nСправка: cvtl-cli --help");
            ExitCode::FAILURE
        }
    }
}
