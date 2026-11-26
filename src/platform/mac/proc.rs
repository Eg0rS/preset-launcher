//! Процессы на macOS: перечисление (`ps`), запуск (`open`), мягкое/принудительное закрытие
//! (NSRunningApplication / сигналы).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2_app_kit::NSRunningApplication;

use super::super::macos_logic::{self, LaunchPlan};
use super::{reap, run_capture};

pub fn is_protected(process_name: &str) -> bool {
    macos_logic::is_protected(process_name)
}

/// `имя процесса (нижний регистр) -> pid` для всех процессов, кроме нашего; `None` — `ps` не сработал.
pub fn snapshot() -> Option<HashMap<String, Vec<u32>>> {
    // Ошибку не маскируем пустым списком: вызывающий решает, что делать (engine ничего не закрывает
    // и не запускает повторно). Одна повторная попытка на случай сбоя.
    (0..2).find_map(|_| {
        run_capture("/bin/ps", &["-axo".into(), "pid=,comm=".into()], Duration::from_secs(5)).ok().map(|out| macos_logic::parse_ps(&out, std::process::id()))
    })
}

/// Окна на macOS без разрешения Accessibility не перечислить, поэтому считаем, что окно
/// есть у каждого процесса: ранний «kill после сворачивания в трей» не срабатывает,
/// приложение закрывается либо само, либо по таймауту (если включено `force_kill`).
pub fn visible_window_count(pids: &[u32]) -> usize {
    pids.len()
}

/// Просит приложения завершиться штатно (как Cmd+Q). Возвращает, скольким отправлен запрос.
pub fn post_close_to_windows(pids: &[u32]) -> usize {
    pids.iter()
        .filter(|&&pid| {
            let pid = pid as libc::pid_t;
            match NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
                Some(app) => app.terminate(),
                // Не GUI-приложение (нет NSRunningApplication) — обычный SIGTERM.
                None => unsafe { libc::kill(pid, libc::SIGTERM) == 0 },
            }
        })
        .count()
}

/// Принудительное завершение (forceTerminate или SIGKILL).
pub fn kill(pid: u32) -> bool {
    let pid = pid as libc::pid_t;
    if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        && app.forceTerminate()
    {
        return true;
    }
    unsafe { libc::kill(pid, libc::SIGKILL) == 0 }
}

/// Пользовательская команда закрытия: выполняется через `sh -c`, без ожидания.
pub fn run_close_command(cmdline: &str) -> std::io::Result<()> {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(cmdline)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(reap)
}

/// Запуск: `.app` — `open -a`, URL/документ — `open`, исполняемый файл — напрямую.
/// Путь `~/…` раскрывается.
pub fn launch(path: &str, args: &str, working_dir: &str) -> Result<(), String> {
    let path = expand_tilde(path.trim());
    match macos_logic::plan_launch(&path, args, is_executable_file(&path)) {
        LaunchPlan::Open(v) => run_capture("/usr/bin/open", &v, Duration::from_secs(15)).map(|_| ()),
        LaunchPlan::Direct { program, args } => {
            let mut cmd = Command::new(program);
            cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            if !working_dir.trim().is_empty() {
                cmd.current_dir(expand_tilde(working_dir.trim()));
            }
            cmd.spawn().map(reap).map_err(|e| e.to_string())
        }
    }
}

pub(crate) fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return format!("{}/{rest}", home.to_string_lossy().trim_end_matches('/'));
    }
    p.to_string()
}

type NameCache = HashMap<String, (Instant, String)>;

/// Имя процесса для пути программы. Для `.app` читается `CFBundleExecutable` из Info.plist
/// (имя исполняемого файла часто отличается от имени пакета); найденное кэшируется на 30 с.
pub fn process_name_from_path(path: &str) -> Option<String> {
    let path = expand_tilde(path.trim());
    if macos_logic::is_app_bundle(&path) {
        static CACHE: Mutex<Option<NameCache>> = Mutex::new(None);
        let key = path.trim_end_matches('/').to_string();
        {
            let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, name)) = guard.get_or_insert_with(HashMap::new).get(&key)
                && at.elapsed() < Duration::from_secs(30)
            {
                return Some(name.clone());
            }
        }
        let plist = format!("{key}/Contents/Info.plist");
        let exe = run_capture(
            "/usr/bin/plutil",
            &["-extract".into(), "CFBundleExecutable".into(), "raw".into(), "-o".into(), "-".into(), plist],
            Duration::from_secs(3),
        )
        .ok()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
        if let Some(name) = exe {
            let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            guard.get_or_insert_with(HashMap::new).insert(key, (Instant::now(), name.clone()));
            return Some(name);
        }
    }
    macos_logic::process_name_from_path(&path, is_executable_file(&path))
}

fn is_executable_file(path: &str) -> bool {
    std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}
