//! Глобальные горячие клавиши пресетов (крейт `global-hotkey`, кроссплатформенный).
//!
//! По умолчанию выключены: пока `settings.hotkeys_enabled == false`, менеджер даже не
//! создаётся и ничего не регистрируется. Событие нажатия приходит из обработчика на
//! главном потоке (тот же поток/цикл сообщений, что и у событий трея), уходит в канал
//! `UiEvent::Hotkey` и обрабатывается в `App::logic`, которое вызывается и при скрытом окне.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use eframe::egui;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::config::{Preset, Settings};
use crate::engine::Engine;
use crate::logging::Level;
use crate::tray::UiEvent;

/// Разбирает строку вида "Ctrl+Alt+1". Нужен хотя бы один модификатор: одиночная
/// клавиша глобально перехватила бы обычный ввод во всех программах.
pub fn parse_hotkey(s: &str) -> Result<HotKey, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("пустая строка".into());
    }
    let hk: HotKey = s.parse().map_err(|e: global_hotkey::hotkey::HotKeyParseError| e.to_string())?;
    if hk.mods.is_empty() {
        return Err("нужен хотя бы один модификатор (Ctrl, Alt, Shift или Super)".into());
    }
    Ok(hk)
}

pub struct Hotkeys {
    tx: Sender<UiEvent>,
    ctx: egui::Context,
    mgr: Option<GlobalHotKeyManager>,
    ids: Arc<Mutex<HashMap<u32, String>>>,
    registered: Vec<HotKey>,
    /// Последний применённый набор (имя пресета, строка хоткея); `None` — ещё не применялся.
    applied: Option<Vec<(String, String)>>,
    /// Ошибки по имени пресета (для показа в UI).
    errors: HashMap<String, String>,
    /// Уже записанные в журнал ошибки (чтобы не повторять при каждом сохранении).
    logged: Vec<String>,
}

impl Hotkeys {
    pub fn new(tx: Sender<UiEvent>, ctx: egui::Context) -> Self {
        Self {
            tx,
            ctx,
            mgr: None,
            ids: Arc::new(Mutex::new(HashMap::new())),
            registered: Vec::new(),
            applied: None,
            errors: HashMap::new(),
            logged: Vec::new(),
        }
    }

    pub fn error_for(&self, preset: &str) -> Option<&str> {
        self.errors.get(preset).map(String::as_str)
    }

    fn ensure_manager(&mut self) -> Result<&GlobalHotKeyManager, String> {
        if self.mgr.is_none() {
            let mgr = GlobalHotKeyManager::new().map_err(|e| format!("не удалось инициализировать горячие клавиши: {e}"))?;
            let (tx, ctx, ids) = (self.tx.clone(), self.ctx.clone(), self.ids.clone());
            GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
                if e.state() != HotKeyState::Pressed {
                    return;
                }
                let name = ids.lock().unwrap().get(&e.id()).cloned();
                if let Some(name) = name {
                    let _ = tx.send(UiEvent::Hotkey(name));
                    ctx.request_repaint();
                }
            }));
            self.mgr = Some(mgr);
        }
        Ok(self.mgr.as_ref().expect("manager just created"))
    }

    /// Приводит регистрацию в соответствие с настройками. Дешёвый no-op, если ничего не менялось.
    pub fn apply(&mut self, settings: &Settings, presets: &[Preset], engine: &Engine) {
        let desired: Vec<(String, String)> = if settings.hotkeys_enabled {
            presets
                .iter()
                .filter(|p| !p.hotkey.trim().is_empty())
                .map(|p| (p.name.clone(), p.hotkey.trim().to_string()))
                .collect()
        } else {
            Vec::new()
        };
        if self.applied.as_ref() == Some(&desired) {
            return;
        }
        self.applied = Some(desired.clone());

        if let Some(mgr) = &self.mgr {
            let _ = mgr.unregister_all(&self.registered);
        }
        self.registered.clear();
        self.ids.lock().unwrap().clear();
        self.errors.clear();
        if desired.is_empty() {
            // Выключено (или нечего регистрировать): освобождаем менеджер целиком.
            if self.mgr.take().is_some() {
                GlobalHotKeyEvent::set_event_handler(None::<fn(GlobalHotKeyEvent)>);
                engine.log_at(Level::Info, "Горячие клавиши: сняты");
            }
            self.logged.clear();
            return;
        }

        let mut new_logged = Vec::new();
        let mut taken: HashMap<u32, String> = HashMap::new();
        let mut ok = 0;
        for (name, text) in &desired {
            let result = self.register_one(name, text, &mut taken);
            match result {
                Ok(()) => {
                    ok += 1;
                    engine.log_at(Level::Debug, format!("Горячая клавиша «{text}» → пресет «{name}»"));
                }
                Err(err) => {
                    let key = format!("{text}|{err}");
                    if !self.logged.contains(&key) {
                        engine.log_error(format!("Горячая клавиша «{text}» для пресета «{name}»: {err}"));
                    }
                    new_logged.push(key);
                    self.errors.insert(name.clone(), err);
                }
            }
        }
        self.logged = new_logged;
        if ok > 0 {
            engine.log_at(Level::Info, format!("Горячие клавиши: зарегистрировано {ok}"));
        }
    }

    fn register_one(&mut self, name: &str, text: &str, taken: &mut HashMap<u32, String>) -> Result<(), String> {
        let hk = parse_hotkey(text)?;
        if let Some(other) = taken.get(&hk.id()) {
            return Err(format!("уже назначена пресету «{other}»"));
        }
        let mgr = self.ensure_manager()?;
        mgr.register(hk).map_err(|e| format!("не удалось зарегистрировать (возможно, занята другим приложением): {e}"))?;
        taken.insert(hk.id(), name.to_string());
        self.ids.lock().unwrap().insert(hk.id(), name.to_string());
        self.registered.push(hk);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use global_hotkey::hotkey::{Code, Modifiers};

    #[test]
    fn parses_typical_combos() {
        let hk = parse_hotkey("Ctrl+Alt+1").unwrap();
        assert_eq!(hk.key, Code::Digit1);
        assert_eq!(hk.mods, Modifiers::CONTROL | Modifiers::ALT);
        let hk = parse_hotkey(" ctrl + shift + F5 ").unwrap();
        assert_eq!(hk.key, Code::F5);
        assert_eq!(hk.mods, Modifiers::CONTROL | Modifiers::SHIFT);
        assert_eq!(parse_hotkey("Super+Q").unwrap().mods, Modifiers::META);
        assert_eq!(parse_hotkey("Alt+A").unwrap().key, Code::KeyA);
    }

    #[test]
    fn same_combo_same_id() {
        assert_eq!(parse_hotkey("Ctrl+Alt+1").unwrap().id(), parse_hotkey("alt+control+1").unwrap().id());
    }

    #[test]
    fn rejects_bad_strings() {
        assert!(parse_hotkey("").is_err());
        assert!(parse_hotkey("   ").is_err());
        assert!(parse_hotkey("F5").is_err(), "без модификатора");
        assert!(parse_hotkey("Ctrl+").is_err());
        assert!(parse_hotkey("Ctrl+Alt+Nope").is_err());
        assert!(parse_hotkey("Ctrl+A+B").is_err());
        assert!(parse_hotkey("Ctrl+Alt").is_err(), "нет основной клавиши");
    }
}
