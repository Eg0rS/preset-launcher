//! Single-instance guard. A second launch signals the running instance to show
//! its window (named event) and exits.

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
use windows::core::w;

pub struct Instance {
    _mutex: HANDLE,
    show_event: HANDLE,
}

/// Returns `None` if another instance is already running.
pub fn acquire() -> Option<Instance> {
    unsafe {
        let mutex = CreateMutexW(None, false, w!("Local\\PresetLauncher.Singleton")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return None;
        }
        let show_event = CreateEventW(None, false, false, w!("Local\\PresetLauncher.Show")).ok()?;
        Some(Instance { _mutex: mutex, show_event })
    }
}

/// Asks the already running instance to show its window.
pub fn signal_existing() {
    unsafe {
        // Let the other process take foreground focus.
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, w!("Local\\PresetLauncher.Show")) {
            let _ = SetEvent(ev);
        }
    }
}

impl Instance {
    /// Calls `on_show` every time another instance signals us.
    pub fn watch(&self, on_show: impl Fn() + Send + 'static) {
        let raw = self.show_event.0 as isize;
        std::thread::Builder::new()
            .name("show-watch".into())
            .spawn(move || {
                let ev = HANDLE(raw as *mut _);
                loop {
                    let r = unsafe { WaitForSingleObject(ev, INFINITE) };
                    if r.0 != 0 {
                        break; // WAIT_OBJECT_0 == 0; anything else = handle gone
                    }
                    on_show();
                }
            })
            .expect("spawn show watcher");
    }
}
