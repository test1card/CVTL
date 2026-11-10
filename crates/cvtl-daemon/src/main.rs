// FILE: crates/cvtl-daemon/src/main.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Serve the local simulator interface and validate startup configuration
//   SCOPE: Loopback routes, owner admission and bounded read jobs
//   DEPENDS: M-CVTL-CORE, M-CVTL-SIM, M-CVTL-CLI
//   LINKS: M-CVTL-DAEMON
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   main - YAML validation or simulator-only web service
// END_MODULE_MAP

mod archive;
mod http;
mod operator;
mod scenario;

use std::{
    env,
    ffi::OsString,
    fs,
    io::{self, Write},
    net::{Ipv4Addr, TcpListener},
    path::PathBuf,
    process::ExitCode,
    sync::{mpsc, Arc, Mutex},
    thread,
};

use http::{Assets, Method, Request, Response};
use operator::{ActionRequest, Operator, Prepared};
use scenario::Setup;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let first = args
        .next()
        .unwrap_or_else(|| "config/simulator.yaml".into());
    if first != "serve" {
        if args.next().is_some() {
            eprintln!("Использование: cvtl-daemon [config.yaml]");
            return ExitCode::FAILURE;
        }
        return match cvtl_core::Config::from_file(first) {
            Ok(_) => {
                println!(
                    "Конфигурация принята. Фаза A: только симуляция; подключения к приборам нет."
                );
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Запуск отклонён: {error}");
                ExitCode::FAILURE
            }
        };
    }
    match serve(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Запуск отклонён ({error})");
            ExitCode::FAILURE
        }
    }
}

fn serve(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
    let mut assets = None;
    let mut runs = None;
    let mut port = None;
    let mut test_faults = false;
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--assets") if assets.is_none() => {
                assets = Some(PathBuf::from(
                    args.next().ok_or("Укажите каталог ресурсов")?,
                ));
            }
            Some("--runs") if runs.is_none() => {
                runs = Some(PathBuf::from(
                    args.next().ok_or("Укажите каталог прогонов")?,
                ));
            }
            Some("--port") if port.is_none() => {
                port = Some(
                    args.next()
                        .and_then(|value| value.to_str()?.parse::<u16>().ok())
                        .ok_or("Недопустимый порт")?,
                );
            }
            Some("--test-faults") if !test_faults => test_faults = true,
            _ => {
                return Err(format!(
                    "Неизвестный или повторный параметр {}",
                    flag.to_string_lossy()
                ))
            }
        }
    }
    let assets = Assets::load(&assets.ok_or("Укажите каталог ресурсов --assets")?)?;
    let runs = runs.ok_or("Укажите каталог прогонов --runs")?;
    fs::create_dir_all(&runs)
        .map_err(|error| format!("Не удалось создать каталог прогонов ({error})"))?;
    let runs = runs
        .canonicalize()
        .map_err(|error| format!("Недоступен каталог прогонов ({error})"))?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port.unwrap_or(0)))
        .map_err(|error| format!("Не удалось открыть локальный порт ({error})"))?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let mut app = App {
        owner: Operator::launch(runs.clone(), test_faults),
        reader: ReadWorker::launch(runs),
        next_id: 0,
    };
    println!("http://{address}");
    io::stdout().flush().map_err(|error| error.to_string())?;
    http::serve(listener, assets, |request| app.route(request))
}

struct App {
    owner: Operator,
    reader: ReadWorker,
    next_id: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRecording {
    preview_id: u64,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ArchiveRequest {
    List,
    Replay { run: String },
}

impl App {
    fn route(&mut self, request: Request) -> Response {
        self.handle(request).unwrap_or_else(|response| response)
    }

    fn handle(&mut self, request: Request) -> Result<Response, Response> {
        let response = match (request.method, request.path.as_str()) {
            (Method::Get, "/api/defaults") => Response::json(200, &Setup::default()),
            (Method::Get, "/api/state") => Response::json(200, &self.owner.snapshot()),
            (Method::Get, "/api/job") => {
                let last = self.reader.state.lock().unwrap().last.clone();
                Response::json(200, &last.as_deref())
            }
            (Method::Post, "/api/preview") => {
                let setup = parse(&request.body)?;
                self.read_job(ReadRequest::Preview(setup))?
            }
            (Method::Post, "/api/archive") => {
                let query = parse(&request.body)?;
                self.read_job(ReadRequest::Archive(query))?
            }
            (Method::Post, "/api/run") => {
                let request: StartRecording = parse(&request.body)?;
                let prepared = self.reader.prepared(request.preview_id).ok_or_else(|| {
                    Response::error(409, "Подготовьте актуальный прогноз сценария")
                })?;
                let id = self.request_id()?;
                self.owner
                    .create(id, prepared)
                    .map_err(|error| Response::error(409, error))?;
                queued(id)
            }
            (Method::Post, "/api/action") => {
                let request: ActionRequest = parse(&request.body)?;
                let id = self.request_id()?;
                self.owner
                    .submit(id, request)
                    .map_err(|error| Response::error(409, error))?;
                queued(id)
            }
            _ => Response::error(404, "Запрошенный ресурс отсутствует"),
        };
        Ok(response)
    }

    fn request_id(&mut self) -> Result<u64, Response> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| Response::error(503, "Исчерпан счётчик заявок"))?;
        Ok(self.next_id)
    }

    fn read_job(&mut self, request: ReadRequest) -> Result<Response, Response> {
        let id = self.request_id()?;
        self.reader
            .enqueue(id, request)
            .map_err(|error| Response::error(409, error))?;
        Ok(queued(id))
    }
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(body)
        .map_err(|error| Response::error(400, &format!("Некорректные поля запроса ({error})")))
}

fn queued(id: u64) -> Response {
    Response::json(202, &json!({"id": id}))
}

enum ReadRequest {
    Preview(Setup),
    Archive(ArchiveRequest),
}

#[derive(Serialize)]
struct ReadResult {
    id: u64,
    value: Option<Value>,
    error: Option<String>,
}

#[derive(Default)]
struct ReadState {
    busy: bool,
    last: Option<Arc<ReadResult>>,
    prepared: Option<(u64, Arc<Prepared>)>,
}

struct ReadWorker {
    queue: mpsc::SyncSender<(u64, ReadRequest)>,
    state: Arc<Mutex<ReadState>>,
}

impl ReadWorker {
    fn launch(runs: PathBuf) -> Self {
        let (queue, jobs) = mpsc::sync_channel::<(u64, ReadRequest)>(1);
        let state = Arc::new(Mutex::new(ReadState::default()));
        let published = Arc::clone(&state);
        thread::spawn(move || {
            while let Ok((id, request)) = jobs.recv() {
                let mut prepared = None;
                let outcome = match request {
                    ReadRequest::Preview(setup) => setup.estimate().and_then(|estimate| {
                        let value = serialize(&estimate)?;
                        prepared = Some((id, Arc::new(Prepared { setup, estimate })));
                        Ok(value)
                    }),
                    ReadRequest::Archive(ArchiveRequest::List) => {
                        archive::list(&runs).and_then(serialize)
                    }
                    ReadRequest::Archive(ArchiveRequest::Replay { run }) => {
                        archive::review(&runs, &run).and_then(serialize)
                    }
                };
                let (value, error) = match outcome {
                    Ok(value) => (Some(value), None),
                    Err(error) => (None, Some(error)),
                };
                let mut state = published.lock().unwrap();
                if prepared.is_some() {
                    state.prepared = prepared;
                }
                state.last = Some(Arc::new(ReadResult { id, value, error }));
                state.busy = false;
            }
        });
        Self { queue, state }
    }

    fn enqueue(&self, id: u64, request: ReadRequest) -> Result<(), &'static str> {
        let mut state = self.state.lock().unwrap();
        if state.busy {
            return Err("Идёт расчёт или чтение архива");
        }
        let preview = matches!(&request, ReadRequest::Preview(_));
        self.queue
            .try_send((id, request))
            .map_err(|_| "Рабочий поток недоступен")?;
        if preview {
            state.prepared = None;
        }
        state.busy = true;
        Ok(())
    }

    fn prepared(&self, id: u64) -> Option<Arc<Prepared>> {
        self.state
            .lock()
            .unwrap()
            .prepared
            .as_ref()
            .filter(|(candidate, _)| *candidate == id)
            .map(|(_, prepared)| Arc::clone(prepared))
    }
}

fn serialize(value: impl Serialize) -> Result<Value, String> {
    serde_json::to_value(value)
        .map_err(|error| format!("Не удалось подготовить результат ({error})"))
}
