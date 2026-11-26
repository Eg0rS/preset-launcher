//! Реализация платформенного слоя для Windows (Win32 / WinRT через крейт `windows`).

mod proc;
mod single;
mod sys;

use std::path::{Path, PathBuf};

use crate::api::PowerAction;

pub use proc::{is_protected, kill, launch, post_close_to_windows, run_close_command, snapshot, visible_window_count};
pub use single::{Instance, acquire, signal_existing};
pub use sys::{get_volume, run_power, send_media_key, set_volume, show_toast};

/// Каталог, внутри которого лежит папка `PresetLauncher` (%APPDATA%).
pub fn config_base_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Локальное время (часы, минуты, секунды).
pub fn local_hms() -> (u32, u32, u32) {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (u32::from(t.wHour), u32::from(t.wMinute), u32::from(t.wSecond))
}

/// Имя процесса (нижний регистр), по которому отслеживается программа с данным путём.
/// Windows: только `*.exe`.
pub fn process_name_from_path(path: &str) -> Option<String> {
    let file = Path::new(path).file_name()?.to_string_lossy().to_lowercase();
    if file.ends_with(".exe") { Some(file) } else { None }
}

/// Открывает файл/папку программой по умолчанию.
pub fn open_path(path: &Path) -> Result<(), String> {
    let p = path.to_string_lossy();
    if path.is_dir() {
        proc::launch("explorer.exe", &format!("\"{p}\""), "")
    } else {
        proc::launch(&p, "", "")
    }
}

/// Все действия питания поддерживаются.
pub fn power_supported(_action: PowerAction) -> bool {
    true
}

/// Подключает консоль родительского процесса, чтобы CLI мог печатать из GUI-сборки
/// (`windows_subsystem = "windows"`). Если stdout/stderr уже перенаправлены — не трогает их.
pub fn attach_console() {
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows::core::w;

    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    let missing = |h: STD_HANDLE| match unsafe { GetStdHandle(h) } {
        Ok(handle) => handle.is_invalid() || handle == HANDLE::default() || handle == INVALID_HANDLE_VALUE,
        Err(_) => true,
    };
    for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if missing(which) {
            let con = unsafe {
                CreateFileW(
                    w!("CONOUT$"),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
            };
            if let Ok(h) = con {
                let _ = unsafe { SetStdHandle(which, h) };
            }
        }
    }
}


/// Расширения для диалога выбора программы.
pub const PROGRAM_EXTENSIONS: &[&str] = &["exe", "lnk", "bat", "cmd", "url"];
pub const PATH_HINT: &str = r"C:\…\program.exe, ярлык или URL";
pub const PROCESS_HINT: &str = "напр. Discord.exe";
pub const CLOSE_COMMAND_HINT: &str = r#"вместо закрытия окон, напр. "C:\Steam\steam.exe" -shutdown"#;
pub const ARGS_HINT: &str = r"например: --new-window https://… или -d C:\work";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_name_derivation() {
        assert_eq!(process_name_from_path(r"C:\x\Foo.EXE").as_deref(), Some("foo.exe"));
        assert_eq!(process_name_from_path(r"C:\x\Foo.lnk"), None);
    }
}
