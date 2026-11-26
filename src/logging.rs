//! Журнал: уровни, фильтр, формат строки и ротация файла `launcher.log`.
//!
//! Формат строки: `HH:MM:SS текст` для info, `HH:MM:SS [ERR] текст` и `HH:MM:SS [DBG] текст`
//! для error и debug. Фильтр уровня глобальный и применяется и к файлу, и к журналу в окне.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Deserializer, Serialize};

/// Максимальный размер `launcher.log`, после которого файл ротируется.
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// Уровень записи и настройка фильтра. Порядок важен: Error < Info < Debug.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    #[default]
    Info,
    Debug,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::Error, Level::Info, Level::Debug];

    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Level::Error => " [ERR]",
            Level::Info => "",
            Level::Debug => " [DBG]",
        }
    }
}

/// Неизвестное значение в конфиге не ломает загрузку: берётся уровень по умолчанию.
impl<'de> Deserialize<'de> for Level {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Level::parse(&s).unwrap_or_default())
    }
}

static FILTER: AtomicU8 = AtomicU8::new(Level::Info as u8);

pub fn set_filter(level: Level) {
    FILTER.store(level as u8, Ordering::Relaxed);
}

pub fn filter() -> Level {
    match FILTER.load(Ordering::Relaxed) {
        0 => Level::Error,
        2 => Level::Debug,
        _ => Level::Info,
    }
}

/// Попадает ли запись уровня `level` в журнал при фильтре `filter`.
pub fn passes(filter: Level, level: Level) -> bool {
    level <= filter
}

pub fn enabled(level: Level) -> bool {
    passes(filter(), level)
}

pub fn format_line(time: &str, level: Level, msg: &str) -> String {
    format!("{time}{} {msg}", level.tag())
}

#[cfg(windows)]
pub fn timestamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
}

#[cfg(not(windows))]
pub fn timestamp() -> String {
    // Без платформенного API локального времени — UTC.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{:02}:{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60)
}

pub fn log_path() -> PathBuf {
    crate::config::app_dir().join("launcher.log")
}

/// Если файл больше `max` байт — переименовывает его в `<имя>.old` (одна старая копия).
pub fn rotate_if_needed(path: &Path, max: u64) {
    if std::fs::metadata(path).map(|m| m.len() > max).unwrap_or(false) {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        let old = PathBuf::from(old);
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(path, old);
    }
}

/// Путь к файлу журнала; файл создаётся пустым, если его ещё нет (чтобы его можно было открыть).
pub fn ensure_log_file() -> PathBuf {
    let path = log_path();
    let _ = std::fs::create_dir_all(crate::config::app_dir());
    let _ = std::fs::OpenOptions::new().create(true).append(true).open(&path);
    path
}

/// Ротация при старте приложения.
pub fn rotate_on_start() {
    rotate_if_needed(&log_path(), MAX_LOG_BYTES);
}

#[cfg(not(test))]
pub fn append_to_file(line: &str) {
    use std::io::Write;
    let path = log_path();
    rotate_if_needed(&path, MAX_LOG_BYTES);
    let _ = std::fs::create_dir_all(crate::config::app_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_levels() {
        assert_eq!(Level::parse("error"), Some(Level::Error));
        assert_eq!(Level::parse(" Debug "), Some(Level::Debug));
        assert_eq!(Level::parse("INFO"), Some(Level::Info));
        assert_eq!(Level::parse("trace"), None);
    }

    #[test]
    fn filter_semantics() {
        assert!(passes(Level::Error, Level::Error));
        assert!(!passes(Level::Error, Level::Info));
        assert!(!passes(Level::Error, Level::Debug));
        assert!(passes(Level::Info, Level::Error));
        assert!(passes(Level::Info, Level::Info));
        assert!(!passes(Level::Info, Level::Debug));
        assert!(Level::ALL.iter().all(|l| passes(Level::Debug, *l)));
    }

    #[test]
    fn line_format() {
        assert_eq!(format_line("01:02:03", Level::Info, "x"), "01:02:03 x");
        assert_eq!(format_line("01:02:03", Level::Error, "x"), "01:02:03 [ERR] x");
        assert_eq!(format_line("01:02:03", Level::Debug, "x"), "01:02:03 [DBG] x");
    }

    #[test]
    fn rotation_keeps_single_old_copy() {
        let dir = std::env::temp_dir().join(format!("pl-rot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("launcher.log");
        std::fs::write(&log, vec![b'a'; 100]).unwrap();
        rotate_if_needed(&log, 1000);
        assert!(log.exists());
        std::fs::write(&log, vec![b'b'; 2000]).unwrap();
        rotate_if_needed(&log, 1000);
        assert!(!log.exists());
        let old = dir.join("launcher.log.old");
        assert_eq!(std::fs::read(&old).unwrap().len(), 2000);
        // повторная ротация заменяет старую копию
        std::fs::write(&log, vec![b'c'; 3000]).unwrap();
        rotate_if_needed(&log, 1000);
        assert_eq!(std::fs::read(&old).unwrap().len(), 3000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
