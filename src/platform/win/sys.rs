//! Системные действия Windows для HTTP API: питание, громкость, медиаклавиши, toast.
//!
//! Весь «настоящий» код, который трогает ОС, живёт здесь; `api.rs` обращается к нему
//! только через trait `Backend`, поэтому в тестах эти функции не вызываются.

use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
use std::sync::Mutex;

use windows::Win32::System::Power::SetSuspendState;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
    VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK, VK_MEDIA_STOP,
};
use windows::core::HSTRING;

use crate::logging::Level;
use crate::api::{LogFn, MediaKey, PowerAction, VolumeChange, apply_volume, is_xml_char};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// AppUserModelID приложения: тот же идентификатор задан ярлыку в меню «Пуск» установщиком.
pub const AUMID: &str = "PresetLauncher.App";

/// COM, инициализированный (MTA) в текущем потоке; `CoUninitialize` — при drop.
struct ComGuard(bool);

impl ComGuard {
    fn new() -> Result<Self, String> {
        // S_OK / S_FALSE — нужно парно вызвать CoUninitialize; RPC_E_CHANGED_MODE — поток уже
        // в STA (например, UI-поток): работаем в нём как есть и CoUninitialize не вызываем.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            Ok(Self(true))
        } else if hr.0 as u32 == 0x8001_0106 {
            Ok(Self(false))
        } else {
            Err(format!("CoInitializeEx: {hr:?}"))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

// ---------------------------------------------------------------------------
// Питание
// ---------------------------------------------------------------------------

fn run_shutdown_exe(arg: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    // Без /f: программы с несохранённой работой могут задержать выключение, но не теряют данные.
    let out = std::process::Command::new("shutdown.exe")
        .args([arg, "/t", "0"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err(format!("код выхода {}", out.status)) }
}

/// `SetSuspendState` блокируется до пробуждения, поэтому вызывается в отдельном потоке
/// и результат не ждётся. Перед вызовом пауза ~1 с, чтобы ответ 202 успел уйти клиенту
/// (важно при `delay_secs: 0`). Итог (в т.ч. код ошибки) пишется в журнал из потока.
fn suspend(hibernate: bool, log: &LogFn) -> Result<(), String> {
    let name = if hibernate { "hibernate" } else { "sleep" };
    let log = log.clone();
    std::thread::Builder::new()
        .name("preset-suspend".into())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(1));
            // При включённой гибернации SetSuspendState(FALSE) всё равно уходит в сон — это нормально.
            if unsafe { SetSuspendState(hibernate, false, false) } {
                log(Level::Info, &format!("API: «{name}» — система вышла из состояния сна"));
            } else {
                let code = unsafe { windows::Win32::Foundation::GetLastError() };
                log(Level::Error, &format!("API: «{name}» не выполнен — SetSuspendState вернул FALSE (GetLastError {})", code.0));
            }
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Выполняет действие питания немедленно.
pub fn run_power(action: PowerAction, log: &LogFn) -> Result<(), String> {
    match action {
        PowerAction::Shutdown => run_shutdown_exe("/s"),
        PowerAction::Restart => run_shutdown_exe("/r"),
        PowerAction::Sleep => suspend(false, log),
        PowerAction::Hibernate => suspend(true, log),
        PowerAction::Lock => unsafe { LockWorkStation() }.map_err(|e| e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Громкость (Core Audio)
// ---------------------------------------------------------------------------

/// Раскладывает работу с `IAudioEndpointVolume` в одном месте; COM живёт на время вызова.
fn with_endpoint<T>(f: impl FnOnce(&IAudioEndpointVolume) -> windows::core::Result<T>) -> Result<T, String> {
    // `_com` объявлен первым — значит, дропается последним, после всех COM-объектов ниже.
    let _com = ComGuard::new()?;
    let run = || -> windows::core::Result<T> {
        let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)? };
        let volume: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None)? };
        f(&volume)
    };
    run().map_err(|e| e.to_string())
}

fn read_volume(v: &IAudioEndpointVolume) -> windows::core::Result<(u8, bool)> {
    let scalar = unsafe { v.GetMasterVolumeLevelScalar()? };
    let muted = unsafe { v.GetMute()? }.as_bool();
    Ok(((scalar * 100.0).round().clamp(0.0, 100.0) as u8, muted))
}

/// Громкость 0–100 и mute устройства вывода по умолчанию.
pub fn get_volume() -> Result<(u8, bool), String> {
    with_endpoint(read_volume)
}

/// Применяет `level`/`delta`/`muted` и возвращает итоговые значения.
pub fn set_volume(change: VolumeChange) -> Result<(u8, bool), String> {
    // Read-modify-write под глобальным локом: параллельные `delta` не теряют друг друга.
    static VOLUME_LOCK: Mutex<()> = Mutex::new(());
    let _guard = VOLUME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_endpoint(|v| {
        let cur = read_volume(v)?;
        let new = apply_volume(cur, change);
        // Явный `level` пишем всегда (текущее значение округлено и может отличаться от скаляра,
        // напр. 0.004 читается как 0); `delta` — только если уровень реально меняется.
        if change.level.is_some() || (change.delta.is_some() && new.0 != cur.0) {
            unsafe { v.SetMasterVolumeLevelScalar(f32::from(new.0) / 100.0, std::ptr::null())? };
        }
        if change.muted.is_some() {
            unsafe { v.SetMute(new.1, std::ptr::null())? };
        }
        read_volume(v)
    })
}

// ---------------------------------------------------------------------------
// Медиаклавиши
// ---------------------------------------------------------------------------

pub fn send_media_key(key: MediaKey) -> Result<(), String> {
    let vk = match key {
        MediaKey::PlayPause => VK_MEDIA_PLAY_PAUSE,
        MediaKey::Next => VK_MEDIA_NEXT_TRACK,
        MediaKey::Prev => VK_MEDIA_PREV_TRACK,
        MediaKey::Stop => VK_MEDIA_STOP,
    };
    let ev = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk.0), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    let inputs = [ev(KEYBD_EVENT_FLAGS(0)), ev(KEYEVENTF_KEYUP)];
    let size = std::mem::size_of::<INPUT>() as i32;
    let sent = unsafe { SendInput(&inputs, size) };
    if sent as usize == inputs.len() {
        return Ok(());
    }
    // Клавиша могла «залипнуть» (down без up): пробуем отдельно отпустить её.
    let mut msg = format!("SendInput отправил {sent} из {}", inputs.len());
    if sent == 1 {
        let up = unsafe { SendInput(&inputs[1..], size) };
        msg.push_str(&format!("; отдельный keyup: {up} из 1"));
    }
    Err(msg)
}

// ---------------------------------------------------------------------------
// Toast-уведомления
// ---------------------------------------------------------------------------

/// Экранирует текст для XML; символы, недопустимые в XML 1.0, выбрасываются.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().filter(|c| is_xml_char(*c)) {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

pub fn toast_xml(title: &str, message: &str) -> String {
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(message)
    )
}

/// Инициализация WinRT (MTA) в текущем потоке; `RoUninitialize` — при drop.
struct RoGuard(bool);

impl RoGuard {
    fn new() -> Result<Self, String> {
        match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            Ok(()) => Ok(Self(true)),
            // Поток уже в другом апартаменте (RPC_E_CHANGED_MODE): работаем как есть.
            Err(e) if e.code().0 as u32 == 0x8001_0106 => Ok(Self(false)),
            Err(e) => Err(format!("RoInitialize: {e}")),
        }
    }
}

impl Drop for RoGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { RoUninitialize() };
        }
    }
}

/// Показывает toast от имени `AUMID`. Работает, если в меню «Пуск» есть ярлык с этим
/// AppUserModelID (его создаёт установщик); при запуске без установки Windows может
/// молча не показать уведомление.
pub fn show_toast(title: &str, message: &str) -> Result<(), String> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    let _ro = RoGuard::new()?;
    let run = || -> windows::core::Result<()> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(toast_xml(title, message)))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?;
        notifier.Show(&toast)
    };
    run().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml() {
        assert_eq!(xml_escape(r#"<a href="x">&'</a>"#), "&lt;a href=&quot;x&quot;&gt;&amp;&apos;&lt;/a&gt;");
        assert_eq!(xml_escape("a\u{0}b\u{8}c\nd"), "abc\nd");
        assert_eq!(xml_escape("Привет"), "Привет");
    }

    /// Ручная проверка на реальной системе: `cargo test -- --ignored --nocapture manual_`.
    #[test]
    #[ignore = "трогает реальное аудио ОС"]
    fn manual_volume_roundtrip() {
        let orig = get_volume().expect("get_volume");
        println!("volume = {orig:?}");
        let same = set_volume(VolumeChange { level: None, delta: Some(0), muted: None }).expect("delta 0");
        assert_eq!(same, orig);
        let back = set_volume(VolumeChange { level: Some(orig.0), delta: None, muted: Some(orig.1) }).expect("restore");
        assert_eq!(back, orig);
    }

    #[test]
    #[ignore = "показывает реальный toast"]
    fn manual_toast() {
        show_toast("Preset Launcher", "Проверка уведомления <test> & ok").expect("toast");
    }

    #[test]
    fn toast_xml_is_escaped() {
        let x = toast_xml("<b>", "a & b");
        assert!(x.contains("<text>&lt;b&gt;</text><text>a &amp; b</text>"), "{x}");
    }
}
