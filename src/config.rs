//! Config model + persistence (`%APPDATA%\PresetLauncher\config.toml` on Windows,
//! `~/Library/Application Support/PresetLauncher/config.toml` on macOS).
//!
//! `config.toml` is edited by the UI; `state.toml` holds runtime state
//! (the currently active preset) and is written only by the engine, so the
//! two never overwrite each other.

use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Path, PathBuf};

pub use crate::logging::Level as LogLevel;

/// Тема оформления. Неизвестное значение в файле трактуется как `system`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for ThemeChoice {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self::parse(&s).unwrap_or_default())
    }
}

/// Акцент по умолчанию (зелёный маркер активного пресета).
pub const DEFAULT_ACCENT: &str = "#3CC88A";

/// Разбирает `#RRGGBB` (регистр любой, `#` обязателен).
pub fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().strip_prefix('#')?;
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

pub fn format_hex_color(c: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

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
    /// Тема окна: "system" | "light" | "dark".
    pub theme: ThemeChoice,
    /// Акцентный цвет "#RRGGBB".
    pub accent: String,
    /// Глобальные горячие клавиши пресетов (по умолчанию выключены).
    pub hotkeys_enabled: bool,
    /// Уровень журнала: "error" | "info" | "debug".
    pub log_level: LogLevel,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            close_timeout_secs: 10,
            startup_preset: String::new(),
            api_enabled: false,
            api_port: 47800,
            api_token: String::new(),
            theme: ThemeChoice::System,
            accent: DEFAULT_ACCENT.into(),
            hotkeys_enabled: false,
            log_level: LogLevel::Info,
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
    /// Короткая метка (эмодзи или 1–2 символа). Пусто — без иконки.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub icon: String,
    /// Цвет метки "#RRGGBB". Пусто — без цвета.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub color: String,
    /// Глобальная горячая клавиша, например "Ctrl+Alt+1". Пусто — нет.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub hotkey: String,
    #[serde(rename = "app")]
    pub apps: Vec<AppEntry>,
}

impl Preset {
    pub fn new(name: impl Into<String>, apps: Vec<AppEntry>) -> Self {
        Self { name: name.into(), apps, ..Default::default() }
    }
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
        crate::platform::process_name_from_path(&path)
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
    crate::platform::config_base_dir().join("PresetLauncher")
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

/// Удаляет запрос переключения, оставшийся от прошлого запуска: только если файл старше `t`
/// (по mtime); свежий запрос второго экземпляра сохраняется.
pub fn discard_pending_switch_older_than(t: std::time::SystemTime) {
    let path = pending_switch_path();
    if std::fs::metadata(&path).and_then(|m| m.modified()).is_ok_and(|m| m < t) {
        let _ = std::fs::remove_file(path);
    }
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
                let _ = write_private(&backup, text.as_bytes());
                (
                    default_config(),
                    Some(format!(
                        "Не удалось прочитать {}: {}. Копия сохранена в {}, загружены пресеты по умолчанию.",
                        path.display(),
                        toml_error_brief(&text, &e),
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

/// Читает конфиг без побочных эффектов (для CLI): ничего не создаёт и не переименовывает.
pub fn read_config() -> Result<Config, String> {
    let path = config_path();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("не удалось прочитать конфиг {} ({e}): запустите приложение хотя бы один раз", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("конфиг {} повреждён: {}", path.display(), toml_error_brief(&text, &e)))
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

/// Краткое описание ошибки разбора TOML: сообщение, строка и столбец. Исходный текст строки
/// в сообщение не попадает (в конфиге может быть токен API).
pub fn toml_error_brief(text: &str, e: &toml::de::Error) -> String {
    match e.span() {
        Some(span) => {
            let start = span.start.min(text.len());
            let before = &text.as_bytes()[..start];
            let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
            let col = start - before.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) + 1;
            format!("{} (строка {line}, столбец {col})", redact(e.message()))
        }
        None => redact(e.message()),
    }
}

/// serde вставляет в сообщения само значение (`invalid type: string "…"`): прячем всё в кавычках.
fn redact(msg: &str) -> String {
    let mut out = String::new();
    let mut close: Option<char> = None;
    for c in msg.chars() {
        match close {
            Some(q) if c == q => close = None,
            Some(_) => {}
            None if c == '"' || c == '`' => {
                out.push(c);
                out.push('…');
                out.push(c);
                close = Some(c);
            }
            None => out.push(c),
        }
    }
    out
}

/// Записывает файл; на Unix создаёт его с правами 0600 (в конфиге лежит токен).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)?.write_all(bytes)
}

fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    write_private(&tmp, contents.as_bytes())?;
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

#[cfg(windows)]
fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_default()
}

#[cfg(windows)]
fn first_existing(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|p| p.is_file())
}

#[cfg(windows)]
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

#[cfg(windows)]
fn entry(name: &str, path: Option<PathBuf>, fallback: &str) -> AppEntry {
    AppEntry {
        enabled: path.is_some(),
        name: name.into(),
        path: path.map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| fallback.into()),
        ..Default::default()
    }
}

#[cfg(windows)]
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

    let steam_reg = steam_exe_from_registry();
    let steam_path = steam_reg.or_else(|| first_existing([pf86.join("Steam\\steam.exe"), pf.join("Steam\\steam.exe")]));
    let mut steam = entry("Steam", steam_path, "C:\\Program Files (x86)\\Steam\\steam.exe");
    steam.close_command = format!("\"{}\" -shutdown", steam.path);
    steam.delay_ms = 1000;

    Config {
        settings: Settings::default(),
        presets: vec![
            Preset::new("Работа", vec![rider, datagrip, browser, telegram, terminal]),
            Preset::new("Игры", vec![discord, steam]),
        ],
    }
}

#[cfg(target_os = "macos")]
pub fn default_config() -> Config {
    let app = |name: &str, path: &str| AppEntry {
        enabled: Path::new(path).exists(),
        name: name.into(),
        path: path.into(),
        ..Default::default()
    };
    let mut steam = app("Steam", "/Applications/Steam.app");
    steam.delay_ms = 1000;
    Config {
        settings: Settings::default(),
        presets: vec![
            Preset::new(
                "Работа",
                vec![
                    app("Visual Studio Code", "/Applications/Visual Studio Code.app"),
                    app("Safari", "/Applications/Safari.app"),
                    app("Telegram", "/Applications/Telegram.app"),
                    app("Терминал", "/System/Applications/Utilities/Terminal.app"),
                ],
            ),
            Preset::new("Игры", vec![app("Discord", "/Applications/Discord.app"), steam]),
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
        #[cfg(windows)]
        {
            let e = AppEntry { path: "C:\\x\\Foo.EXE".into(), ..Default::default() };
            assert_eq!(e.effective_process_name().as_deref(), Some("foo.exe"));
        }
        let e = AppEntry { process_name: "Bar.exe".into(), ..Default::default() };
        assert_eq!(e.effective_process_name().as_deref(), Some("bar.exe"));
        let u = AppEntry { path: "steam://open/games".into(), ..Default::default() };
        assert_eq!(u.effective_process_name(), None);
    }

    #[test]
    fn old_030_config_loads_with_defaults() {
        let old = r#"
[settings]
close_timeout_secs = 7
startup_preset = "Работа"
api_enabled = true
api_port = 47801
api_token = "abc"

[[preset]]
name = "Работа"

[[preset.app]]
name = "Rider"
path = "rider64.exe"
"#;
        let cfg: Config = toml::from_str(old).unwrap();
        assert_eq!(cfg.settings.close_timeout_secs, 7);
        assert_eq!(cfg.settings.api_token, "abc");
        assert_eq!(cfg.settings.theme, ThemeChoice::System);
        assert_eq!(cfg.settings.accent, DEFAULT_ACCENT);
        assert!(!cfg.settings.hotkeys_enabled);
        assert_eq!(cfg.settings.log_level, LogLevel::Info);
        let p = &cfg.presets[0];
        assert_eq!((p.icon.as_str(), p.color.as_str(), p.hotkey.as_str()), ("", "", ""));
        assert_eq!(p.apps[0].name, "Rider");
        let empty: Config = toml::from_str("").unwrap();
        assert_eq!(empty.settings, Settings::default());
    }

    #[test]
    fn new_fields_roundtrip_and_lenient_enums() {
        let text = r##"
[settings]
theme = "dark"
accent = "#112233"
hotkeys_enabled = true
log_level = "debug"

[[preset]]
name = "A"
icon = "🎮"
color = "#FF0000"
hotkey = "Ctrl+Alt+1"
"##;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.settings.theme, ThemeChoice::Dark);
        assert_eq!(cfg.settings.log_level, LogLevel::Debug);
        assert_eq!(cfg.presets[0].hotkey, "Ctrl+Alt+1");
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(cfg, back);
        let bad: Config = toml::from_str("[settings]\ntheme = \"neon\"\nlog_level = \"trace\"\n").unwrap();
        assert_eq!(bad.settings.theme, ThemeChoice::System);
        assert_eq!(bad.settings.log_level, LogLevel::Info);
    }

    #[test]
    fn toml_errors_do_not_leak_values() {
        for text in ["[settings]\napi_port = \"supersecret\"\n", "[settings]\napi_token = 987654321\n", "[settings]\napi_token = \"supersecret\n"] {
            let e = toml::from_str::<Config>(text).unwrap_err();
            let brief = toml_error_brief(text, &e);
            assert!(!brief.contains("supersecret") && !brief.contains("987654321"), "{brief}");
            assert!(brief.contains("строка 2"), "{brief}");
        }
    }

    #[test]
    fn hex_colors() {
        assert_eq!(parse_hex_color("#FF8000"), Some([255, 128, 0]));
        assert_eq!(parse_hex_color(" #3cc88a "), Some([0x3c, 0xc8, 0x8a]));
        assert_eq!(parse_hex_color("3cc88a"), None);
        assert_eq!(parse_hex_color("#fff"), None);
        assert_eq!(parse_hex_color("#GG0000"), None);
        assert_eq!(parse_hex_color("#+12345"), None);
        assert_eq!(parse_hex_color("#ÿÿÿ"), None);
        assert_eq!(format_hex_color([1, 2, 255]), "#0102FF");
        assert_eq!(parse_hex_color(DEFAULT_ACCENT), Some([0x3c, 0xc8, 0x8a]));
    }

    #[test]
    fn config_roundtrip() {
        let cfg = default_config();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }
}
