//! Реализация платформенного слоя для macOS: системные утилиты (`osascript`, `pmset`, `open`,
//! `ps`) и AppKit через `objc2`. Чистая логика (разбор вывода, экранирование, plist)
//! лежит в `super::macos_logic` и покрыта тестами на любой платформе.

mod proc;
mod single;
mod sys;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::macos_logic;
use crate::api::PowerAction;

pub use proc::{is_protected, kill, launch, post_close_to_windows, process_name_from_path, run_close_command, snapshot, visible_window_count};
pub use single::{Instance, acquire, signal_existing};
pub use sys::{get_volume, run_power, send_media_key, set_volume, show_toast};

/// Каталог, внутри которого лежит папка `PresetLauncher`: `~/Library/Application Support`.
pub fn config_base_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => macos_logic::app_support_dir(Path::new(&home)),
        _ => PathBuf::from("."),
    }
}

/// Локальное время (часы, минуты, секунды).
pub fn local_hms() -> (u32, u32, u32) {
    // SAFETY: `time` принимает NULL; `localtime_r` пишет только в переданную структуру.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return (0, 0, 0);
        }
        (tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32)
    }
}

/// Открывает файл/папку программой по умолчанию (`open`).
pub fn open_path(path: &Path) -> Result<(), String> {
    run_capture("/usr/bin/open", &[path.to_string_lossy().into_owned()], Duration::from_secs(10)).map(|_| ())
}

/// Hibernate на macOS не поддерживается.
pub fn power_supported(action: PowerAction) -> bool {
    action != PowerAction::Hibernate
}

/// На macOS вывод CLI идёт в терминал напрямую: ничего не нужно.
pub fn attach_console() {}

/// Автозапуск при входе: LaunchAgent в `~/Library/LaunchAgents` (действует со следующего входа).
pub fn autostart_set(enabled: bool) -> Result<(), String> {
    let home = std::env::var_os("HOME").ok_or("переменная HOME не задана")?;
    let path = macos_logic::launch_agent_path(Path::new(&home));
    if !enabled {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        };
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let plist = macos_logic::launch_agent_plist(&exe.to_string_lossy(), &["--autostart"]);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, plist).map_err(|e| e.to_string())
}

pub fn autostart_enabled() -> bool {
    std::env::var_os("HOME").is_some_and(|h| macos_logic::launch_agent_path(Path::new(&h)).is_file())
}

/// Максимум байт, которые мы сохраняем из stdout/stderr дочернего процесса.
const MAX_CAPTURE: u64 = 4 * 1024 * 1024;

/// Читает поток в отдельном потоке (иначе дочерний процесс может зависнуть на полном pipe,
/// пока мы ждём его завершения). Остаток сверх лимита вычитывается и отбрасывается.
fn spawn_reader(stream: Option<impl Read + Send + 'static>) -> Option<std::thread::JoinHandle<String>> {
    let mut stream = stream?;
    std::thread::Builder::new()
        .spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut stream).take(MAX_CAPTURE).read_to_end(&mut buf);
            let _ = std::io::copy(&mut stream, &mut std::io::sink());
            String::from_utf8_lossy(&buf).into_owned()
        })
        .ok()
}

/// Запускает программу, ждёт завершения не дольше `timeout` и возвращает stdout.
/// Ненулевой код выхода или таймаут — `Err` с текстом stderr.
pub(crate) fn run_capture(program: &str, args: &[String], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let out_reader = spawn_reader(child.stdout.take());
    let err_reader = spawn_reader(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("{program}: не завершился за {} с", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => break Err(format!("{program}: {e}")),
        }
    };
    let out = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let err = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let status = status?;
    if status.success() { Ok(out) } else { Err(format!("{program}: {status}: {}", err.trim())) }
}

/// Не теряет `Child` без `wait`: фоновый поток собирает завершившийся процесс (без зомби).
pub(crate) fn reap(mut child: Child) {
    let _ = std::thread::Builder::new().name("reap".into()).spawn(move || {
        let _ = child.wait();
    });
}


/// `osascript` с набором аргументов (`-e …` и т. п.).
pub(crate) fn osascript(args: &[String], timeout: Duration) -> Result<String, String> {
    run_capture("/usr/bin/osascript", args, timeout)
}

/// Расширения для диалога выбора программы (пакет `.app`).
pub const PROGRAM_EXTENSIONS: &[&str] = &["app"];
pub const PATH_HINT: &str = "/Applications/Программа.app, исполняемый файл или URL";
pub const PROCESS_HINT: &str = "напр. Discord";
pub const CLOSE_COMMAND_HINT: &str = "вместо мягкого закрытия, напр. osascript -e 'quit app \"Steam\"'";
pub const ARGS_HINT: &str = "например: --new-window https://… или -d ~/work";
