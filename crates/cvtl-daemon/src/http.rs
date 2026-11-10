// FILE: crates/cvtl-daemon/src/http.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Serve the operator interface over bounded local HTTP
//   SCOPE: Loopback HTTP/1.1 framing, origin checks and preloaded assets
//   DEPENDS: httparse, serde, serde_json
//   LINKS: M-CVTL-DAEMON
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   Request - validated HTTP method, path and body
//   Response - bounded-deadline HTTP response
//   Assets - checked static build loaded at startup
//   serve - sequential loopback listener and dispatch
// END_MODULE_MAP

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, TcpListener, TcpStream},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;

const MAX_HEAD_BYTES: usize = 8 * 1024;
const MAX_HEADERS: usize = 32;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_ASSET_FILES: usize = 128;
const MAX_ASSET_BYTES: usize = 8 * 1024 * 1024;
const IO_BUDGET: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Method {
    Get,
    Post,
}

pub(crate) struct Request {
    pub method: Method,
    pub path: String,
    pub body: Vec<u8>,
}

#[derive(Clone)]
pub(crate) struct Response {
    status: u16,
    content_type: &'static str,
    body: Arc<[u8]>,
}

impl Response {
    pub(crate) fn json(status: u16, value: &impl Serialize) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => Self::new(status, "application/json", body.into()),
            Err(_) => Self::error(500, "Не удалось сформировать ответ"),
        }
    }

    pub(crate) fn error(status: u16, message: &str) -> Self {
        let body = serde_json::json!({ "error": message }).to_string();
        Self::new(status, "application/json", body.into_bytes().into())
    }

    fn new(status: u16, content_type: &'static str, body: Arc<[u8]>) -> Self {
        Self {
            status,
            content_type,
            body,
        }
    }

    fn write(&self, stream: &mut TcpStream) -> io::Result<()> {
        let deadline = Instant::now() + IO_BUDGET;
        let head = format!(
            concat!(
                "HTTP/1.1 {} {}\r\n",
                "Content-Type: {}\r\nContent-Length: {}\r\n",
                "Connection: close\r\nCache-Control: no-store\r\n",
                "X-Content-Type-Options: nosniff\r\n\r\n"
            ),
            self.status,
            reason(self.status),
            self.content_type,
            self.body.len(),
        );
        write_bytes(stream, head.as_bytes(), deadline)?;
        write_bytes(stream, &self.body, deadline)?;
        remaining(deadline, Instant::now())?;
        Ok(())
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Response",
    }
}

pub(crate) struct Assets {
    index: Response,
    files: HashMap<String, Response>,
}

impl Assets {
    pub(crate) fn load(dir: &Path) -> Result<Self, String> {
        let metadata = fs::symlink_metadata(dir).map_err(|error| asset_io(dir, error))?;
        if !metadata.is_dir() {
            return Err(format!(
                "Требуется обычный каталог сборки {}",
                dir.display()
            ));
        }
        let mut budget = MAX_ASSET_BYTES;
        let index = Response::new(
            200,
            "text/html; charset=utf-8",
            read_asset(&dir.join("index.html"), &mut budget)?,
        );
        let mut files = HashMap::new();
        let assets = dir.join("assets");
        match fs::symlink_metadata(&assets) {
            Ok(metadata) if metadata.is_dir() => {
                let entries = fs::read_dir(&assets).map_err(|error| asset_io(&assets, error))?;
                for entry in entries {
                    if files.len() == MAX_ASSET_FILES {
                        return Err("Слишком много файлов сборки интерфейса".into());
                    }
                    let entry = entry.map_err(|error| asset_io(&assets, error))?;
                    let name = entry
                        .file_name()
                        .into_string()
                        .ok()
                        .filter(|name| safe_asset_name(name))
                        .ok_or("Недопустимое имя файла сборки")?;
                    let content_type = match name.rsplit_once('.').map(|(_, extension)| extension) {
                        Some("js" | "mjs") => "text/javascript; charset=utf-8",
                        Some("css") => "text/css; charset=utf-8",
                        Some("png") => "image/png",
                        Some("svg") => "image/svg+xml",
                        Some("woff2") => "font/woff2",
                        _ => "application/octet-stream",
                    };
                    let body = read_asset(&entry.path(), &mut budget)?;
                    files.insert(name, Response::new(200, content_type, body));
                }
            }
            Ok(_) => return Err(format!("Требуется обычный каталог {}", assets.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(asset_io(&assets, error)),
        }
        Ok(Self { index, files })
    }

    fn response(&self, request: &Request) -> Option<Response> {
        if request.method != Method::Get {
            return None;
        }
        if request.path == "/" {
            return Some(self.index.clone());
        }
        let name = request.path.strip_prefix("/assets/")?;
        Some(
            self.files
                .get(name)
                .cloned()
                .unwrap_or_else(|| Response::error(404, "Файл интерфейса не найден")),
        )
    }
}

fn safe_asset_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && name.bytes().all(path_character)
}

fn path_character(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

fn asset_io(path: &Path, error: io::Error) -> String {
    format!("Не удалось прочитать {} ({error})", path.display())
}

fn read_asset(path: &Path, budget: &mut usize) -> Result<Arc<[u8]>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| asset_io(path, error))?;
    if !metadata.is_file() {
        return Err(format!("Требуется обычный файл {}", path.display()));
    }
    if metadata.len() > *budget as u64 {
        return Err("Размер сборки интерфейса превышает предел".into());
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(*budget as u64 + 1).read_to_end(&mut bytes))
        .map_err(|error| asset_io(path, error))?;
    if bytes.len() > *budget {
        return Err("Размер сборки интерфейса превышает предел".into());
    }
    *budget -= bytes.len();
    Ok(bytes.into())
}

pub(crate) fn serve(
    listener: TcpListener,
    assets: Assets,
    mut dispatch: impl FnMut(Request) -> Response,
) -> Result<(), String> {
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    if address.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Err("Интерфейс доступен только через 127.0.0.1".into());
    }
    let host = address.to_string();
    let origin = format!("http://{host}");
    for connection in listener.incoming() {
        let mut stream = match connection {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        let _ = stream.set_nodelay(true);
        let response = match read_request(&mut stream, &host, &origin) {
            Ok(request) => assets
                .response(&request)
                .unwrap_or_else(|| dispatch(request)),
            Err(response) => response,
        };
        let _ = response.write(&mut stream);
    }
    Ok(())
}

fn invalid_request() -> Response {
    Response::error(400, "Недопустимый HTTP-запрос")
}

fn large_head() -> Response {
    Response::error(431, "Превышен предел заголовков запроса")
}

fn large_body() -> Response {
    Response::error(413, "Превышен предел тела запроса")
}

fn single_header<'a>(
    headers: &[httparse::Header<'a>],
    name: &str,
) -> Result<Option<&'a [u8]>, Response> {
    let mut matching = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case(name));
    let first = matching.next().map(|header| header.value);
    if matching.next().is_some() {
        return Err(invalid_request());
    }
    Ok(first)
}

fn parse_head(head: &[u8], host: &str, origin: &str) -> Result<(Method, String, usize), Response> {
    if head.len() > MAX_HEAD_BYTES {
        return Err(large_head());
    }
    if !head.ends_with(b"\r\n\r\n")
        || head.starts_with(b"\n")
        || head
            .windows(2)
            .any(|pair| (pair[0] == b'\r') != (pair[1] == b'\n'))
    {
        return Err(invalid_request());
    }
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(head) {
        Ok(httparse::Status::Complete(end)) if end == head.len() && request.version == Some(1) => {}
        Err(httparse::Error::TooManyHeaders) => return Err(large_head()),
        _ => return Err(invalid_request()),
    }
    if single_header(request.headers, "Host")?.ok_or_else(invalid_request)? != host.as_bytes() {
        return Err(Response::error(
            403,
            "Адрес Host не совпадает с адресом интерфейса",
        ));
    }
    for header in request.headers.iter() {
        if ["Transfer-Encoding", "Content-Encoding", "Expect", "Upgrade"]
            .iter()
            .any(|name| header.name.eq_ignore_ascii_case(name))
            || (header.name.eq_ignore_ascii_case("Connection")
                && header
                    .value
                    .split(|byte| *byte == b',')
                    .any(|token| token.trim_ascii().eq_ignore_ascii_case(b"upgrade")))
        {
            return Err(invalid_request());
        }
    }
    let method = match request.method {
        Some("GET") => Method::Get,
        Some("POST") => Method::Post,
        _ => return Err(Response::error(405, "Метод запроса не поддерживается")),
    };
    let path = request.path.ok_or_else(invalid_request)?;
    if !path.starts_with('/')
        || path.contains("//")
        || path.split('/').any(|part| matches!(part, "." | ".."))
        || !path
            .bytes()
            .all(|byte| byte == b'/' || path_character(byte))
    {
        return Err(invalid_request());
    }
    if method == Method::Post {
        if single_header(request.headers, "Origin")? != Some(origin.as_bytes()) {
            return Err(Response::error(
                403,
                "Запрос пришёл со страницы другого источника",
            ));
        }
        let media_type = single_header(request.headers, "Content-Type")?
            .and_then(|value| value.split(|byte| *byte == b';').next())
            .map(<[u8]>::trim_ascii);
        if !media_type.is_some_and(|value| value.eq_ignore_ascii_case(b"application/json")) {
            return Err(Response::error(
                415,
                "Для POST требуется тело application/json",
            ));
        }
    }
    let length = match single_header(request.headers, "Content-Length")? {
        Some(bytes) if !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit) => {
            std::str::from_utf8(bytes)
                .map_err(|_| invalid_request())?
                .parse::<usize>()
                .map_err(|_| large_body())?
        }
        None if method == Method::Get => 0,
        _ => return Err(invalid_request()),
    };
    if length > MAX_BODY_BYTES {
        return Err(large_body());
    }
    if method == Method::Get && length != 0 {
        return Err(invalid_request());
    }
    Ok((method, path.to_owned(), length))
}

fn read_request(stream: &mut TcpStream, host: &str, origin: &str) -> Result<Request, Response> {
    let deadline = Instant::now() + IO_BUDGET;
    let mut buffer = [0; MAX_HEAD_BYTES];
    let mut filled: usize = 0;
    let header_end = loop {
        if filled == buffer.len() {
            return Err(large_head());
        }
        let scan_from = filled.saturating_sub(3);
        filled += read_bytes(stream, &mut buffer[filled..], deadline).map_err(read_error)?;
        if let Some(end) = buffer[scan_from..filled]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            break scan_from + end + 4;
        }
    };
    let (method, path, length) = parse_head(&buffer[..header_end], host, origin)?;
    let initial_body = &buffer[header_end..filled];
    if initial_body.len() > length {
        return Err(invalid_request());
    }
    let mut body = vec![0; length];
    body[..initial_body.len()].copy_from_slice(initial_body);
    let mut filled = initial_body.len();
    while filled < body.len() {
        filled += read_bytes(stream, &mut body[filled..], deadline).map_err(read_error)?;
    }
    remaining(deadline, Instant::now()).map_err(read_error)?;
    Ok(Request { method, path, body })
}

fn remaining(deadline: Instant, now: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::ErrorKind::TimedOut.into())
}

fn read_bytes(stream: &mut TcpStream, buffer: &mut [u8], deadline: Instant) -> io::Result<usize> {
    loop {
        stream.set_read_timeout(Some(remaining(deadline, Instant::now())?))?;
        match stream.read(buffer) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn write_bytes(stream: &mut TcpStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline, Instant::now())?))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn read_error(error: io::Error) -> Response {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        Response::error(408, "Истекло время чтения запроса")
    } else {
        invalid_request()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "127.0.0.1:8123";
    const ORIGIN: &str = "http://127.0.0.1:8123";

    fn status(head: &str) -> u16 {
        match parse_head(head.as_bytes(), HOST, ORIGIN) {
            Ok(_) => 200,
            Err(response) => response.status,
        }
    }

    #[test]
    fn accepts_local_http11_and_requires_post_origin_and_json() {
        let (method, path, len) = parse_head(
            b"POST /api/commands HTTP/1.1\r\nHost: 127.0.0.1:8123\r\nOrigin: http://127.0.0.1:8123\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n",
            HOST,
            ORIGIN,
        )
        .unwrap_or_else(|response| panic!("unexpected status {}", response.status));
        assert_eq!(
            (method, path.as_str(), len),
            (Method::Post, "/api/commands", 2)
        );
        assert_eq!(
            status("GET / HTTP/1.1\r\nHost: 127.0.0.1:8123\r\n\r\n"),
            200
        );
        assert_eq!(
            status("GET / HTTP/1.1\r\nHost: localhost:8123\r\n\r\n"),
            403
        );
        assert_eq!(
            status("POST /api/commands HTTP/1.1\r\nHost: 127.0.0.1:8123\r\nOrigin: http://127.0.0.1:8124\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n"),
            403
        );
        assert_eq!(
            status("POST /api/commands HTTP/1.1\r\nHost: 127.0.0.1:8123\r\nOrigin: http://127.0.0.1:8123\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n"),
            415
        );
        assert_eq!(
            status("POST /api/commands HTTP/1.1\r\nHost: 127.0.0.1:8123\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n"),
            403
        );
    }

    #[test]
    fn rejects_ambiguous_framing_and_protocol_switches() {
        for header in [
            "Content-Length: 0\r\nContent-Length: 0\r\n",
            "Content-Length: 1\r\n",
            "Content-Length: +0\r\n",
            "Content-Length: 0, 0\r\n",
            "Transfer-Encoding: chunked\r\n",
            "Content-Encoding: gzip\r\n",
            "Expect: 100-continue\r\n",
            "Upgrade: websocket\r\n",
            "Connection: keep-alive, Upgrade\r\n",
            "Host: 127.0.0.1:8123\r\n",
        ] {
            let head = format!("GET / HTTP/1.1\r\nHost: {HOST}\r\n{header}\r\n");
            assert_eq!(status(&head), 400, "{header}");
        }
        for target in [
            "http://127.0.0.1:8123/",
            "/assets/../index.html",
            "/assets/%2e%2e",
        ] {
            assert_eq!(
                status(&format!("GET {target} HTTP/1.1\r\nHost: {HOST}\r\n\r\n")),
                400
            );
        }
        assert_eq!(
            status(&format!("GET / HTTP/1.0\r\nHost: {HOST}\r\n\r\n")),
            400
        );
        assert_eq!(
            status(&format!("OPTIONS / HTTP/1.1\r\nHost: {HOST}\r\n\r\n")),
            405
        );
    }

    #[test]
    fn bounds_header_count_bytes_and_body_length() {
        let fields = "X-Test: v\r\n".repeat(32);
        let head = format!("GET / HTTP/1.1\r\nHost: {HOST}\r\n{fields}\r\n");
        assert_eq!(status(&head), 431);
        let prefix = format!("GET / HTTP/1.1\r\nHost: {HOST}\r\nX-Test: ");
        let head = format!("{prefix}{}\r\n\r\n", "x".repeat(8192 - prefix.len() - 4));
        assert_eq!(status(&head), 200);
        let head = format!("{prefix}{}\r\n\r\n", "x".repeat(8192 - prefix.len() - 3));
        assert_eq!(status(&head), 431);
        for (length, expected) in [(65536, 200), (65537, 413)] {
            let head = format!(
                "POST /api/commands HTTP/1.1\r\nHost: {HOST}\r\nOrigin: {ORIGIN}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n"
            );
            assert_eq!(status(&head), expected);
        }
    }

    #[test]
    fn absolute_budget_decreases_until_exact_deadline() {
        let start = Instant::now();
        let deadline = start + Duration::from_millis(250);
        assert_eq!(
            remaining(deadline, start).unwrap(),
            Duration::from_millis(250)
        );
        assert_eq!(
            remaining(deadline, start + Duration::from_millis(249)).unwrap(),
            Duration::from_millis(1)
        );
        assert_eq!(
            remaining(deadline, deadline).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[cfg(unix)]
    #[test]
    fn serves_preloaded_regular_assets_and_rejects_symlinks() {
        use std::{
            os::unix::fs::symlink,
            time::{SystemTime, UNIX_EPOCH},
        };

        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let root = std::env::temp_dir().join(format!("cvtl-http-assets-{}", stamp.as_nanos()));
        let dir = root.join("assets");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&dir).unwrap();
        fs::write(root.join("index.html"), "<p>CVTL</p>").unwrap();
        let file = dir.join("operator.js");
        fs::write(&file, "console.log('original')").unwrap();
        let assets = Assets::load(&root).unwrap();
        fs::write(&file, "console.log('changed')").unwrap();
        symlink(&file, dir.join("link.js")).unwrap();
        let rejected = Assets::load(&root).is_err();
        fs::remove_dir_all(&root).unwrap();

        let response = assets
            .response(&Request {
                method: Method::Get,
                path: "/assets/operator.js".into(),
                body: Vec::new(),
            })
            .unwrap();
        assert_eq!(response.content_type, "text/javascript; charset=utf-8");
        assert_eq!(&*response.body, b"console.log('original')");
        assert!(rejected);
    }
}
