//! Локальный HTTP API (tiny_http): статус, пресеты, питание ПК, громкость, медиа, уведомления.
//!
//! Маршрутизация (`handle`) не зависит от сети и покрыта юнит-тестами;
//! сетевой слой (`start`) лишь переводит запросы tiny_http в `Request`.

use std::io::Read;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::Settings;
use crate::engine::Engine;
use crate::logging::Level;

const MAX_BODY: usize = 16 * 1024;
const DEFAULT_POWER_DELAY: u64 = 10;
const MAX_POWER_DELAY: u64 = 600;
const MAX_TITLE_CHARS: usize = 100;
const MAX_MESSAGE_CHARS: usize = 500;
/// Максимум одновременно обрабатываемых запросов; сверх лимита — 503.
const MAX_CONCURRENT: usize = 8;
/// Сколько `Running::stop` максимум ждёт выхода accept-потока (чтобы порт освободился).
const STOP_WAIT: Duration = Duration::from_millis(250);

/// Действие питания ПК.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    Shutdown,
    Sleep,
    Hibernate,
    Lock,
    Restart,
}

impl PowerAction {
    pub fn name(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::Sleep => "sleep",
            Self::Hibernate => "hibernate",
            Self::Lock => "lock",
            Self::Restart => "restart",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "sleep" => Self::Sleep,
            "hibernate" => Self::Hibernate,
            "lock" => Self::Lock,
            "restart" => Self::Restart,
            _ => return None,
        })
    }

    fn default_delay(self) -> u64 {
        if self == Self::Lock { 0 } else { DEFAULT_POWER_DELAY }
    }
}

/// Медиаклавиша.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKey {
    PlayPause,
    Next,
    Prev,
    Stop,
}

impl MediaKey {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "play_pause" => Self::PlayPause,
            "next" => Self::Next,
            "prev" => Self::Prev,
            "stop" => Self::Stop,
            _ => return None,
        })
    }
}

/// Разобранный запрос `/api/volume`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeChange {
    pub level: Option<u8>,
    pub delta: Option<i32>,
    pub muted: Option<bool>,
}

/// То, чем API управляет. В тестах подменяется заглушкой.
pub trait Backend: Send + Sync {
    fn active(&self) -> Option<String>;
    fn busy(&self) -> bool;
    fn presets(&self) -> Vec<String>;
    fn switch(&self, preset: &str);
    /// Ставит действие питания через `delay_secs` (0 — выполнить сразу), заменяя ожидающее.
    /// `Err` — если команду поставить/выполнить не удалось.
    fn power(&self, action: PowerAction, delay_secs: u64) -> Result<(), String>;
    /// Поддерживает ли ОС действие питания (иначе API отвечает 501, не ставя таймер).
    fn power_supported(&self, _action: PowerAction) -> bool {
        true
    }
    /// Отменяет ожидающее действие питания; true, если оно действительно ожидало.
    fn cancel_shutdown(&self) -> bool;
    /// Какое действие питания ожидает исполнения.
    fn pending_power(&self) -> Option<PowerAction>;
    /// Громкость 0–100 и mute основного устройства; `None` — ошибка получения громкости.
    fn volume(&self) -> Option<(u8, bool)>;
    /// Применяет изменение и возвращает итоговые (громкость, mute).
    fn set_volume(&self, change: VolumeChange) -> Result<(u8, bool), String>;
    fn media(&self, key: MediaKey) -> Result<(), String>;
    fn notify(&self, title: &str, message: &str) -> Result<(), String>;
    fn log(&self, level: Level, msg: &str);
}

impl Backend for Engine {
    fn active(&self) -> Option<String> {
        self.shared.lock().unwrap().active.clone()
    }
    fn busy(&self) -> bool {
        self.shared.lock().unwrap().busy
    }
    fn presets(&self) -> Vec<String> {
        self.preset_names()
    }
    fn switch(&self, preset: &str) {
        Engine::switch(self, preset);
    }
    fn power(&self, action: PowerAction, delay_secs: u64) -> Result<(), String> {
        self.shutdown.schedule_after(action, Duration::from_secs(delay_secs))
    }
    fn power_supported(&self, action: PowerAction) -> bool {
        crate::platform::power_supported(action)
    }
    fn cancel_shutdown(&self) -> bool {
        self.shutdown.cancel()
    }
    fn pending_power(&self) -> Option<PowerAction> {
        self.shutdown.pending_action()
    }
    fn volume(&self) -> Option<(u8, bool)> {
        crate::platform::get_volume().ok()
    }
    fn set_volume(&self, change: VolumeChange) -> Result<(u8, bool), String> {
        crate::platform::set_volume(change)
    }
    fn media(&self, key: MediaKey) -> Result<(), String> {
        crate::platform::send_media_key(key)
    }
    fn notify(&self, title: &str, message: &str) -> Result<(), String> {
        // Тост показывается в фоне: ответ 202 уходит сразу, ошибка показа — только в журнал.
        let _slot = crate::platform::acquire_toast_slot().ok_or_else(|| NOTIFY_BUSY.to_string())?;
        let (title, message, engine) = (title.to_string(), message.to_string(), self.clone());
        std::thread::Builder::new()
            .name("preset-toast".into())
            .spawn(move || {
                let _slot = _slot;
                if let Err(e) = crate::platform::show_toast(&title, &message) {
                    Engine::log_error(&engine, format!("API: уведомление не показано — {e}"));
                }
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn log(&self, level: Level, msg: &str) {
        Engine::log_at(self, level, msg);
    }
}

// ---------------------------------------------------------------------------
// Отменяемое отложенное действие питания
// ---------------------------------------------------------------------------

type ExecFn = Arc<dyn Fn(PowerAction) -> Result<(), String> + Send + Sync>;
pub type LogFn = Arc<dyn Fn(Level, &str) + Send + Sync>;

/// Значение `Err` из `Backend::notify`, означающее «слишком много показов тостов сразу» (-> 503).
pub const NOTIFY_BUSY: &str = "busy";

/// Значение `Err` из `Backend`, означающее «действие не поддерживается на этой ОС» (-> 501).
pub const UNSUPPORTED: &str = "unsupported";

/// Отложенное действие питания (одно ожидающее; новое заменяет старое).
/// Задержка отсчитывается здесь, а не в системной команде, поэтому её можно отменить.
pub struct ShutdownTimer {
    /// (id, дедлайн, действие) ожидающего действия.
    pending: Arc<Mutex<Option<(u64, Instant, PowerAction)>>>,
    next_id: AtomicU64,
    exec: ExecFn,
    log: LogFn,
}

impl ShutdownTimer {
    pub fn new(exec: ExecFn, log: LogFn) -> Self {
        Self { pending: Arc::new(Mutex::new(None)), next_id: AtomicU64::new(1), exec, log }
    }

    pub fn pending_action(&self) -> Option<PowerAction> {
        self.pending.lock().unwrap().map(|(_, _, a)| a)
    }

    /// true, если ожидающее действие было и отменено.
    pub fn cancel(&self) -> bool {
        self.pending.lock().unwrap().take().is_some()
    }

    /// Нулевая задержка — выполняет сразу и возвращает результат. Иначе ставит таймер
    /// (заменяя ранее поставленный); ошибка исполнения по таймеру пишется в журнал.
    pub fn schedule_after(&self, action: PowerAction, delay: Duration) -> Result<(), String> {
        if delay.is_zero() {
            self.cancel();
            return run_exec(&self.exec, &self.log, action);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + delay;
        // Лок держим на время spawn: новый поток первым делом берёт его же, поэтому увидит
        // уже опубликованное pending; при неудаче spawn прежнее действие остаётся нетронутым.
        let mut guard = self.pending.lock().unwrap();
        let pending = self.pending.clone();
        let (exec, log) = (self.exec.clone(), self.log.clone());
        let spawned = thread::Builder::new().name("preset-power".into()).spawn(move || {
            loop {
                {
                    let mut p = pending.lock().unwrap();
                    match *p {
                        Some((i, dl, _)) if i == id => {
                            if Instant::now() >= dl {
                                *p = None;
                                break;
                            }
                        }
                        _ => return, // отменено или заменено
                    }
                }
                let left = deadline.saturating_duration_since(Instant::now());
                thread::sleep(left.min(Duration::from_millis(50)));
            }
            let _ = run_exec(&exec, &log, action);
        });
        match spawned {
            Ok(_) => {
                *guard = Some((id, deadline, action));
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

fn run_exec(exec: &ExecFn, log: &LogFn, action: PowerAction) -> Result<(), String> {
    match exec(action) {
        Ok(()) => {
            if action == PowerAction::Shutdown {
                log(Level::Info, "API: команда выключения выполнена, ПК выключается");
            } else if matches!(action, PowerAction::Sleep | PowerAction::Hibernate) {
                // Итог сна пишет сам поток в platform (после пробуждения).
                log(Level::Info, &format!("API: действие питания «{}» запущено", action.name()));
            } else {
                log(Level::Info, &format!("API: действие питания «{}» выполнено", action.name()));
            }
            Ok(())
        }
        Err(e) => {
            log(Level::Error, &format!("API: не удалось выполнить действие питания «{}» — {e}", action.name()));
            Err(e)
        }
    }
}

pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// Значение заголовка Authorization.
    pub auth: Option<&'a str>,
    pub body: &'a [u8],
}

#[derive(Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

fn resp(status: u16, v: Value) -> Response {
    Response { status, body: v.to_string() }
}

fn err(status: u16, msg: &str) -> Response {
    resp(status, json!({ "error": msg }))
}

/// Сравнение за постоянное время (по длине входов, без раннего выхода).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= (*a.get(i).unwrap_or(&0) ^ *b.get(i).unwrap_or(&0)) as usize;
    }
    diff == 0
}

fn authorized(auth: Option<&str>, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let Some(h) = auth else { return false };
    let h = h.trim();
    let given = match h.split_once(' ') {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest.trim(),
        _ => return false,
    };
    ct_eq(given.as_bytes(), token.as_bytes())
}

pub fn handle(req: &Request, token: &str, backend: &dyn Backend) -> Response {
    if !authorized(req.auth, token) {
        return err(401, "unauthorized");
    }
    let path = req.path.split('?').next().unwrap_or("");
    match (req.method, path) {
        ("GET", "/api/status") => {
            let pending = backend.pending_power();
            let vol = backend.volume();
            resp(
                200,
                json!({
                    "active": backend.active(),
                    "busy": backend.busy(),
                    "presets": backend.presets(),
                    "version": env!("CARGO_PKG_VERSION"),
                    "shutdown_pending": pending.is_some(),
                    "pending_power": pending.map(PowerAction::name),
                    "volume": vol.map(|(v, _)| v),
                    "muted": vol.map(|(_, m)| m),
                }),
            )
        }
        ("POST", "/api/switch") => {
            let Ok(v) = serde_json::from_slice::<Value>(req.body) else { return err(400, "bad request") };
            let Some(name) = v.get("preset").and_then(Value::as_str) else { return err(400, "bad request") };
            if !backend.presets().iter().any(|p| p == name) {
                return err(404, "unknown preset");
            }
            if backend.cancel_shutdown() {
                backend.log(Level::Info, "API: отложенное действие питания отменено (пришла команда переключения)");
            }
            backend.log(Level::Info, &format!("API: переключение на «{name}»"));
            backend.switch(name);
            resp(202, json!({ "accepted": true, "preset": name }))
        }
        ("POST", "/api/shutdown") => {
            let Some(delay) = parse_shutdown_delay(req.body) else { return err(400, "bad request") };
            if !backend.power_supported(PowerAction::Shutdown) {
                return err(501, "not supported");
            }
            backend.log(Level::Info, &format!("API: запрошено выключение ПК через {delay} с"));
            match backend.power(PowerAction::Shutdown, delay) {
                Ok(()) => resp(202, json!({ "accepted": true, "delay_secs": delay })),
                Err(e) if e == UNSUPPORTED => err(501, "not supported"),
                Err(e) => {
                    backend.log(Level::Error, &format!("API: выключение не выполнено — {e}"));
                    err(500, "shutdown failed")
                }
            }
        }
        ("POST", "/api/power") => {
            let Some((action, delay)) = parse_power(req.body) else { return err(400, "bad request") };
            if !backend.power_supported(action) {
                return err(501, "not supported");
            }
            backend.log(Level::Info, &format!("API: запрошено действие питания «{}» через {delay} с", action.name()));
            match backend.power(action, delay) {
                Ok(()) => resp(202, json!({ "accepted": true, "action": action.name(), "delay_secs": delay })),
                Err(e) if e == UNSUPPORTED => err(501, "not supported"),
                Err(e) => {
                    backend.log(Level::Error, &format!("API: действие питания «{}» не выполнено — {e}", action.name()));
                    err(500, "power failed")
                }
            }
        }
        ("POST", "/api/shutdown/cancel") => {
            let cancelled = backend.cancel_shutdown();
            if cancelled {
                backend.log(Level::Info, "API: отложенное действие питания отменено");
            }
            resp(200, json!({ "cancelled": cancelled }))
        }
        ("POST", "/api/volume") => {
            let Some(change) = parse_volume(req.body) else { return err(400, "bad request") };
            match backend.set_volume(change) {
                Ok((volume, muted)) => resp(200, json!({ "volume": volume, "muted": muted })),
                Err(e) if e == UNSUPPORTED => err(501, "not supported"),
                Err(e) => {
                    backend.log(Level::Error, &format!("API: громкость не изменена — {e}"));
                    err(500, "volume failed")
                }
            }
        }
        ("POST", "/api/media") => {
            let Some(key) = parse_media(req.body) else { return err(400, "bad request") };
            match backend.media(key) {
                Ok(()) => resp(200, json!({ "ok": true })),
                Err(e) if e == UNSUPPORTED => err(501, "not supported"),
                Err(e) => {
                    backend.log(Level::Error, &format!("API: медиаклавиша не отправлена — {e}"));
                    err(500, "media failed")
                }
            }
        }
        ("POST", "/api/notify") => {
            let Some((title, message)) = parse_notify(req.body) else { return err(400, "bad request") };
            match backend.notify(&title, &message) {
                Ok(()) => resp(202, json!({ "accepted": true })),
                Err(e) if e == NOTIFY_BUSY => err(503, "busy"),
                Err(e) => {
                    backend.log(Level::Error, &format!("API: уведомление не показано — {e}"));
                    err(500, "notify failed")
                }
            }
        }
        _ => err(404, "not found"),
    }
}

/// Необязательное `delay_secs` 0..=600 из JSON-объекта; `default` — если поля нет.
fn parse_delay(obj: &serde_json::Map<String, Value>, default: u64) -> Option<u64> {
    match obj.get("delay_secs") {
        None => Some(default),
        Some(d) => d.as_u64().filter(|n| *n <= MAX_POWER_DELAY),
    }
}

fn parse_object(body: &[u8]) -> Option<serde_json::Map<String, Value>> {
    match serde_json::from_slice::<Value>(body).ok()? {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

/// Строгий разбор тела `/api/shutdown`: пустое тело или JSON-объект с необязательным
/// целым `delay_secs` 0..=600. Всё остальное — `None`.
fn parse_shutdown_delay(body: &[u8]) -> Option<u64> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Some(DEFAULT_POWER_DELAY);
    }
    parse_delay(&parse_object(body)?, DEFAULT_POWER_DELAY)
}

/// Тело `/api/power`: объект с обязательным `action` (sleep|hibernate|lock|restart)
/// и необязательным `delay_secs` 0..=600 (по умолчанию 0 для lock, иначе 10).
fn parse_power(body: &[u8]) -> Option<(PowerAction, u64)> {
    let obj = parse_object(body)?;
    let action = PowerAction::parse(obj.get("action")?.as_str()?)?;
    Some((action, parse_delay(&obj, action.default_delay())?))
}

/// Тело `/api/volume`: хотя бы одно из `level` 0..=100, `delta` -100..=100, `muted`;
/// `level` и `delta` вместе недопустимы.
fn parse_volume(body: &[u8]) -> Option<VolumeChange> {
    let obj = parse_object(body)?;
    let level = match obj.get("level") {
        None => None,
        Some(v) => Some(u8::try_from(v.as_u64().filter(|n| *n <= 100)?).ok()?),
    };
    let delta = match obj.get("delta") {
        None => None,
        Some(v) => Some(i32::try_from(v.as_i64().filter(|n| (-100..=100).contains(n))?).ok()?),
    };
    let muted = match obj.get("muted") {
        None => None,
        Some(v) => Some(v.as_bool()?),
    };
    if level.is_some() && delta.is_some() {
        return None;
    }
    if level.is_none() && delta.is_none() && muted.is_none() {
        return None;
    }
    Some(VolumeChange { level, delta, muted })
}

/// Тело `/api/media`: объект с `action` из play_pause|next|prev|stop.
fn parse_media(body: &[u8]) -> Option<MediaKey> {
    MediaKey::parse(parse_object(body)?.get("action")?.as_str()?)
}

/// Допустимый в XML 1.0 символ (суррогатов в `char` не бывает).
pub fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || ((c as u32) >= 0x20 && c != '\u{FFFE}' && c != '\u{FFFF}')
}

/// Тело `/api/notify`: `title` 1–100 и `message` 1–500 символов (не пустые после trim).
fn parse_notify(body: &[u8]) -> Option<(String, String)> {
    let obj = parse_object(body)?;
    let field = |key: &str, max: usize| -> Option<String> {
        let s = obj.get(key)?.as_str()?;
        let n = s.chars().count();
        // Непустота проверяется после удаления символов, недопустимых в XML 1.0.
        let visible: String = s.chars().filter(|c| is_xml_char(*c)).collect();
        (n >= 1 && n <= max && !visible.trim().is_empty()).then(|| s.to_string())
    };
    Some((field("title", MAX_TITLE_CHARS)?, field("message", MAX_MESSAGE_CHARS)?))
}

// ---------------------------------------------------------------------------
// Сетевой слой
// ---------------------------------------------------------------------------

pub struct Running {
    server: Arc<tiny_http::Server>,
    thread: Option<JoinHandle<()>>,
    pub addr: SocketAddr,
}

impl Running {
    /// Не блокирует UI: будит accept-поток и ждёт его выхода не дольше `STOP_WAIT`
    /// (после выхода listener освобождён). Запросы в работе не ждём — они в своих потоках.
    pub fn stop(mut self) {
        self.server.unblock();
        let Some(t) = self.thread.take() else { return };
        let (tx, rx) = mpsc::channel();
        let _ = thread::Builder::new().name("preset-api-join".into()).spawn(move || {
            let _ = t.join();
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(STOP_WAIT);
    }
}

/// Счётчик активных запросов: освобождается при drop.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn json_header() -> tiny_http::Header {
    tiny_http::Header::from_bytes("Content-Type", "application/json; charset=utf-8").unwrap()
}

/// Поднимает сервер на `bind` (например `0.0.0.0:47800`). Токен читается при каждом запросе.
/// Каждый запрос обрабатывается в отдельном потоке (не более `MAX_CONCURRENT`, сверх — 503).
pub fn start(bind: &str, token: Arc<Mutex<String>>, backend: Arc<dyn Backend>) -> Result<Running, String> {
    // Старый listener освобождается асинхронно (его держит внутренний поток tiny_http),
    // поэтому при перезапуске на том же порту коротко повторяем попытку.
    let mut attempts = 0;
    let server = loop {
        match tiny_http::Server::http(bind) {
            Ok(s) => break Arc::new(s),
            Err(e) => {
                let in_use = e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::AddrInUse);
                if !in_use || attempts >= 20 {
                    return Err(e.to_string());
                }
                attempts += 1;
                thread::sleep(Duration::from_millis(25));
            }
        }
    };
    let addr = server.server_addr().to_ip().ok_or("не удалось определить адрес сервера")?;
    let srv = server.clone();
    let active = Arc::new(AtomicUsize::new(0));
    let thread = thread::Builder::new()
        .name("preset-api".into())
        .spawn(move || {
            for request in srv.incoming_requests() {
                if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT {
                    active.fetch_sub(1, Ordering::SeqCst);
                    let _ = request.respond(
                        tiny_http::Response::from_string(json!({ "error": "busy" }).to_string())
                            .with_status_code(503)
                            .with_header(json_header()),
                    );
                    continue;
                }
                let slot = Slot(active.clone());
                let (token, backend) = (token.clone(), backend.clone());
                // Если поток не создался, запрос (и соединение) просто закрывается.
                let _ = thread::Builder::new().name("preset-api-req".into()).spawn(move || {
                    let _slot = slot;
                    let mut request = request;
                    let response = serve_one(&mut request, &token, backend.as_ref());
                    let _ = request.respond(
                        tiny_http::Response::from_string(response.body)
                            .with_status_code(response.status)
                            .with_header(json_header()),
                    );
                });
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Running { server, thread: Some(thread), addr })
}

/// Обрабатывает запрос и пишет в журнал (debug) метод, путь без query и код ответа.
/// Токен и тело запроса в журнал не попадают.
fn serve_one(request: &mut tiny_http::Request, token: &Mutex<String>, backend: &dyn Backend) -> Response {
    let method = request.method().as_str().to_string();
    let path = request.url().to_string();
    let response = serve_inner(request, &method, &path, token, backend);
    let log_path = path.split('?').next().unwrap_or("");
    backend.log(Level::Debug, &format!("API: {method} {log_path} -> {}", response.status));
    response
}

fn serve_inner(request: &mut tiny_http::Request, method: &str, path: &str, token: &Mutex<String>, backend: &dyn Backend) -> Response {
    let auth = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str().to_string());
    let token = token.lock().unwrap().clone();

    // Сначала авторизация: без неё тело даже не читаем.
    if !authorized(auth.as_deref(), &token) {
        return err(401, "unauthorized");
    }
    if request.body_length().is_some_and(|n| n > MAX_BODY) {
        return err(400, "bad request");
    }
    let mut body = Vec::new();
    if request.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return err(400, "bad request");
    }
    // Обрыв тела (меньше заявленного Content-Length) — не обрабатываем: иначе усечённый JSON
    // мог бы дать валидную, но неполную команду.
    if request.body_length().is_some_and(|n| n != body.len()) {
        return err(400, "bad request");
    }
    handle(&Request { method, path, auth: auth.as_deref(), body: &body }, &token, backend)
}

// ---------------------------------------------------------------------------
// Управление жизненным циклом (применение настроек «вживую»)
// ---------------------------------------------------------------------------

pub struct ApiManager {
    backend: Arc<dyn Backend>,
    token: Arc<Mutex<String>>,
    running: Option<Running>,
    applied: Option<(bool, u16)>,
    /// Последняя ошибка запуска (для UI).
    pub error: Option<String>,
}

impl ApiManager {
    pub fn new(engine: Engine) -> Self {
        Self { backend: Arc::new(engine), token: Arc::new(Mutex::new(String::new())), running: None, applied: None, error: None }
    }

    /// Применяет настройки: токен — сразу, включение/порт — перезапуском сервера.
    pub fn apply(&mut self, s: &Settings) {
        *self.token.lock().unwrap() = s.api_token.clone();
        let want = (s.api_enabled, s.api_port);
        if self.applied == Some(want) {
            return;
        }
        self.applied = Some(want);
        if let Some(r) = self.running.take() {
            r.stop();
            self.backend.log(Level::Info, "API: остановлен");
        }
        self.error = None;
        if !s.api_enabled {
            return;
        }
        match start(&format!("0.0.0.0:{}", s.api_port), self.token.clone(), self.backend.clone()) {
            Ok(r) => {
                self.backend.log(Level::Info, &format!("API: слушаю порт {}", r.addr.port()));
                self.running = Some(r);
            }
            Err(e) => {
                let msg = format!("API: не удалось занять порт {} — {e}", s.api_port);
                self.backend.log(Level::Error, &msg);
                self.error = Some(msg);
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }
}

/// Локальный IP в сети (адрес исходящего интерфейса; пакеты не отправляются).
pub fn local_ip() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}


/// Чистая логика `/api/volume`: итоговые (громкость, mute) по текущим и запросу.
/// `level`/`delta` ограничиваются диапазоном 0–100.
pub fn apply_volume(cur: (u8, bool), c: VolumeChange) -> (u8, bool) {
    let mut vol = i32::from(cur.0);
    if let Some(l) = c.level {
        vol = i32::from(l);
    } else if let Some(d) = c.delta {
        vol += d;
    }
    (vol.clamp(0, 100) as u8, c.muted.unwrap_or(cur.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpStream;
    use std::sync::atomic::AtomicBool;

    #[derive(Default)]
    struct Fake {
        switched: Mutex<Vec<String>>,
        powers: Mutex<Vec<(PowerAction, u64)>>,
        pending: Mutex<Option<PowerAction>>,
        fail_shutdown: AtomicBool,
        /// Текущая громкость; `None` — «ошибка Core Audio».
        vol: Mutex<Option<(u8, bool)>>,
        volume_calls: Mutex<Vec<VolumeChange>>,
        media_calls: Mutex<Vec<MediaKey>>,
        notifications: Mutex<Vec<(String, String)>>,
        /// Имитация macOS: нет hibernate и media stop.
        mac_like: AtomicBool,
        fail_misc: AtomicBool,
    }

    impl Fake {
        fn shutdown_pending(&self) -> bool {
            self.pending.lock().unwrap().is_some()
        }
        fn shutdown_delays(&self) -> Vec<u64> {
            self.powers.lock().unwrap().iter().filter(|(a, _)| *a == PowerAction::Shutdown).map(|(_, d)| *d).collect()
        }
    }

    impl Backend for Fake {
        fn active(&self) -> Option<String> {
            Some("Игры".into())
        }
        fn busy(&self) -> bool {
            false
        }
        fn presets(&self) -> Vec<String> {
            vec!["Работа".into(), "Игры".into()]
        }
        fn switch(&self, p: &str) {
            self.switched.lock().unwrap().push(p.into());
        }
        fn power(&self, a: PowerAction, d: u64) -> Result<(), String> {
            self.powers.lock().unwrap().push((a, d));
            if self.fail_shutdown.load(Ordering::SeqCst) {
                return Err("boom".into());
            }
            *self.pending.lock().unwrap() = (d > 0).then_some(a);
            Ok(())
        }
        fn power_supported(&self, a: PowerAction) -> bool {
            !(self.mac_like.load(Ordering::SeqCst) && a == PowerAction::Hibernate)
        }
        fn cancel_shutdown(&self) -> bool {
            self.pending.lock().unwrap().take().is_some()
        }
        fn pending_power(&self) -> Option<PowerAction> {
            *self.pending.lock().unwrap()
        }
        fn volume(&self) -> Option<(u8, bool)> {
            *self.vol.lock().unwrap()
        }
        fn set_volume(&self, c: VolumeChange) -> Result<(u8, bool), String> {
            self.volume_calls.lock().unwrap().push(c);
            let mut v = self.vol.lock().unwrap();
            let cur = v.ok_or("no audio")?;
            let new = apply_volume(cur, c);
            *v = Some(new);
            Ok(new)
        }
        fn media(&self, k: MediaKey) -> Result<(), String> {
            self.media_calls.lock().unwrap().push(k);
            if self.mac_like.load(Ordering::SeqCst) && k == MediaKey::Stop {
                return Err(UNSUPPORTED.into());
            }
            if self.fail_misc.load(Ordering::SeqCst) { Err("boom".into()) } else { Ok(()) }
        }
        fn notify(&self, t: &str, m: &str) -> Result<(), String> {
            self.notifications.lock().unwrap().push((t.into(), m.into()));
            if self.fail_misc.load(Ordering::SeqCst) { Err("boom".into()) } else { Ok(()) }
        }
        fn log(&self, _: Level, _: &str) {}
    }

    fn call(f: &Fake, method: &str, path: &str, auth: Option<&str>, body: &str) -> (u16, Value) {
        let r = handle(&Request { method, path, auth, body: body.as_bytes() }, "secret", f);
        (r.status, serde_json::from_str(&r.body).unwrap())
    }

    const OK: Option<&str> = Some("Bearer secret");

    #[test]
    fn auth_required() {
        let f = Fake::default();
        for auth in [None, Some("Bearer wrong"), Some("secret"), Some("Basic secret"), Some("Bearer ")] {
            let (s, b) = call(&f, "GET", "/api/status", auth, "");
            assert_eq!((s, b), (401, json!({"error": "unauthorized"})));
        }
        let (s, _) = call(&f, "POST", "/api/shutdown/cancel", None, "");
        assert_eq!(s, 401);
        // пустой токен в настройках никогда не авторизует
        let r = handle(&Request { method: "GET", path: "/api/status", auth: Some("Bearer "), body: b"" }, "", &f);
        assert_eq!(r.status, 401);
    }

    #[test]
    fn status() {
        let f = Fake::default();
        let (s, b) = call(&f, "GET", "/api/status", OK, "");
        assert_eq!(s, 200);
        assert_eq!(
            b,
            json!({"active": "Игры", "busy": false, "presets": ["Работа", "Игры"], "version": env!("CARGO_PKG_VERSION"), "shutdown_pending": false, "pending_power": null, "volume": null, "muted": null})
        );
        call(&f, "POST", "/api/shutdown", OK, "");
        let (_, b) = call(&f, "GET", "/api/status", OK, "");
        assert_eq!(b["shutdown_pending"], true);
    }

    #[test]
    fn switch_cases() {
        let f = Fake::default();
        let (s, b) = call(&f, "POST", "/api/switch", OK, r#"{"preset":"Игры"}"#);
        assert_eq!((s, b), (202, json!({"accepted": true, "preset": "Игры"})));
        assert_eq!(*f.switched.lock().unwrap(), vec!["Игры"]);
        let (s, b) = call(&f, "POST", "/api/switch", OK, r#"{"preset":"Нет"}"#);
        assert_eq!((s, b), (404, json!({"error": "unknown preset"})));
        for body in ["", "{", "{}", r#"{"preset":5}"#] {
            let (s, b) = call(&f, "POST", "/api/switch", OK, body);
            assert_eq!((s, b), (400, json!({"error": "bad request"})), "{body}");
        }
        assert_eq!(f.switched.lock().unwrap().len(), 1);
    }

    #[test]
    fn shutdown_accepts_valid_bodies() {
        let f = Fake::default();
        let (s, b) = call(&f, "POST", "/api/shutdown", OK, "");
        assert_eq!((s, b), (202, json!({"accepted": true, "delay_secs": 10})));
        let (s, b) = call(&f, "POST", "/api/shutdown", OK, "{}");
        assert_eq!((s, b), (202, json!({"accepted": true, "delay_secs": 10})));
        let (_, b) = call(&f, "POST", "/api/shutdown", OK, r#"{"delay_secs":0}"#);
        assert_eq!(b["delay_secs"], 0);
        let (_, b) = call(&f, "POST", "/api/shutdown", OK, r#"{"delay_secs":600}"#);
        assert_eq!(b["delay_secs"], 600);
        assert_eq!(f.shutdown_delays(), vec![10, 10, 0, 600]);
    }

    #[test]
    fn shutdown_rejects_bad_bodies_before_backend() {
        let f = Fake::default();
        for body in [
            "[]", "false", "null", "\"10\"", "10", "garbage", r#"{"delay_secs":700}"#, r#"{"delay_secs":601}"#,
            r#"{"delay_secs":-1}"#, r#"{"delay_secs":1.5}"#, r#"{"delay_secs":5.0}"#, r#"{"delay_secs":"5"}"#,
            r#"{"delay_secs":null}"#, r#"{"delay_secs":99999}"#,
        ] {
            let (s, b) = call(&f, "POST", "/api/shutdown", OK, body);
            assert_eq!((s, b), (400, json!({"error": "bad request"})), "{body}");
        }
        assert!(f.powers.lock().unwrap().is_empty());
    }

    #[test]
    fn shutdown_failure_is_500() {
        let f = Fake::default();
        f.fail_shutdown.store(true, Ordering::SeqCst);
        let (s, b) = call(&f, "POST", "/api/shutdown", OK, r#"{"delay_secs":0}"#);
        assert_eq!((s, b), (500, json!({"error": "shutdown failed"})));
    }

    #[test]
    fn cancel_endpoint() {
        let f = Fake::default();
        let (s, b) = call(&f, "POST", "/api/shutdown/cancel", OK, "");
        assert_eq!((s, b), (200, json!({"cancelled": false})));
        call(&f, "POST", "/api/shutdown", OK, r#"{"delay_secs":30}"#);
        assert!(f.shutdown_pending());
        let (s, b) = call(&f, "POST", "/api/shutdown/cancel", OK, "");
        assert_eq!((s, b), (200, json!({"cancelled": true})));
        assert!(!f.shutdown_pending());
        let (_, b) = call(&f, "POST", "/api/shutdown/cancel", OK, "");
        assert_eq!(b, json!({"cancelled": false}));
    }

    #[test]
    fn switch_cancels_pending_shutdown() {
        let f = Fake::default();
        call(&f, "POST", "/api/shutdown", OK, r#"{"delay_secs":30}"#);
        assert!(f.shutdown_pending());
        // неизвестный пресет выключение не отменяет
        call(&f, "POST", "/api/switch", OK, r#"{"preset":"Нет"}"#);
        assert!(f.shutdown_pending());
        let (s, _) = call(&f, "POST", "/api/switch", OK, r#"{"preset":"Работа"}"#);
        assert_eq!(s, 202);
        assert!(!f.shutdown_pending());
    }

    #[test]
    fn not_found() {
        let f = Fake::default();
        for (m, p) in [("GET", "/"), ("POST", "/api/status"), ("GET", "/api/switch"), ("DELETE", "/api/shutdown"), ("GET", "/api/shutdown/cancel")] {
            let (s, b) = call(&f, m, p, OK, "");
            assert_eq!((s, b), (404, json!({"error": "not found"})), "{m} {p}");
        }
    }

    // --- ShutdownTimer -----------------------------------------------------

    fn timer() -> (Arc<ShutdownTimer>, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let logs = Arc::new(Mutex::new(Vec::new()));
        let (r, l) = (runs.clone(), logs.clone());
        let t = ShutdownTimer::new(
            Arc::new(move |_| {
                r.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            Arc::new(move |_: Level, m: &str| l.lock().unwrap().push(m.to_string())),
        );
        (Arc::new(t), runs, logs)
    }

    #[test]
    fn timer_fires_after_deadline() {
        let (t, runs, logs) = timer();
        t.schedule_after(PowerAction::Shutdown, Duration::from_millis(150)).unwrap();
        assert!(t.pending_action().is_some());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        thread::sleep(Duration::from_millis(600));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(t.pending_action().is_none());
        assert!(!t.cancel());
        assert!(logs.lock().unwrap().iter().any(|m| m.contains("команда выключения")));
    }

    #[test]
    fn timer_cancel_prevents_run() {
        let (t, runs, _) = timer();
        t.schedule_after(PowerAction::Shutdown, Duration::from_millis(200)).unwrap();
        assert!(t.cancel());
        assert!(t.pending_action().is_none());
        thread::sleep(Duration::from_millis(500));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn timer_reschedule_replaces_previous() {
        let (t, runs, _) = timer();
        t.schedule_after(PowerAction::Shutdown, Duration::from_millis(150)).unwrap();
        t.schedule_after(PowerAction::Shutdown, Duration::from_millis(300)).unwrap();
        thread::sleep(Duration::from_millis(700));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn timer_zero_delay_is_synchronous_and_reports_error() {
        let (t, runs, _) = timer();
        t.schedule_after(PowerAction::Shutdown, Duration::from_millis(10_000)).unwrap();
        t.schedule_after(PowerAction::Shutdown, Duration::ZERO).unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(t.pending_action().is_none()); // отложенное заменено немедленным

        let failing = ShutdownTimer::new(Arc::new(|_| Err("denied".to_string())), Arc::new(|_: Level, _: &str| {}));
        assert_eq!(failing.schedule_after(PowerAction::Shutdown, Duration::ZERO), Err("denied".to_string()));
    }

    // --- Питание / громкость / медиа / уведомления ---------------------------

    fn bad(f: &Fake, path: &str, body: &str) {
        let (s, b) = call(f, "POST", path, OK, body);
        assert_eq!((s, b), (400, json!({"error": "bad request"})), "{path} {body}");
    }

    #[test]
    fn power_parsing_and_defaults() {
        let f = Fake::default();
        let (s, b) = call(&f, "POST", "/api/power", OK, r#"{"action":"lock"}"#);
        assert_eq!((s, b), (202, json!({"accepted": true, "action": "lock", "delay_secs": 0})));
        for (a, d) in [("sleep", 10), ("hibernate", 10), ("restart", 10)] {
            let (s, b) = call(&f, "POST", "/api/power", OK, &format!(r#"{{"action":"{a}"}}"#));
            assert_eq!((s, b), (202, json!({"accepted": true, "action": a, "delay_secs": d})));
        }
        let (_, b) = call(&f, "POST", "/api/power", OK, r#"{"action":"sleep","delay_secs":600}"#);
        assert_eq!(b["delay_secs"], 600);
        let (_, b) = call(&f, "POST", "/api/power", OK, r#"{"action":"restart","delay_secs":0,"extra":1}"#);
        assert_eq!(b["delay_secs"], 0);
        let (_, b) = call(&f, "POST", "/api/power", OK, r#"{"action":"lock","delay_secs":5}"#);
        assert_eq!(b["delay_secs"], 5);
        assert_eq!(
            *f.powers.lock().unwrap(),
            vec![
                (PowerAction::Lock, 0),
                (PowerAction::Sleep, 10),
                (PowerAction::Hibernate, 10),
                (PowerAction::Restart, 10),
                (PowerAction::Sleep, 600),
                (PowerAction::Restart, 0),
                (PowerAction::Lock, 5),
            ]
        );
    }

    #[test]
    fn power_rejects_bad_bodies() {
        let f = Fake::default();
        for body in [
            "", "{}", "[]", "null", "\"sleep\"", "garbage", r#"{"action":"shutdown"}"#, r#"{"action":"SLEEP"}"#,
            r#"{"action":""}"#, r#"{"action":5}"#, r#"{"action":null}"#, r#"{"action":["sleep"]}"#,
            r#"{"action":"sleep","delay_secs":601}"#, r#"{"action":"sleep","delay_secs":-1}"#,
            r#"{"action":"sleep","delay_secs":1.5}"#, r#"{"action":"sleep","delay_secs":"5"}"#,
            r#"{"action":"sleep","delay_secs":null}"#, r#"{"delay_secs":5}"#,
        ] {
            bad(&f, "/api/power", body);
        }
        assert!(f.powers.lock().unwrap().is_empty());
    }

    #[test]
    fn power_failure_is_500() {
        let f = Fake::default();
        f.fail_shutdown.store(true, Ordering::SeqCst);
        let (s, b) = call(&f, "POST", "/api/power", OK, r#"{"action":"lock"}"#);
        assert_eq!((s, b), (500, json!({"error": "power failed"})));
    }

    #[test]
    fn pending_power_in_status_replace_and_cancel() {
        let f = Fake::default();
        call(&f, "POST", "/api/power", OK, r#"{"action":"sleep","delay_secs":60}"#);
        let (_, b) = call(&f, "GET", "/api/status", OK, "");
        assert_eq!((b["shutdown_pending"].clone(), b["pending_power"].clone()), (json!(true), json!("sleep")));
        // новое действие заменяет ожидающее
        call(&f, "POST", "/api/shutdown", OK, r#"{"delay_secs":60}"#);
        let (_, b) = call(&f, "GET", "/api/status", OK, "");
        assert_eq!(b["pending_power"], "shutdown");
        call(&f, "POST", "/api/power", OK, r#"{"action":"restart","delay_secs":60}"#);
        let (_, b) = call(&f, "GET", "/api/status", OK, "");
        assert_eq!(b["pending_power"], "restart");
        // cancel отменяет любое
        let (s, b) = call(&f, "POST", "/api/shutdown/cancel", OK, "");
        assert_eq!((s, b), (200, json!({"cancelled": true})));
        let (_, b) = call(&f, "GET", "/api/status", OK, "");
        assert_eq!((b["shutdown_pending"].clone(), b["pending_power"].clone()), (json!(false), Value::Null));
        // успешный switch отменяет любое действие
        call(&f, "POST", "/api/power", OK, r#"{"action":"hibernate","delay_secs":60}"#);
        assert!(f.shutdown_pending());
        call(&f, "POST", "/api/switch", OK, r#"{"preset":"Работа"}"#);
        assert!(!f.shutdown_pending());
    }

    #[test]
    fn status_volume_fields() {
        let f = Fake::default();
        *f.vol.lock().unwrap() = Some((42, true));
        let (_, b) = call(&f, "GET", "/api/status", OK, "");
        assert_eq!((b["volume"].clone(), b["muted"].clone()), (json!(42), json!(true)));
    }

    #[test]
    fn volume_parsing_and_results() {
        let f = Fake::default();
        *f.vol.lock().unwrap() = Some((50, false));
        let (s, b) = call(&f, "POST", "/api/volume", OK, r#"{"level":30}"#);
        assert_eq!((s, b), (200, json!({"volume": 30, "muted": false})));
        let (_, b) = call(&f, "POST", "/api/volume", OK, r#"{"delta":-100}"#);
        assert_eq!(b, json!({"volume": 0, "muted": false}));
        let (_, b) = call(&f, "POST", "/api/volume", OK, r#"{"delta":100,"muted":true}"#);
        assert_eq!(b, json!({"volume": 100, "muted": true}));
        let (_, b) = call(&f, "POST", "/api/volume", OK, r#"{"muted":false}"#);
        assert_eq!(b, json!({"volume": 100, "muted": false}));
        let (_, b) = call(&f, "POST", "/api/volume", OK, r#"{"level":0,"muted":true,"x":1}"#);
        assert_eq!(b, json!({"volume": 0, "muted": true}));
        let (_, b) = call(&f, "POST", "/api/volume", OK, r#"{"level":100}"#);
        assert_eq!(b["volume"], 100);
        let (_, b) = call(&f, "POST", "/api/volume", OK, r#"{"delta":0}"#);
        assert_eq!(b["volume"], 100);
        assert_eq!(f.volume_calls.lock().unwrap().len(), 7);
    }

    #[test]
    fn volume_rejects_bad_bodies() {
        let f = Fake::default();
        *f.vol.lock().unwrap() = Some((50, false));
        for body in [
            "", "{}", "[]", "null", "5", "garbage", r#"{"level":101}"#, r#"{"level":-1}"#, r#"{"level":1.5}"#,
            r#"{"level":"5"}"#, r#"{"level":null}"#, r#"{"delta":101}"#, r#"{"delta":-101}"#, r#"{"delta":0.5}"#,
            r#"{"delta":"1"}"#, r#"{"muted":1}"#, r#"{"muted":"true"}"#, r#"{"muted":null}"#,
            r#"{"level":10,"delta":5}"#, r#"{"level":10,"delta":0}"#, r#"{"other":1}"#,
        ] {
            bad(&f, "/api/volume", body);
        }
        assert!(f.volume_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn volume_failure_is_500() {
        let f = Fake::default(); // громкость недоступна
        let (s, b) = call(&f, "POST", "/api/volume", OK, r#"{"level":10}"#);
        assert_eq!((s, b), (500, json!({"error": "volume failed"})));
    }

    #[test]
    fn apply_volume_math() {
        let c = |level, delta, muted| VolumeChange { level, delta, muted };
        assert_eq!(apply_volume((50, false), c(Some(70), None, None)), (70, false));
        assert_eq!(apply_volume((50, true), c(None, Some(-20), None)), (30, true));
        assert_eq!(apply_volume((95, false), c(None, Some(100), Some(true))), (100, true));
        assert_eq!(apply_volume((5, false), c(None, Some(-100), None)), (0, false));
    }

    #[test]
    fn unsupported_actions_are_501() {
        let f = Fake::default();
        f.mac_like.store(true, Ordering::SeqCst);
        let (s, b) = call(&f, "POST", "/api/power", OK, r#"{"action":"hibernate"}"#);
        assert_eq!((s, b), (501, json!({"error": "not supported"})));
        assert!(f.powers.lock().unwrap().is_empty());
        let (s, _) = call(&f, "POST", "/api/media", OK, r#"{"action":"stop"}"#);
        assert_eq!(s, 501);
        // остальное работает
        let (s, _) = call(&f, "POST", "/api/media", OK, r#"{"action":"next"}"#);
        assert_eq!(s, 200);
        let (s, _) = call(&f, "POST", "/api/power", OK, r#"{"action":"sleep"}"#);
        assert_eq!(s, 202);
    }

    #[test]
    fn media_cases() {
        let f = Fake::default();
        for (a, k) in [
            ("play_pause", MediaKey::PlayPause),
            ("next", MediaKey::Next),
            ("prev", MediaKey::Prev),
            ("stop", MediaKey::Stop),
        ] {
            let (s, b) = call(&f, "POST", "/api/media", OK, &format!(r#"{{"action":"{a}"}}"#));
            assert_eq!((s, b), (200, json!({"ok": true})));
            assert_eq!(f.media_calls.lock().unwrap().last(), Some(&k));
        }
        for body in ["", "{}", "[]", "null", "garbage", r#"{"action":"pause"}"#, r#"{"action":5}"#, r#"{"action":null}"#, r#"{"key":"next"}"#] {
            bad(&f, "/api/media", body);
        }
        assert_eq!(f.media_calls.lock().unwrap().len(), 4);
        f.fail_misc.store(true, Ordering::SeqCst);
        let (s, b) = call(&f, "POST", "/api/media", OK, r#"{"action":"next"}"#);
        assert_eq!((s, b), (500, json!({"error": "media failed"})));
    }

    #[test]
    fn notify_cases() {
        let f = Fake::default();
        let (s, b) = call(&f, "POST", "/api/notify", OK, r#"{"title":"Привет","message":"Мир <&>"}"#);
        assert_eq!((s, b), (202, json!({"accepted": true})));
        assert_eq!(*f.notifications.lock().unwrap(), vec![("Привет".to_string(), "Мир <&>".to_string())]);
        // границы длины считаются в символах
        let t100 = "я".repeat(100);
        let m500 = "ы".repeat(500);
        let ok = json!({"title": t100, "message": m500}).to_string();
        assert_eq!(call(&f, "POST", "/api/notify", OK, &ok).0, 202);
        let long_t = json!({"title": "я".repeat(101), "message": "x"}).to_string();
        let long_m = json!({"title": "x", "message": "ы".repeat(501)}).to_string();
        bad(&f, "/api/notify", &long_t);
        bad(&f, "/api/notify", &long_m);
        for body in [
            "", "{}", "[]", "null", "garbage", r#"{"title":"a"}"#, r#"{"message":"a"}"#, r#"{"title":"","message":"a"}"#,
            r#"{"title":"a","message":""}"#, r#"{"title":"  ","message":"a"}"#, r#"{"title":5,"message":"a"}"#,
            r#"{"title":"a","message":null}"#, r#"{"title":["a"],"message":"a"}"#,
            r#"{"title":"\u0000","message":"a"}"#, r#"{"title":"a","message":"\u0001 \u0000"}"#,
        ] {
            bad(&f, "/api/notify", body);
        }
        assert_eq!(f.notifications.lock().unwrap().len(), 2);
        f.fail_misc.store(true, Ordering::SeqCst);
        let (s, b) = call(&f, "POST", "/api/notify", OK, r#"{"title":"a","message":"b"}"#);
        assert_eq!((s, b), (500, json!({"error": "notify failed"})));
    }

    #[test]
    fn notify_busy_is_503() {
        struct Busy(Fake);
        impl Backend for Busy {
            fn active(&self) -> Option<String> { self.0.active() }
            fn busy(&self) -> bool { false }
            fn presets(&self) -> Vec<String> { self.0.presets() }
            fn switch(&self, p: &str) { self.0.switch(p) }
            fn power(&self, a: PowerAction, d: u64) -> Result<(), String> { self.0.power(a, d) }
            fn cancel_shutdown(&self) -> bool { false }
            fn pending_power(&self) -> Option<PowerAction> { None }
            fn volume(&self) -> Option<(u8, bool)> { None }
            fn set_volume(&self, c: VolumeChange) -> Result<(u8, bool), String> { self.0.set_volume(c) }
            fn media(&self, k: MediaKey) -> Result<(), String> { self.0.media(k) }
            fn notify(&self, _: &str, _: &str) -> Result<(), String> { Err(NOTIFY_BUSY.to_string()) }
            fn log(&self, _: Level, _: &str) {}
        }
        let b = Busy(Fake::default());
        let r = handle(&Request { method: "POST", path: "/api/notify", auth: OK, body: br#"{"title":"a","message":"b"}"# }, "secret", &b);
        assert_eq!((r.status, r.body.as_str()), (503, r#"{"error":"busy"}"#));
    }

    #[test]
    fn new_routes_require_auth_and_exact_method() {
        let f = Fake::default();
        for p in ["/api/power", "/api/volume", "/api/media", "/api/notify"] {
            let (s, b) = call(&f, "POST", p, None, "{}");
            assert_eq!((s, b), (401, json!({"error": "unauthorized"})), "{p}");
            let (s, b) = call(&f, "GET", p, OK, "");
            assert_eq!((s, b), (404, json!({"error": "not found"})), "GET {p}");
        }
        assert!(f.powers.lock().unwrap().is_empty());
        assert!(f.volume_calls.lock().unwrap().is_empty());
        assert!(f.media_calls.lock().unwrap().is_empty());
        assert!(f.notifications.lock().unwrap().is_empty());
    }

    #[test]
    fn timer_passes_action_and_replaces_pending() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let t = ShutdownTimer::new(
            Arc::new(move |a| {
                s2.lock().unwrap().push(a);
                Ok(())
            }),
            Arc::new(|_: Level, _: &str| {}),
        );
        t.schedule_after(PowerAction::Sleep, Duration::from_millis(150)).unwrap();
        assert_eq!(t.pending_action(), Some(PowerAction::Sleep));
        t.schedule_after(PowerAction::Restart, Duration::from_millis(250)).unwrap();
        assert_eq!(t.pending_action(), Some(PowerAction::Restart));
        thread::sleep(Duration::from_millis(700));
        assert_eq!(*seen.lock().unwrap(), vec![PowerAction::Restart]);
        // отмена любого действия
        t.schedule_after(PowerAction::Hibernate, Duration::from_millis(200)).unwrap();
        assert!(t.cancel());
        thread::sleep(Duration::from_millis(500));
        assert_eq!(seen.lock().unwrap().len(), 1);
        // нулевая задержка — синхронно
        t.schedule_after(PowerAction::Lock, Duration::ZERO).unwrap();
        assert_eq!(seen.lock().unwrap().last(), Some(&PowerAction::Lock));
    }

    // --- Сеть --------------------------------------------------------------

    fn http(addr: SocketAddr, raw: &str) -> (u16, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let status = out.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, out.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
    }

    const STATUS_REQ: &str = "GET /api/status HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer secret\r\nConnection: close\r\n\r\n";
    const HANG_REQ: &[u8] = b"POST /api/switch HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer secret\r\nContent-Length: 2000\r\n\r\nabc";

    #[test]
    fn network_roundtrip() {
        let fake = Arc::new(Fake::default());
        let token = Arc::new(Mutex::new("secret".to_string()));
        let running = start("127.0.0.1:0", token, fake.clone()).unwrap();
        let a = running.addr;

        let (s, b) = http(a, STATUS_REQ);
        assert_eq!(s, 200);
        assert!(b.contains("\"presets\":[\"Работа\",\"Игры\"]"), "{b}");
        assert!(b.contains("\"shutdown_pending\":false"), "{b}");

        let (s, b) = http(a, "GET /api/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert_eq!((s, b.as_str()), (401, r#"{"error":"unauthorized"}"#));

        let body = r#"{"preset":"Работа"}"#;
        let req = format!(
            "POST /api/switch HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (s, _) = http(a, &req);
        assert_eq!(s, 202);
        assert_eq!(*fake.switched.lock().unwrap(), vec!["Работа"]);

        let big = "x".repeat(MAX_BODY + 10);
        let req = format!(
            "POST /api/switch HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer secret\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{big}",
            big.len()
        );
        assert_eq!(http(a, &req).0, 400);

        running.stop();
    }

    #[test]
    fn hanging_request_does_not_block_status() {
        let fake = Arc::new(Fake::default());
        let running = start("127.0.0.1:0", Arc::new(Mutex::new("secret".into())), fake).unwrap();
        let a = running.addr;

        // Авторизованный запрос обещает 100 байт тела, но шлёт 3 и «виснет».
        let mut hung = TcpStream::connect(a).unwrap();
        hung.write_all(HANG_REQ).unwrap();
        thread::sleep(Duration::from_millis(200));

        let started = Instant::now();
        let (s, _) = http(a, STATUS_REQ);
        assert_eq!(s, 200);
        assert!(started.elapsed() < Duration::from_secs(2));

        drop(hung);
        running.stop();
    }

    #[test]
    fn requests_over_limit_get_503() {
        let fake = Arc::new(Fake::default());
        let running = start("127.0.0.1:0", Arc::new(Mutex::new("secret".into())), fake).unwrap();
        let a = running.addr;
        let hung: Vec<TcpStream> = (0..MAX_CONCURRENT)
            .map(|_| {
                let mut c = TcpStream::connect(a).unwrap();
                c.write_all(HANG_REQ).unwrap();
                c
            })
            .collect();
        // Ждём, пока сервер разберёт все зависшие запросы и займёт слоты: фиксированная пауза
        // нестабильна на медленных CI-раннерах. Пока слот свободен, STATUS_REQ отвечает 200 и слот освобождает.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut s = 0;
        while Instant::now() < deadline {
            s = http(a, STATUS_REQ).0;
            if s == 503 {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(s, 503);
        drop(hung);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && http(a, STATUS_REQ).0 != 200 {
            thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(http(a, STATUS_REQ).0, 200);
        running.stop();
    }

    #[test]
    fn truncated_body_is_not_executed() {
        let fake = Arc::new(Fake::default());
        let running = start("127.0.0.1:0", Arc::new(Mutex::new("secret".into())), fake.clone()).unwrap();
        let a = running.addr;
        // Content-Length 2000, а пришёл валидный префикс JSON и обрыв записи.
        let mut s = TcpStream::connect(a).unwrap();
        s.write_all(b"POST /api/shutdown HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer secret\r\nContent-Length: 2000\r\n\r\n{}").unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        assert!(out.is_empty() || out.contains(" 400 "), "{out}");
        assert!(fake.powers.lock().unwrap().is_empty());
        assert!(!fake.shutdown_pending());
        running.stop();
    }

    #[test]
    fn huge_content_length_does_not_abort_process() {
        let fake = Arc::new(Fake::default());
        let running = start("127.0.0.1:0", Arc::new(Mutex::new("secret".into())), fake).unwrap();
        let a = running.addr;
        for auth in ["", "Authorization: Bearer secret\r\n"] {
            let req = format!("POST /api/shutdown HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: 9000000000000\r\nConnection: close\r\n\r\n");
            let (s, _) = http(a, &req);
            assert!(s == 401 || s == 400, "{s}");
        }
        assert_eq!(http(a, STATUS_REQ).0, 200);
        running.stop();
    }

    #[test]
    fn stop_is_prompt_even_with_hanging_request() {
        let fake = Arc::new(Fake::default());
        let running = start("127.0.0.1:0", Arc::new(Mutex::new("secret".into())), fake).unwrap();
        let mut hung = TcpStream::connect(running.addr).unwrap();
        hung.write_all(HANG_REQ).unwrap();
        thread::sleep(Duration::from_millis(200));
        let t = Instant::now();
        running.stop();
        assert!(t.elapsed() < Duration::from_secs(2), "stop blocked");
        drop(hung);
    }

    #[test]
    fn port_can_be_reused_after_stop() {
        let fake = Arc::new(Fake::default());
        let token = Arc::new(Mutex::new("secret".to_string()));
        let running = start("127.0.0.1:0", token.clone(), fake.clone()).unwrap();
        let port = running.addr.port();
        assert_eq!(http(running.addr, STATUS_REQ).0, 200);
        running.stop();

        let again = start(&format!("127.0.0.1:{port}"), token, fake).expect("port must be free after stop");
        assert_eq!(http(again.addr, STATUS_REQ).0, 200);
        again.stop();
    }
}
