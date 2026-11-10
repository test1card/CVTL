use std::{env, process::ExitCode};

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let path = args
        .next()
        .unwrap_or_else(|| "config/simulator.yaml".into());
    if args.next().is_some() {
        eprintln!("Использование: cvtl-daemon [config.yaml]");
        return ExitCode::FAILURE;
    }
    match cvtl_core::Config::from_file(path) {
        Ok(_) => {
            println!("Конфигурация принята. Фаза A: только симуляция; подключения к приборам нет.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("Запуск отклонён: {error}");
            ExitCode::FAILURE
        }
    }
}
