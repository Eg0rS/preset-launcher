//! Командная строка: `switch <имя>`, `status`, `list` обращаются к уже запущенному экземпляру
//! по локальному HTTP API (127.0.0.1:<порт из конфига>, токен из конфига). Без подкоманды
//! приложение запускается как обычно (GUI).
//!
//! Разбор аргументов, формирование запроса/разбор ответа и форматирование вывода — чистые
//! функции с тестами; сеть и файлы трогает только `run`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::Settings;

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAIL: i32 = 1;
pub const EXIT_USAGE: i32 = 2;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const IO_TIMEOUT: Duration = Duration::from_secs(15);
/// Общий срок запроса целиком и максимальный размер ответа.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE: usize = 1024 * 1024;

pub const HELP: &str = "\
Preset Launcher — переключатель наборов программ

Использование:
  preset-launcher                       запустить приложение (окно/трей)
  preset-launcher switch <имя пресета>  переключить пресет в запущенном экземпляре
  preset-launcher status [--json]       состояние: активный пресет, громкость, версия
  preset-launcher list [--json]         список пресетов (по одному в строке)
  preset-launcher autostart on|off|status   автозапуск при входе (только macOS)
  preset-launcher --version | --help

Команды обращаются к локальному HTTP API запущенного приложения; API должен быть включён
в настройках. Каталог конфига можно переопределить переменной PRESET_LAUNCHER_DIR.
Коды выхода: 0 — успех, 1 — ошибка (приложение не запущено, неверный токен, неизвестный
пресет…), 2 — неверные аргументы.
";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Switch(String),
    Status { json: bool },
    List { json: bool },
    Autostart(AutostartOp),
    /// Служебная: записать PNG значка (используется `scripts/bundle-macos.sh`).
    IconPng { size: u32, path: String },
    Help,
    Version,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AutostartOp {
    On,
    Off,
    Status,
}

/// Флаги обычного (GUI) запуска.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GuiArgs {
    pub autostart: bool,
    pub minimized: bool,
    pub preset: Option<String>,
}

/// Что делать с аргументами.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Gui(GuiArgs),
    Cli(Command),
    /// Неверные аргументы (текст ошибки).
    Usage(String),
}

/// GUI запускается только без аргументов или с известными флагами; всё остальное — ошибка
/// использования (опечатка в подкоманде не должна молча открыть окно).
pub fn route(args: &[String]) -> Route {
    if args.is_empty() {
        return Route::Gui(GuiArgs::default());
    }
    match parse(args) {
        Some(Ok(c)) => Route::Cli(c),
        Some(Err(e)) => Route::Usage(e),
        None => match parse_gui(args) {
            Ok(g) => Route::Gui(g),
            Err(e) => Route::Usage(e),
        },
    }
}

fn parse_gui(args: &[String]) -> Result<GuiArgs, String> {
    let mut g = GuiArgs::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--autostart" => g.autostart = true,
            "--minimized" => g.minimized = true,
            "--preset" => match it.next() {
                Some(name) if !name.trim().is_empty() => g.preset = Some(name.clone()),
                _ => return Err("после --preset нужно имя пресета".into()),
            },
            other => return Err(format!("неизвестный аргумент или подкоманда: {other}")),
        }
    }
    g.minimized |= g.autostart || g.preset.is_some();
    Ok(g)
}

/// Разбор CLI-подкоманд; `None` — первый аргумент не подкоманда.
pub fn parse(args: &[String]) -> Option<Result<Command, String>> {
    let first = args.first()?.as_str();
    let rest = &args[1..];
    let json_flag = |rest: &[String]| -> Result<bool, String> {
        match rest {
            [] => Ok(false),
            [f] if f == "--json" => Ok(true),
            _ => Err(format!("лишние аргументы: {}", rest.join(" "))),
        }
    };
    Some(match first {
        "switch" => {
            let name = rest.join(" ");
            if name.trim().is_empty() { Err("укажите имя пресета: preset-launcher switch <имя>".into()) } else { Ok(Command::Switch(name.trim().to_string())) }
        }
        "status" => json_flag(rest).map(|json| Command::Status { json }),
        "list" => json_flag(rest).map(|json| Command::List { json }),
        "autostart" => match rest {
            [op] if op == "on" => Ok(Command::Autostart(AutostartOp::On)),
            [op] if op == "off" => Ok(Command::Autostart(AutostartOp::Off)),
            [op] if op == "status" => Ok(Command::Autostart(AutostartOp::Status)),
            _ => Err("использование: preset-launcher autostart on|off|status".into()),
        },
        "icon-png" => match rest {
            [size, path] => match size.parse::<u32>() {
                Ok(size) if (16..=2048).contains(&size) => Ok(Command::IconPng { size, path: path.clone() }),
                _ => Err("размер значка — число от 16 до 2048".into()),
            },
            _ => Err("использование: preset-launcher icon-png <размер> <файл.png>".into()),
        },
        "help" | "--help" | "-h" => Ok(Command::Help),
        "version" | "--version" | "-V" => Ok(Command::Version),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// HTTP (минимальный клиент HTTP/1.1 поверх TcpStream)
// ---------------------------------------------------------------------------

/// Сырой HTTP-запрос. Тело (JSON) — для POST.
pub fn build_request(method: &str, path: &str, port: u16, token: &str, body: Option<&str>) -> Vec<u8> {
    let mut s = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nAccept: application/json\r\n");
    match body {
        Some(b) => {
            s.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}", b.len()));
        }
        None => s.push_str("\r\n"),
    }
    s.into_bytes()
}

/// Ответ получен целиком: по `Content-Length` или по завершающему chunk. Без них читаем до EOF.
pub fn response_complete(raw: &[u8]) -> bool {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { return false };
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let body_len = raw.len() - split - 4;
    if let Some(cl) = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()) {
        return body_len >= cl;
    }
    head.contains("transfer-encoding: chunked") && raw.ends_with(b"0\r\n\r\n")
}

/// Разбирает ответ: (код, тело). Поддерживает `Transfer-Encoding: chunked`.
pub fn parse_response(raw: &[u8]) -> Result<(u16, String), String> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("неполный ответ сервера")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];
    let mut lines = head.lines();
    let status_line = lines.next().ok_or("пустой ответ сервера")?;
    let status: u16 = status_line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or("не удалось разобрать код ответа")?;
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked { decode_chunked(body)? } else { body.to_vec() };
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let eol = data.windows(2).position(|w| w == b"\r\n").ok_or("повреждённый chunked-ответ")?;
        let size_str = String::from_utf8_lossy(&data[..eol]);
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16).map_err(|_| "повреждённый chunked-ответ")?;
        data = &data[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size {
            return Err("обрезанный chunked-ответ".into());
        }
        out.extend_from_slice(&data[..size]);
        data = data[size..].strip_prefix(b"\r\n").ok_or("повреждённый chunked-ответ")?;
    }
}

/// Порт и токен для обращения к API.
pub fn endpoint(s: &Settings) -> Result<(u16, &str), String> {
    if !s.api_enabled {
        return Err("API выключен: включите «Локальный API» в настройках приложения".into());
    }
    if s.api_token.trim().is_empty() {
        return Err("в конфиге нет токена API: запустите приложение и включите API".into());
    }
    Ok((s.api_port, s.api_token.trim()))
}

/// Понятное сообщение об ошибочном ответе API.
pub fn api_error(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body).ok().and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
    match status {
        401 => "неверный токен API (проверьте api_token в config.toml)".into(),
        404 if detail.as_deref() == Some("unknown preset") => "пресет с таким именем не найден (см. `preset-launcher list`)".into(),
        501 => "действие не поддерживается на этой ОС".into(),
        503 => "сервер занят, повторите позже".into(),
        _ => match detail {
            Some(d) => format!("ошибка API {status}: {d}"),
            None => format!("ошибка API {status}"),
        },
    }
}

fn request(method: &str, path: &str, body: Option<&str>) -> Result<(u16, String), String> {
    let cfg = crate::config::read_config()?;
    let (port, token) = endpoint(&cfg.settings)?;
    exchange(port, token, method, path, body)
}

/// Один запрос к `127.0.0.1:<port>`.
fn exchange(port: u16, token: &str, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String), String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| format!("не удалось подключиться к 127.0.0.1:{port} ({e}): Preset Launcher не запущен или другой порт API"))?;
    let deadline = Instant::now() + TOTAL_TIMEOUT;
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    stream.write_all(&build_request(method, path, port, token, body)).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("сервер не ответил за отведённое время".into());
        }
        let _ = stream.set_read_timeout(Some(left.min(IO_TIMEOUT)));
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > MAX_RESPONSE {
                    return Err("ответ сервера слишком большой".into());
                }
                if response_complete(&raw) {
                    break;
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err("сервер не ответил за отведённое время".into());
            }
            Err(e) => return Err(format!("ошибка чтения ответа: {e}")),
        }
    }
    parse_response(&raw)
}

// ---------------------------------------------------------------------------
// Форматирование
// ---------------------------------------------------------------------------

pub fn format_status(v: &Value) -> String {
    let mut out = String::new();
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    out.push_str(&format!("Версия: {}\n", s("version").unwrap_or("?")));
    out.push_str(&format!("Активный пресет: {}\n", s("active").unwrap_or("—")));
    out.push_str(&format!("Занят переключением: {}\n", if v.get("busy").and_then(Value::as_bool) == Some(true) { "да" } else { "нет" }));
    out.push_str(&format!("Пресеты: {}\n", format_list(v).replace('\n', ", ")));
    if let Some(vol) = v.get("volume").and_then(Value::as_u64) {
        let muted = v.get("muted").and_then(Value::as_bool) == Some(true);
        out.push_str(&format!("Громкость: {vol}%{}\n", if muted { " (звук выключен)" } else { "" }));
    }
    if let Some(p) = v.get("pending_power").and_then(Value::as_str) {
        out.push_str(&format!("Ожидает действие питания: {p}\n"));
    }
    out
}

/// Имена пресетов из ответа `/api/status`, по одному в строке.
pub fn format_list(v: &Value) -> String {
    v.get("presets")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Запуск
// ---------------------------------------------------------------------------

fn say(s: &str) {
    let _ = writeln!(std::io::stdout(), "{s}");
}

fn fail(s: &str) {
    let _ = writeln!(std::io::stderr(), "ошибка: {s}");
}

/// Аргументы командной строки; не-UTF-8 — ошибка использования, а не паника.
pub fn collect_args() -> Result<Vec<String>, String> {
    std::env::args_os()
        .skip(1)
        .map(|a| a.into_string().map_err(|_| "аргументы должны быть в кодировке UTF-8".to_string()))
        .collect()
}

/// Печатает ошибку использования и возвращает код выхода.
pub fn usage_error(msg: &str) -> i32 {
    // Из GUI-сборки на Windows (windows_subsystem) консоль родителя нужно подключить явно.
    crate::platform::attach_console();
    fail(msg);
    let _ = writeln!(std::io::stderr(), "Справка: preset-launcher --help");
    EXIT_USAGE
}

/// Выполняет CLI-команду и возвращает код выхода.
pub fn execute(cmd: Command) -> i32 {
    crate::platform::attach_console();
    run(cmd)
}

fn status_json() -> Result<(String, Value), String> {
    let (status, body) = request("GET", "/api/status", None)?;
    if status != 200 {
        return Err(api_error(status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("некорректный ответ API: {e}"))?;
    Ok((body, v))
}

pub fn run(cmd: Command) -> i32 {
    let result: Result<(), String> = match cmd {
        Command::Help => {
            say(HELP.trim_end());
            Ok(())
        }
        Command::Version => {
            say(&format!("preset-launcher {}", env!("CARGO_PKG_VERSION")));
            Ok(())
        }
        Command::Switch(name) => {
            let body = serde_json::json!({ "preset": name }).to_string();
            request("POST", "/api/switch", Some(&body)).and_then(|(status, resp)| {
                if status == 202 {
                    say(&format!("Переключение на «{name}» запущено"));
                    Ok(())
                } else {
                    Err(api_error(status, &resp))
                }
            })
        }
        Command::Status { json } => status_json().map(|(raw, v)| {
            let text = if json { raw } else { format_status(&v) };
            say(text.trim_end());
        }),
        Command::List { json } => status_json().map(|(_, v)| {
            if json {
                say(&v.get("presets").cloned().unwrap_or(Value::Array(vec![])).to_string());
            } else {
                say(&format_list(&v));
            }
        }),
        Command::Autostart(op) => autostart(op),
        Command::IconPng { size, path } => {
            let png = crate::icon_art::encode_png(&crate::icon_art::render_icon_rgba(size), size, size);
            std::fs::write(&path, png).map_err(|e| format!("не удалось записать {path}: {e}"))
        }
    };
    match result {
        Ok(()) => EXIT_OK,
        Err(e) => {
            fail(&e);
            EXIT_FAIL
        }
    }
}

#[cfg(target_os = "macos")]
fn autostart(op: AutostartOp) -> Result<(), String> {
    match op {
        AutostartOp::On => crate::platform::autostart_set(true).map(|()| say("Автозапуск включён (со следующего входа в систему)")),
        AutostartOp::Off => crate::platform::autostart_set(false).map(|()| say("Автозапуск выключен")),
        AutostartOp::Status => {
            say(if crate::platform::autostart_enabled() { "включён" } else { "выключен" });
            Ok(())
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn autostart(_op: AutostartOp) -> Result<(), String> {
    Err("на Windows автозапуск настраивается установщиком (задача «Запускать при входе»)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_commands() {
        assert!(parse(&a(&[])).is_none());
        assert!(parse(&a(&["--autostart"])).is_none());
        assert!(parse(&a(&["--preset", "Игры"])).is_none());
        assert!(parse(&a(&["--minimized"])).is_none());
        assert_eq!(parse(&a(&["switch", "Работа"])), Some(Ok(Command::Switch("Работа".into()))));
        assert_eq!(parse(&a(&["switch", "Мои", "игры"])), Some(Ok(Command::Switch("Мои игры".into()))));
        assert!(matches!(parse(&a(&["switch"])), Some(Err(_))));
        assert_eq!(parse(&a(&["status"])), Some(Ok(Command::Status { json: false })));
        assert_eq!(parse(&a(&["status", "--json"])), Some(Ok(Command::Status { json: true })));
        assert!(matches!(parse(&a(&["list", "x"])), Some(Err(_))));
        assert_eq!(parse(&a(&["list"])), Some(Ok(Command::List { json: false })));
        assert_eq!(parse(&a(&["autostart", "on"])), Some(Ok(Command::Autostart(AutostartOp::On))));
        assert!(matches!(parse(&a(&["autostart"])), Some(Err(_))));
        assert_eq!(parse(&a(&["--version"])), Some(Ok(Command::Version)));
        assert_eq!(parse(&a(&["--help"])), Some(Ok(Command::Help)));
        assert_eq!(parse(&a(&["icon-png", "64", "o.png"])), Some(Ok(Command::IconPng { size: 64, path: "o.png".into() })));
        assert!(matches!(parse(&a(&["icon-png", "5", "o.png"])), Some(Err(_))));
    }

    #[test]
    fn routing() {
        assert_eq!(route(&a(&[])), Route::Gui(GuiArgs::default()));
        assert_eq!(route(&a(&["--autostart"])), Route::Gui(GuiArgs { autostart: true, minimized: true, preset: None }));
        assert_eq!(route(&a(&["--minimized"])), Route::Gui(GuiArgs { autostart: false, minimized: true, preset: None }));
        assert_eq!(route(&a(&["--preset", "Игры"])), Route::Gui(GuiArgs { autostart: false, minimized: true, preset: Some("Игры".into()) }));
        assert!(matches!(route(&a(&["--preset"])), Route::Usage(_)));
        assert!(matches!(route(&a(&["swich", "x"])), Route::Usage(_)));
        assert!(matches!(route(&a(&["--bogus"])), Route::Usage(_)));
        assert!(matches!(route(&a(&["--autostart", "extra"])), Route::Usage(_)));
        assert!(matches!(route(&a(&["status"])), Route::Cli(Command::Status { .. })));
        assert!(matches!(route(&a(&["switch"])), Route::Usage(_)));
    }

    #[test]
    fn completeness() {
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel"));
        assert!(response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello"));
        assert!(response_complete(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\n"));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\n\r\nbody"));
    }

    #[test]
    fn request_format() {
        let r = String::from_utf8(build_request("POST", "/api/switch", 47800, "tok", Some(r#"{"preset":"Я"}"#))).unwrap();
        assert!(r.starts_with("POST /api/switch HTTP/1.1\r\n"));
        assert!(r.contains("Authorization: Bearer tok\r\n"));
        assert!(r.contains("Content-Length: 15\r\n\r\n{\"preset\":\"Я\"}"), "{r}");
        let g = String::from_utf8(build_request("GET", "/api/status", 1, "t", None)).unwrap();
        assert!(g.ends_with("Accept: application/json\r\n\r\n"));
        assert!(!g.contains("Content-Length"));
    }

    #[test]
    fn response_parse() {
        let raw = b"HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"accepted\":1}\n";
        assert_eq!(parse_response(raw), Ok((202, "{\"accepted\":1}\n".into())));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked), Ok((200, "hello world".into())));
        assert!(parse_response(b"garbage").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nshort\r\n").is_err());
    }

    #[test]
    fn endpoint_checks() {
        let mut s = Settings::default();
        assert!(endpoint(&s).unwrap_err().contains("выключен"));
        s.api_enabled = true;
        assert!(endpoint(&s).unwrap_err().contains("токен"));
        s.api_token = " abc ".into();
        assert_eq!(endpoint(&s), Ok((47800, "abc")));
    }

    #[test]
    fn error_messages() {
        assert!(api_error(401, r#"{"error":"unauthorized"}"#).contains("токен"));
        assert!(api_error(404, r#"{"error":"unknown preset"}"#).contains("не найден"));
        assert!(api_error(501, "{}").contains("не поддерживается"));
        assert_eq!(api_error(500, r#"{"error":"boom"}"#), "ошибка API 500: boom");
        assert_eq!(api_error(500, "x"), "ошибка API 500");
    }

    #[test]
    fn formatting() {
        let v = json!({"active":"Игры","busy":false,"presets":["Работа","Игры"],"version":"0.5.0","volume":40,"muted":true,"pending_power":"sleep"});
        assert_eq!(format_list(&v), "Работа\nИгры");
        let s = format_status(&v);
        assert!(s.contains("Активный пресет: Игры\n") && s.contains("Пресеты: Работа, Игры\n"), "{s}");
        assert!(s.contains("Громкость: 40% (звук выключен)") && s.contains("питания: sleep"));
        let n = format_status(&json!({"active":null,"presets":[],"version":"0.5.0","volume":null}));
        assert!(n.contains("Активный пресет: —") && !n.contains("Громкость"));
    }

    #[test]
    fn exchange_against_canned_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let body = r#"{"accepted":true}"#;
            write!(s, "HTTP/1.1 202 Accepted\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            req
        });
        let r = exchange(port, "tok", "POST", "/api/switch", Some(r#"{"preset":"A"}"#)).unwrap();
        assert_eq!(r, (202, r#"{"accepted":true}"#.to_string()));
        let req = h.join().unwrap();
        assert!(req.starts_with("POST /api/switch HTTP/1.1") && req.contains("Bearer tok") && req.ends_with(r#"{"preset":"A"}"#), "{req}");
        // порт без сервера -> понятная ошибка
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        assert!(exchange(dead_port, "t", "GET", "/api/status", None).unwrap_err().contains("не запущен"));
    }
}
