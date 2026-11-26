//! Preset switching engine. Runs on its own thread so the UI/tray never block.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{self, AppEntry, Config, State};
use crate::logging::{self, Level};
use crate::platform;

const LOG_LINES: usize = 300;
const POLL: Duration = Duration::from_millis(300);
/// A process whose windows are all gone but which is still alive after this long
/// is assumed to have minimized itself to the tray and is killed early.
const WINDOWLESS_GRACE: Duration = Duration::from_secs(3);

pub enum Cmd {
    /// Имя пресета и номер поколения (см. `Engine::switch`).
    Switch(String, u64),
}

#[derive(Default)]
pub struct Shared {
    pub active: Option<String>,
    pub busy: bool,
    pub status: String,
    pub log: VecDeque<String>,
}

#[derive(Clone)]
pub struct Engine {
    tx: Sender<Cmd>,
    pub shared: Arc<Mutex<Shared>>,
    config: Arc<Mutex<Config>>,
    /// Номер последней команды переключения: всё, что старше, считается отменённым.
    generation: Arc<AtomicU64>,
    /// Делает «увеличить номер + отправить» атомарным относительно других потоков.
    submit: Arc<Mutex<()>>,
    /// Отложенное выключение ПК (отменяемое).
    pub shutdown: Arc<crate::api::ShutdownTimer>,
}

impl Engine {
    pub fn start(cfg: Config, on_change: impl Fn() + Send + 'static) -> Self {
        logging::rotate_on_start();
        logging::set_filter(cfg.settings.log_level);
        let state = config::load_state();
        let shared = Arc::new(Mutex::new(Shared { active: state.active_preset, ..Default::default() }));
        let config = Arc::new(Mutex::new(cfg));
        let (tx, rx) = channel();
        let generation = Arc::new(AtomicU64::new(0));
        {
            let shared = shared.clone();
            let config = config.clone();
            let generation = generation.clone();
            thread::Builder::new()
                .name("preset-engine".into())
                .spawn(move || worker(rx, shared, config, generation, on_change))
                .expect("spawn engine thread");
        }
        let log_shared = shared.clone();
        let log: crate::api::LogFn = Arc::new(move |level: Level, m: &str| push_log(&log_shared, level, m.to_string()));
        let power_log = log.clone();
        let shutdown = Arc::new(crate::api::ShutdownTimer::new(
            Arc::new(move |action| crate::platform::run_power(action, &power_log)),
            log,
        ));
        Self { tx, shared, config, generation, submit: Arc::new(Mutex::new(())), shutdown }
    }

    pub fn switch(&self, preset: &str) {
        let _guard = self.submit.lock().unwrap();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.tx.send(Cmd::Switch(preset.to_string(), generation));
    }

    pub fn update_config(&self, cfg: Config) {
        logging::set_filter(cfg.settings.log_level);
        *self.config.lock().unwrap() = cfg;
    }

    /// Имена пресетов из актуального конфига движка, в порядке конфига.
    pub fn preset_names(&self) -> Vec<String> {
        self.config.lock().unwrap().presets.iter().map(|p| p.name.clone()).collect()
    }

    pub fn log_at(&self, level: Level, msg: impl Into<String>) {
        push_log(&self.shared, level, msg.into());
    }

    pub fn log(&self, msg: impl Into<String>) {
        self.log_at(Level::Info, msg);
    }

    pub fn log_error(&self, msg: impl Into<String>) {
        self.log_at(Level::Error, msg);
    }
}

// ---------------------------------------------------------------------------
// Planning (pure, unit-tested)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CloseGroup {
    pub process: String,
    pub display: String,
    pub close_command: Option<String>,
    pub force_kill: bool,
}

#[derive(Debug)]
pub struct Plan {
    pub close: Vec<CloseGroup>,
    pub launch: Vec<AppEntry>,
}

/// Decides what to close and what to start when switching to `target`.
///
/// Closed: enabled apps from *other* presets with `close_on_switch`, whose
/// process isn't used by any enabled app of the target preset.
pub fn plan_switch(cfg: &Config, target: &str) -> Option<Plan> {
    let target_preset = cfg.presets.iter().find(|p| p.name == target)?;
    let launch: Vec<AppEntry> = target_preset.apps.iter().filter(|a| a.enabled).cloned().collect();
    let keep: HashSet<String> = launch.iter().filter_map(|a| a.effective_process_name()).collect();

    let mut seen = HashSet::new();
    let mut close = Vec::new();
    for preset in cfg.presets.iter().filter(|p| p.name != target) {
        for app in preset.apps.iter().filter(|a| a.enabled && a.close_on_switch) {
            let Some(proc_name) = app.effective_process_name() else { continue };
            if keep.contains(&proc_name) || platform::is_protected(&proc_name) || !seen.insert(proc_name.clone()) {
                continue;
            }
            let cc = config::expand_env(app.close_command.trim());
            close.push(CloseGroup {
                process: proc_name,
                display: app.display_name(),
                close_command: (!cc.is_empty()).then_some(cc),
                force_kill: app.force_kill,
            });
        }
    }
    Some(Plan { close, launch })
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Берёт следующую команду и схлопывает все уже ожидающие в очереди до последней.
/// Возвращает команду и число отброшенных.
fn next_cmd(rx: &Receiver<Cmd>) -> Option<(Cmd, usize)> {
    let mut cmd = rx.recv().ok()?;
    let mut skipped = 0;
    while let Ok(newer) = rx.try_recv() {
        cmd = newer;
        skipped += 1;
    }
    Some((cmd, skipped))
}

/// Спит `dur`, но просыпается раньше, если `cancelled()`. Возвращает true, если отменено.
fn sleep_unless(dur: Duration, cancelled: &dyn Fn() -> bool) -> bool {
    let end = Instant::now() + dur;
    loop {
        if cancelled() {
            return true;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        thread::sleep(left.min(Duration::from_millis(50)));
    }
}

fn worker(
    rx: Receiver<Cmd>,
    shared: Arc<Mutex<Shared>>,
    config: Arc<Mutex<Config>>,
    generation: Arc<AtomicU64>,
    on_change: impl Fn(),
) {
    while let Some((cmd, skipped)) = next_cmd(&rx) {
        match cmd {
            Cmd::Switch(name, gen_id) => {
                if skipped > 0 {
                    push_log(&shared, Level::Info, format!("Пропущено устаревших команд переключения: {skipped}"));
                }
                let cancelled = || generation.load(Ordering::SeqCst) != gen_id;
                let cfg = config.lock().unwrap().clone();
                {
                    let mut s = shared.lock().unwrap();
                    s.busy = true;
                    s.status = format!("Переключаюсь на «{name}»…");
                }
                on_change();
                let ok = run_switch(&cfg, &name, &shared, &on_change, &cancelled);
                {
                    let mut s = shared.lock().unwrap();
                    s.busy = false;
                    s.status.clear();
                    if ok {
                        s.active = Some(name.clone());
                    }
                }
                if ok
                    && let Err(e) = config::save_state(&State { active_preset: Some(name) }) {
                        push_log(&shared, Level::Error, format!("Не удалось сохранить состояние: {e}"));
                    }
                on_change();
            }
        }
    }
}

fn set_status(shared: &Mutex<Shared>, on_change: &dyn Fn(), status: String) {
    shared.lock().unwrap().status = status;
    on_change();
}

/// Выполняет переключение. Возвращает true, только если оно дошло до конца
/// (прерванное новой командой переключение возвращает false).
fn run_switch(
    cfg: &Config,
    target: &str,
    shared: &Mutex<Shared>,
    on_change: &dyn Fn(),
    cancelled: &dyn Fn() -> bool,
) -> bool {
    let Some(plan) = plan_switch(cfg, target) else {
        push_log(shared, Level::Error, format!("Пресет «{target}» не найден"));
        return false;
    };
    push_log(shared, Level::Info, format!("── Пресет «{target}» ──"));
    let abort = |shared: &Mutex<Shared>| {
        push_log(shared, Level::Info, format!("Переключение на «{target}» прервано: пришла новая команда"));
        false
    };

    if !close_programs(&plan.close, cfg.settings.close_timeout_secs, shared, on_change, cancelled) {
        return abort(shared);
    }

    for app in &plan.launch {
        if cancelled() {
            return abort(shared);
        }
        let name = app.display_name();
        let Some(running) = snapshot_has(app) else {
            push_log(shared, Level::Error, format!("{name}: не удалось получить список процессов, запуск пропущен"));
            continue;
        };
        if running && !app.launch_if_running {
            push_log(shared, Level::Info, format!("{name}: уже запущен, пропускаю"));
            continue;
        }
        set_status(shared, on_change, format!("Запускаю {name}…"));
        let path = config::expand_env(app.path.trim());
        let args = config::expand_env(&app.args);
        let dir = config::expand_env(app.working_dir.trim());
        match platform::launch(&path, &args, &dir) {
            Ok(()) => push_log(shared, Level::Info, format!("{name}: запущен")),
            Err(e) => push_log(shared, Level::Error, format!("{name}: ошибка запуска — {e}")),
        }
        if app.delay_ms > 0 && sleep_unless(Duration::from_millis(app.delay_ms), cancelled) {
            return abort(shared);
        }
    }
    if cancelled() {
        return abort(shared);
    }
    push_log(shared, Level::Info, format!("Готово: «{target}»"));
    true
}

/// `None` — список процессов получить не удалось (тогда не запускаем, чтобы не плодить дубликаты).
fn snapshot_has(app: &AppEntry) -> Option<bool> {
    match app.effective_process_name() {
        Some(p) => platform::snapshot().map(|s| s.contains_key(&p)),
        None => Some(false),
    }
}

struct Pending<'a> {
    group: &'a CloseGroup,
    windowless_since: Option<Instant>,
}

/// Возвращает false, если процесс прерван новой командой переключения.
fn close_programs(
    groups: &[CloseGroup],
    timeout_secs: u64,
    shared: &Mutex<Shared>,
    on_change: &dyn Fn(),
    cancelled: &dyn Fn() -> bool,
) -> bool {
    let Some(snap) = platform::snapshot() else {
        push_log(shared, Level::Error, "Не удалось получить список процессов: закрытие программ пропущено".to_string());
        return true;
    };
    let mut pending: Vec<Pending> = Vec::new();

    for g in groups {
        if cancelled() {
            return false;
        }
        let Some(pids) = snap.get(&g.process) else { continue };
        set_status(shared, on_change, format!("Закрываю {}…", g.display));
        let asked = if let Some(cmd) = &g.close_command {
            match platform::run_close_command(cmd) {
                Ok(()) => true,
                Err(e) => {
                    push_log(shared, Level::Error, format!("{}: команда закрытия не выполнилась — {e}", g.display));
                    platform::post_close_to_windows(pids) > 0
                }
            }
        } else {
            platform::post_close_to_windows(pids) > 0
        };

        if asked {
            push_log(shared, Level::Debug, format!("{}: закрываю", g.display));
            pending.push(Pending { group: g, windowless_since: None });
        } else if g.force_kill {
            // No windows (sits in the tray) — nothing to ask politely.
            let n = pids.iter().filter(|&&p| platform::kill(p)).count();
            push_log(shared, Level::Info, format!("{}: нет окон, завершён принудительно ({n})", g.display));
        } else {
            push_log(shared, Level::Info, format!("{}: нет окон, принудительное завершение отключено — оставляю", g.display));
        }
    }

    if pending.is_empty() {
        return true;
    }

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let Some(snap) = platform::snapshot() else {
            // Состояние неизвестно: не считаем программы закрытыми и не завершаем их.
            if Instant::now() >= deadline {
                break;
            }
            if sleep_unless(POLL, cancelled) {
                return false;
            }
            continue;
        };
        pending.retain_mut(|p| {
            let Some(pids) = snap.get(&p.group.process) else {
                push_log(shared, Level::Debug, format!("{}: закрыт", p.group.display));
                return false;
            };
            // Early kill for apps that just hid into the tray on WM_CLOSE.
            if p.group.force_kill && p.group.close_command.is_none() {
                if platform::visible_window_count(pids) == 0 {
                    let since = *p.windowless_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= WINDOWLESS_GRACE {
                        let n = pids.iter().filter(|&&pid| platform::kill(pid)).count();
                        push_log(shared, Level::Info, format!("{}: свернулся в трей, завершён принудительно ({n})", p.group.display));
                        return false;
                    }
                } else {
                    p.windowless_since = None;
                }
            }
            true
        });
        if pending.is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            break;
        }
        if sleep_unless(POLL, cancelled) {
            return false;
        }
    }

    if cancelled() {
        return false;
    }
    let Some(snap) = platform::snapshot() else {
        push_log(shared, Level::Error, "Не удалось получить список процессов: принудительное завершение пропущено".to_string());
        return true;
    };
    for p in pending {
        let Some(pids) = snap.get(&p.group.process) else { continue };
        if p.group.force_kill {
            let n = pids.iter().filter(|&&pid| platform::kill(pid)).count();
            push_log(shared, Level::Info, format!("{}: не закрылся за {timeout_secs} с, завершён принудительно ({n})", p.group.display));
        } else {
            push_log(shared, Level::Info, format!("{}: не закрылся за {timeout_secs} с, оставляю", p.group.display));
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

pub fn push_log(shared: &Mutex<Shared>, level: Level, msg: String) {
    if !logging::enabled(level) {
        return;
    }
    let line = logging::format_line(&logging::timestamp(), level, &msg);
    #[cfg(not(test))]
    logging::append_to_file(&line);
    let mut s = shared.lock().unwrap();
    s.log.push_back(line);
    while s.log.len() > LOG_LINES {
        s.log.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Preset;

    // Имя процесса задаётся явно: вывод имени из пути зависит от ОС.
    fn app(name: &str, proc_: &str) -> AppEntry {
        AppEntry { name: name.into(), path: format!("apps/{proc_}"), process_name: proc_.into(), ..Default::default() }
    }

    /// Процесс, который на текущей ОС защищён от закрытия.
    #[cfg(windows)]
    const PROTECTED_PROC: &str = "explorer.exe";
    #[cfg(target_os = "macos")]
    const PROTECTED_PROC: &str = "finder";

    fn cfg() -> Config {
        let mut tg_games = app("Telegram", "Telegram.exe");
        tg_games.close_on_switch = true;
        let mut explorer = app("Explorer", PROTECTED_PROC);
        explorer.close_on_switch = true;
        let mut keep_open = app("Music", "music.exe");
        keep_open.close_on_switch = false;
        let mut disabled = app("Old", "old.exe");
        disabled.enabled = false;
        Config {
            settings: Default::default(),
            presets: vec![
                Preset::new("Работа", vec![app("Rider", "rider64.exe"), app("Telegram", "telegram.exe"), explorer, keep_open, disabled]),
                Preset::new("Игры", vec![app("Steam", "steam.exe"), tg_games]),
            ],
        }
    }

    #[test]
    fn closes_other_presets_but_keeps_shared_apps() {
        let plan = plan_switch(&cfg(), "Игры").unwrap();
        let closed: Vec<_> = plan.close.iter().map(|g| g.process.as_str()).collect();
        assert_eq!(closed, vec!["rider64.exe"]); // telegram shared, explorer protected, music opted out, old disabled
        let launched: Vec<_> = plan.launch.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(launched, vec!["Steam", "Telegram"]);
    }

    #[test]
    fn launch_order_preserved_and_disabled_skipped() {
        let plan = plan_switch(&cfg(), "Работа").unwrap();
        let launched: Vec<_> = plan.launch.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(launched, vec!["Rider", "Telegram", "Explorer", "Music"]);
        let closed: Vec<_> = plan.close.iter().map(|g| g.process.as_str()).collect();
        assert_eq!(closed, vec!["steam.exe"]);
    }

    fn log_text(shared: &Mutex<Shared>) -> String {
        shared.lock().unwrap().log.iter().cloned().collect::<Vec<_>>().join("
")
    }

    fn nonexistent_app(name: &str, delay_ms: u64) -> AppEntry {
        #[cfg(windows)]
        let path = format!("C:\\definitely-missing-dir\\{name}.exe");
        #[cfg(target_os = "macos")]
        let path = format!("/definitely-missing-dir/{name}");
        AppEntry { name: name.into(), path, delay_ms, ..Default::default() }
    }

    #[test]
    fn queue_collapses_to_last_command() {
        let (tx, rx) = channel();
        for (n, g) in [("A", 1), ("B", 2), ("C", 3)] {
            tx.send(Cmd::Switch(n.into(), g)).unwrap();
        }
        let (Cmd::Switch(name, g), skipped) = next_cmd(&rx).unwrap();
        assert_eq!((name.as_str(), g, skipped), ("C", 3, 2));
        drop(tx);
        assert!(next_cmd(&rx).is_none());
    }

    #[test]
    fn new_command_during_delay_aborts_running_switch() {
        use std::sync::atomic::AtomicBool;
        let cfg = Config {
            settings: Default::default(),
            presets: vec![Preset::new("P1", vec![nonexistent_app("FirstApp", 30_000), nonexistent_app("SecondApp", 0)])],
        };
        let shared = Arc::new(Mutex::new(Shared::default()));
        let flag = Arc::new(AtomicBool::new(false));
        let f2 = flag.clone();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            f2.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let ok = run_switch(&cfg, "P1", &shared, &|| {}, &|| flag.load(Ordering::SeqCst));
        t.join().unwrap();
        assert!(!ok);
        assert!(started.elapsed() < Duration::from_secs(5), "delay was not interrupted");
        let log = log_text(&shared);
        assert!(log.contains("FirstApp"), "{log}");
        assert!(!log.contains("SecondApp"), "{log}");
        assert!(log.contains("прервано"), "{log}");
    }

    #[test]
    fn uncancelled_switch_completes() {
        let cfg = Config {
            settings: Default::default(),
            presets: vec![Preset::new("P1", vec![nonexistent_app("FirstApp", 0), nonexistent_app("SecondApp", 0)])],
        };
        let shared = Mutex::new(Shared::default());
        assert!(run_switch(&cfg, "P1", &shared, &|| {}, &|| false));
        assert!(log_text(&shared).contains("SecondApp"));
    }

    #[test]
    fn platform_protects_system_shell() {
        assert!(platform::is_protected(PROTECTED_PROC));
        assert!(!platform::is_protected("definitely-not-protected"));
    }

    #[test]
    fn unknown_preset() {
        assert!(plan_switch(&cfg(), "Нет такого").is_none());
    }
}
