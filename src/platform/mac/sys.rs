//! Системные действия macOS для HTTP API: питание, громкость, медиаклавиши, уведомления.

use std::sync::Mutex;
use std::time::Duration;

use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
use objc2_core_graphics::{CGEvent, CGEventTapLocation, CGPreflightPostEventAccess};
use objc2_foundation::NSPoint;

use super::super::macos_logic as logic;
use super::{osascript, run_capture};
use crate::api::{LogFn, MediaKey, PowerAction, UNSUPPORTED, VolumeChange, apply_volume};
use crate::logging::Level;

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(10);

fn script(src: &str) -> Vec<String> {
    vec!["-e".into(), src.into()]
}

/// Выполняет действие питания немедленно.
pub fn run_power(action: PowerAction, log: &LogFn) -> Result<(), String> {
    match action {
        // Без принуждения: программы с несохранённой работой могут задержать выключение.
        PowerAction::Shutdown => osascript(&script("tell application \"System Events\" to shut down"), SCRIPT_TIMEOUT).map(|_| ()),
        PowerAction::Restart => osascript(&script("tell application \"System Events\" to restart"), SCRIPT_TIMEOUT).map(|_| ()),
        PowerAction::Sleep => {
            // Пауза ~1 с, чтобы ответ 202 успел уйти клиенту (как на Windows).
            let log = log.clone();
            std::thread::Builder::new()
                .name("preset-suspend".into())
                .spawn(move || {
                    std::thread::sleep(Duration::from_secs(1));
                    match run_capture("/usr/bin/pmset", &["sleepnow".into()], Duration::from_secs(15)) {
                        Ok(_) => log(Level::Info, "API: «sleep» — система вышла из состояния сна"),
                        Err(e) => log(Level::Error, &format!("API: «sleep» не выполнен — {e}")),
                    }
                })
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        PowerAction::Hibernate => Err(UNSUPPORTED.into()),
        // Без запасных вариантов: гашение дисплея не блокирует сеанс при отложенном запросе пароля,
        // а ложный «успех» хуже ошибки.
        PowerAction::Lock => osascript(&script(logic::LOCK_SCRIPT), SCRIPT_TIMEOUT).map(|_| ()).map_err(|e| {
            format!("не удалось заблокировать экран ({e}); проверьте разрешения Универсальный доступ и Автоматизация (System Events) для Preset Launcher")
        }),
    }
}

/// Громкость 0–100 и mute устройства вывода по умолчанию.
pub fn get_volume() -> Result<(u8, bool), String> {
    let out = osascript(&script("get volume settings"), SCRIPT_TIMEOUT)?;
    logic::parse_volume_settings(&out)
}

/// Применяет `level`/`delta`/`muted` и возвращает итоговые значения.
pub fn set_volume(change: VolumeChange) -> Result<(u8, bool), String> {
    // Read-modify-write под глобальным локом: параллельные `delta` не теряют друг друга.
    static VOLUME_LOCK: Mutex<()> = Mutex::new(());
    let _guard = VOLUME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cur = get_volume()?;
    let new = apply_volume(cur, change);
    if change.level.is_some() || (change.delta.is_some() && new.0 != cur.0) {
        // Числа и bool форматируются нами, пользовательских строк в скрипте нет.
        osascript(&script(&format!("set volume output volume {}", new.0)), SCRIPT_TIMEOUT)?;
    }
    if change.muted.is_some() {
        osascript(&script(&format!("set volume output muted {}", new.1)), SCRIPT_TIMEOUT)?;
    }
    get_volume()
}

/// Системное media-событие (NX_SYSDEFINED, subtype 8) через CGEventPost.
/// Может потребовать разрешения «Универсальный доступ»/«Мониторинг ввода».
pub fn send_media_key(key: MediaKey) -> Result<(), String> {
    let code = logic::media_key_code(key).ok_or_else(|| UNSUPPORTED.to_string())?;
    if !CGPreflightPostEventAccess() {
        return Err("нет разрешения на отправку клавиш: включите Preset Launcher в Системные настройки → Конфиденциальность и безопасность → Универсальный доступ (Accessibility)".into());
    }
    for down in [true, false] {
        let data1 = logic::media_key_data1(code, down);
        let flags = NSEventModifierFlags::from_bits_retain(if down { 0xA00 } else { 0xB00 });
        let ev = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::SystemDefined,
            NSPoint::new(0.0, 0.0),
            flags,
            0.0,
            0,
            None,
            8,
            data1 as isize,
            -1,
        )
        .ok_or("не удалось создать системное событие клавиши")?;
        let cg = ev.CGEvent().ok_or("у события нет CGEvent")?;
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&cg));
    }
    Ok(())
}

/// Показывает уведомление Центра уведомлений. Текст передаётся аргументами `on run argv`,
/// а не вставляется в AppleScript.
pub fn show_toast(title: &str, message: &str) -> Result<(), String> {
    osascript(&logic::notification_osascript_args(title, message), SCRIPT_TIMEOUT).map(|_| ())
}
