//! Платформенный слой: процессы, питание, громкость, медиа, уведомления, single-instance,
//! каталог конфига, локальное время, открытие файлов. Остальной код вызывает только
//! `crate::platform::*` и не знает, на какой ОС работает.
//!
//! - `win` — Windows (Win32/WinRT);
//! - `mac` — macOS (osascript/pmset/open + AppKit через objc2);
//! - `macos_logic` — чистая логика macOS (экранирование, разбор вывода утилит, plist),
//!   компилируется на всех платформах, чтобы тесты шли и на Windows.

use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!("Preset Launcher поддерживает только Windows и macOS");

#[allow(dead_code)] // на Windows используется только в тестах
pub mod macos_logic;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::*;

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::*;

/// Максимум одновременно показываемых уведомлений (сверх — `/api/notify` отвечает 503).
const MAX_TOASTS: usize = 4;
static TOASTS_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Занятый слот показа уведомления; освобождается при drop.
pub struct ToastSlot(());

impl Drop for ToastSlot {
    fn drop(&mut self) {
        TOASTS_ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn acquire_toast_slot() -> Option<ToastSlot> {
    if TOASTS_ACTIVE.fetch_add(1, Ordering::SeqCst) >= MAX_TOASTS {
        TOASTS_ACTIVE.fetch_sub(1, Ordering::SeqCst);
        return None;
    }
    Some(ToastSlot(()))
}

/// Результат попытки стать единственным экземпляром.
pub enum Acquire {
    Got(Instance),
    AlreadyRunning,
    /// Защиту создать не удалось (диагностика): запускаться без неё нельзя.
    Failed(String),
}
