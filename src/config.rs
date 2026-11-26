//! Config model + persistence (%APPDATA%\PresetLauncher\config.toml).
//!
//! `config.toml` is edited by the UI; `state.toml` holds runtime state
//! (the currently active preset) and is written only by the engine, so the
//! two never overwrite each other.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub settings: Settings,
    #[serde(rename = "preset")]
    pub presets: Vec<Preset>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// How long to wait for programs to close gracefully before killing them.
    pub close_timeout_secs: u64,
    /// Preset applied automatically when started with `--autostart` (empty = none).
    pub startup_preset: String,
    /// Локальный HTTP API для удалённого управления (мост к голосовому ассистенту).
    pub api_enabled: bool,
    pub api_port: u16,
    /// Bearer-токен API. Пусто — генерируется при включении API.
    pub api_token: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            close_timeout_secs: 10,
            startup_preset: String::new(),
            api_enabled: false,
            api_port: 47800,
            api_token: String::new(),
        }
    }
}

/// Случайный токен: 32 байта в hex.
pub fn generate_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("системный генератор случайных чисел недоступен");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Preset {
    pub name: String,
    #[serde(rename = "app")]
    pub apps: Vec<AppEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppEntry {
    pub enabled: bool,
    pub name: String,
    /// Path to an .exe, or anything ShellExecute can open (.lnk, URL, steam://...).
    /// %ENV% variables are expanded.
    pub path: String,
    /// Raw argument string, passed as-is (quote paths with spaces yourself).
    pub args: String,
    pub working_dir: String,
    /// Process image name used to detect/close the program, e.g. `Discord.exe`.
    /// Empty = file name of `path`.
    pub process_name: String,
    /// Pause after starting this program, before the next one.
    pub delay_ms: u64,
    /// Start even if the process is already running (e.g. browser with work tabs).
    pub launch_if_running: bool,
    /// Close this program when switching to a preset that doesn't contain it.
    pub close_on_switch: bool,
    /// Kill the process if it hasn't exited after the graceful-close timeout.
    pub force_kill: bool,
    /// Optional command line used instead of WM_CLOSE (e.g. `"C:\Steam\steam.exe" -shutdown`).
    pub close_command: String,
}

impl Default for AppEntry {
    fn default() -> Self {
        Self {
            enabled: true,
            name: String::new(),
            path: String::new(),
            args: String::new(),
            working_dir: String::new(),
            process_name: String::new(),
            delay_ms: 500,
            launch_if_running: false,
            close_on_switch: true,
            force_kill: true,
            close_command: String::new(),
        }
    }
}

impl AppEntry {
    /// Lower-cased process image name used for matching running processes.
    pub fn effective_process_name(&self) -> Option<String> {
        let explicit = self.process_name.trim();
        if !explicit.is_empty() {
            return Some(explicit.to_lowercase());
        }
        let path = expand_env(self.path.trim());
        if path.contains("://") {
            return None; // URL — no process to track
        }
        let file = Path::new(&path).file_name()?.to_string_lossy().to_lowercase();
        if file.ends_with(".exe") { Some(file) } else { None }
    }

    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        Path::new(&self.path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "(без имени)".into())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub active_preset: Option<String>,
}


pub fn app_dir() -> PathBuf {
    // Override for testing / portable setups.
    if let Some(dir) = std::env::var_os("PRESET_LAUNCHER_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("PresetLauncher")
}

fn config_path() -> PathBuf {
    app_dir().join("config.toml")
}

fn state_path() -> PathBuf {
    app_dir().join("state.toml")
}

fn pending_switch_path() -> PathBuf {
    app_dir().join("pending_switch.txt")
}

/// Hand-off of a `--preset NAME` request from a second instance to the running one.
pub fn write_pending_switch(name: &str) -> anyhow::Result<()> {
    write_atomic(&pending_switch_path(), name)
}

pub fn take_pending_switch() -> Option<String> {
    let path = pending_switch_path();
    let name = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let name = name.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Loads config; on first run writes a default config built from detected programs.
/// Returns an error message (and a fallback config) if the file exists but is invalid —
/// in that case the broken file is backed up rather than silently overwritten.
pub fn load_config() -> (Config, Option<String>) {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(cfg) => (cfg, None),
            Err(e) => {
                let backup = path.with_extension("toml.broken");
                let _ = std::fs::copy(&path, &backup);
                (
                    default_config(),
                    Some(format!(
                        "Не удалось прочитать {}: {e}. Копия сохранена в {}, загружены пресеты по умолчанию.",
                        path.display(),
                        backup.display()
                    )),
                )
            }
        },
        Err(_) => {
            let cfg = default_config();
            let err = save_config(&cfg).err().map(|e| format!("Не удалось сохранить конфиг: {e}"));
            (cfg, err)
        }
    }
}

pub fn save_config(cfg: &Config) -> anyhow::Result<()> {
    write_atomic(&config_path(), &toml::to_string_pretty(cfg)?)
}

pub fn load_state() -> State {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_state(state: &State) -> anyhow::Result<()> {
    write_atomic(&state_path(), &toml::to_string_pretty(state)?)
}

fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Expands `%VAR%` references using the process environment. Unknown vars are left as-is.
pub fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(val) => out.push_str(&val),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Default presets, built from programs found on this machine.
// ---------------------------------------------------------------------------

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_default()
}

fn first_existing(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|p| p.is_file())
}

/// Finds a JetBrains IDE launcher (Toolbox or standalone installs).
fn find_jetbrains(dir_prefix: &str, exe: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    let roots = [
        env_path("LOCALAPPDATA").join("JetBrains").join("Installations"),
        env_path("LOCALAPPDATA").join("Programs"),
        env_path("ProgramFiles").join("JetBrains"),
    ];
    for root in roots {
        if let Ok(rd) = std::fs::read_dir(&root) {
            let mut dirs: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().to_lowercase().contains(&dir_prefix.to_lowercase()))
                        .unwrap_or(false)
                })
                .collect();
            dirs.sort();
            for d in dirs.into_iter().rev() {
                candidates.push(d.join("bin").join(exe));
            }
        }
    }
    first_existing(candidates)
}

#[cfg(windows)]
fn steam_exe_from_registry() -> Option<PathBuf> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::w;
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let res = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Valve\\Steam"),
            w!("SteamExe"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if res.is_err() {
        return None;
    }
    let chars = (len as usize / 2).saturating_sub(1);
    let s = String::from_utf16_lossy(&buf[..chars.min(buf.len())]).replace('/', "\\");
    let p = PathBuf::from(s);
    p.is_file().then_some(p)
}

fn entry(name: &str, path: Option<PathBuf>, fallback: &str) -> AppEntry {
    AppEntry {
        enabled: path.is_some(),
        name: name.into(),
        path: path.map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| fallback.into()),
        ..Default::default()
    }
}

pub fn default_config() -> Config {
    let local = env_path("LOCALAPPDATA");
    let roaming = env_path("APPDATA");
    let pf = env_path("ProgramFiles");
    let pf86 = env_path("ProgramFiles(x86)");

    let rider = entry("Rider", find_jetbrains("rider", "rider64.exe"), "rider64.exe");
    let datagrip = entry("DataGrip", find_jetbrains("datagrip", "datagrip64.exe"), "datagrip64.exe");

    let browser_path = first_existing([
        pf.join("Google\\Chrome\\Application\\chrome.exe"),
        pf86.join("Google\\Chrome\\Application\\chrome.exe"),
        local.join("Google\\Chrome\\Application\\chrome.exe"),
        pf.join("Yandex\\YandexBrowser\\Application\\browser.exe"),
        local.join("Yandex\\YandexBrowser\\Application\\browser.exe"),
        pf86.join("Microsoft\\Edge\\Application\\msedge.exe"),
    ]);
    let mut browser = entry("Браузер (рабочие вкладки)", browser_path, "chrome.exe");
    browser.args = "--new-window https://github.com https://mail.google.com".into();
    browser.launch_if_running = true;

    let telegram = entry(
        "Telegram",
        first_existing([
            roaming.join("Telegram Desktop\\Telegram.exe"),
            local.join("Telegram Desktop\\Telegram.exe"),
            pf.join("Telegram Desktop\\Telegram.exe"),
        ]),
        "%APPDATA%\\Telegram Desktop\\Telegram.exe",
    );

    let terminal = AppEntry {
        name: "Терминал".into(),
        path: "wt.exe".into(),
        args: "-d \"%USERPROFILE%\"".into(),
        process_name: "WindowsTerminal.exe".into(),
        ..Default::default()
    };

    let discord_update = local.join("Discord\\Update.exe");
    let discord = AppEntry {
        enabled: discord_update.is_file(),
        name: "Discord".into(),
        path: "%LOCALAPPDATA%\\Discord\\Update.exe".into(),
        args: "--processStart Discord.exe".into(),
        process_name: "Discord.exe".into(),
        ..Default::default()
    };

    #[cfg(windows)]
    let steam_reg = steam_exe_from_registry();
    #[cfg(not(windows))]
    let steam_reg: Option<PathBuf> = None;
    let steam_path = steam_reg.or_else(|| first_existing([pf86.join("Steam\\steam.exe"), pf.join("Steam\\steam.exe")]));
    let mut steam = entry("Steam", steam_path, "C:\\Program Files (x86)\\Steam\\steam.exe");
    steam.close_command = format!("\"{}\" -shutdown", steam.path);
    steam.delay_ms = 1000;

    Config {
        settings: Settings::default(),
        presets: vec![
            Preset { name: "Работа".into(), apps: vec![rider, datagrip, browser, telegram, terminal] },
            Preset { name: "Игры".into(), apps: vec![discord, steam] },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_env_basic() {
        unsafe { std::env::set_var("PL_TEST_VAR", "X") };
        assert_eq!(expand_env("a%PL_TEST_VAR%b"), "aXb");
        assert_eq!(expand_env("%NOPE_NOT_SET_123%"), "%NOPE_NOT_SET_123%");
        assert_eq!(expand_env("100%"), "100%");
        assert_eq!(expand_env("%%"), "%%");
    }

    #[test]
    fn process_name_derivation() {
        let mut e = AppEntry { path: "C:\\x\\Foo.EXE".into(), ..Default::default() };
        assert_eq!(e.effective_process_name().as_deref(), Some("foo.exe"));
        e.process_name = "Bar.exe".into();
        assert_eq!(e.effective_process_name().as_deref(), Some("bar.exe"));
        let u = AppEntry { path: "steam://open/games".into(), ..Default::default() };
        assert_eq!(u.effective_process_name(), None);
    }

    #[test]
    fn config_roundtrip() {
        let cfg = default_config();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }
}
