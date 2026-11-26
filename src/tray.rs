//! System tray icon + menu. Tray callbacks run on the main (UI) thread's message
//! loop; they forward events to the app through a channel and wake egui, so they
//! work even while the window is hidden (eframe then calls `App::logic`).

use std::sync::mpsc::Sender;

use eframe::egui;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub enum UiEvent {
    Show,
    Switch(String),
    /// Нажата глобальная горячая клавиша пресета.
    Hotkey(String),
    Quit,
}

const ID_SHOW: &str = "show";
const ID_QUIT: &str = "quit";
const PRESET_PREFIX: &str = "preset:";

pub fn install_handlers(tx: Sender<UiEvent>, ctx: egui::Context) {
    {
        let tx = tx.clone();
        let ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let id = e.id.0.as_str();
            let ev = match id {
                ID_SHOW => UiEvent::Show,
                ID_QUIT => UiEvent::Quit,
                _ => match id.strip_prefix(PRESET_PREFIX) {
                    Some(name) => UiEvent::Switch(name.to_string()),
                    None => return,
                },
            };
            let _ = tx.send(ev);
            ctx.request_repaint();
        }));
    }
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        let show = matches!(
            e,
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
        );
        if show {
            let _ = tx.send(UiEvent::Show);
            ctx.request_repaint();
        }
    }));
}

#[cfg(windows)]
fn load_icon() -> Option<Icon> {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON, SM_CYSMICON};
    // Resource #1 is the .ico embedded by build.rs; ask for the DPI-correct small size.
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
    let size = (w.max(16) as u32, h.max(16) as u32);
    Icon::from_resource(1, Some(size))
        .ok()
        .or_else(|| Icon::from_rgba(crate::icon_art::render_icon_rgba(32), 32, 32).ok())
}

/// macOS: значок строки меню (22 pt, рисуем 44 px для Retina).
#[cfg(target_os = "macos")]
fn load_icon() -> Option<Icon> {
    Icon::from_rgba(crate::icon_art::render_icon_rgba(44), 44, 44).ok()
}

/// Подпись пункта меню: иконка пресета (если есть) перед названием.
pub fn menu_label(name: &str, icon: &str) -> String {
    let icon = icon.trim();
    if icon.is_empty() { name.to_string() } else { format!("{icon} {name}") }
}

fn build_menu(presets: &[(String, String)], active: Option<&str>, busy: bool) -> Menu {
    let menu = Menu::new();
    for (name, icon) in presets {
        let checked = active == Some(name.as_str());
        let item = CheckMenuItem::with_id(format!("{PRESET_PREFIX}{name}"), menu_label(name, icon), !busy, checked, None);
        let _ = menu.append(&item);
    }
    if !presets.is_empty() {
        let _ = menu.append(&PredefinedMenuItem::separator());
    }
    let _ = menu.append(&MenuItem::with_id(ID_SHOW, "Открыть окно", true, None));
    let _ = menu.append(&MenuItem::with_id(ID_QUIT, "Выход", true, None));
    menu
}

fn tooltip(active: Option<&str>, busy: bool) -> String {
    match (active, busy) {
        (_, true) => "Preset Launcher — переключение…".into(),
        (Some(a), _) => format!("Preset Launcher — {a}"),
        (None, _) => "Preset Launcher".into(),
    }
}

/// (пресеты: имя и иконка, активный пресет, занят ли движок)
type TrayKey = (Vec<(String, String)>, Option<String>, bool);

pub struct Tray {
    icon: TrayIcon,
    last: Option<TrayKey>,
}

impl Tray {
    pub fn new() -> anyhow::Result<Self> {
        let mut builder = TrayIconBuilder::new()
            .with_tooltip("Preset Launcher")
            .with_menu(Box::new(build_menu(&[], None, false)))
            .with_menu_on_left_click(false);
        if let Some(icon) = load_icon() {
            builder = builder.with_icon(icon);
        }
        Ok(Self { icon: builder.build()?, last: None })
    }

    /// Rebuilds menu/tooltip only when something visible changed.
    pub fn sync(&mut self, presets: Vec<(String, String)>, active: Option<String>, busy: bool) {
        let key = (presets, active, busy);
        if self.last.as_ref() == Some(&key) {
            return;
        }
        let (presets, active, busy) = &key;
        self.icon.set_menu(Some(Box::new(build_menu(presets, active.as_deref(), *busy))));
        let _ = self.icon.set_tooltip(Some(tooltip(active.as_deref(), *busy)));
        self.last = Some(key);
    }
}

#[cfg(test)]
mod tests {
    use super::menu_label;

    #[test]
    fn menu_label_prefixes_icon() {
        assert_eq!(menu_label("Игры", "🎮"), "🎮 Игры");
        assert_eq!(menu_label("Игры", "  "), "Игры");
        assert_eq!(menu_label("Работа", ""), "Работа");
    }
}
