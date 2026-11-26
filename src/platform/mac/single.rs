//! Single-instance на macOS: эксклюзивный `flock` на файле в каталоге конфига.
//! Второй запуск просит первый показать окно через файл-запрос, который тот опрашивает.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::config::app_dir;
use crate::platform::Acquire;

pub struct Instance {
    /// Держит блокировку, пока жив процесс.
    _lock: File,
}

fn lock_path() -> PathBuf {
    app_dir().join("instance.lock")
}

fn show_request_path() -> PathBuf {
    app_dir().join("show.request")
}

/// Берёт эксклюзивную блокировку. Любая ошибка, кроме «уже занято», — `Failed`:
/// без блокировки запускаться нельзя (два экземпляра делили бы конфиг и порт API).
pub fn acquire() -> Acquire {
    let started = SystemTime::now();
    if let Err(e) = std::fs::create_dir_all(app_dir()) {
        return Acquire::Failed(format!("не удалось создать каталог {}: {e}", app_dir().display()));
    }
    let file = match OpenOptions::new().create(true).truncate(false).write(true).open(lock_path()) {
        Ok(f) => f,
        Err(e) => return Acquire::Failed(format!("не удалось открыть {}: {e}", lock_path().display())),
    };
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Acquire::AlreadyRunning,
        Err(TryLockError::Error(e)) => return Acquire::Failed(format!("flock на {} не удался: {e}", lock_path().display())),
    }
    // Устаревший запрос от прошлого запуска удаляем, но свежий (от второго экземпляра, успевшего
    // записать его уже после начала нашего запуска) оставляем.
    let path = show_request_path();
    if std::fs::metadata(&path).and_then(|m| m.modified()).is_ok_and(|t| t < started) {
        let _ = std::fs::remove_file(&path);
    }
    Acquire::Got(Instance { _lock: file })
}

/// Просит уже запущенный экземпляр показать окно.
pub fn signal_existing() {
    let _ = std::fs::write(show_request_path(), b"1");
}

impl Instance {
    /// Вызывает `on_show` каждый раз, когда другой экземпляр просит показать окно.
    pub fn watch(&self, on_show: impl Fn() + Send + 'static) {
        std::thread::Builder::new()
            .name("show-watch".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(400));
                    if std::fs::remove_file(show_request_path()).is_ok() {
                        on_show();
                    }
                }
            })
            .expect("spawn show watcher");
    }
}
