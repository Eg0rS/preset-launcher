#![cfg_attr(windows, windows_subsystem = "windows")]

mod api;
mod config;
mod engine;
mod hotkeys;
mod icon_art;
mod cli;
mod logging;
mod platform;
mod theme;
mod tray;
mod ui;

use std::sync::mpsc::channel;

use eframe::egui;

const TITLE: &str = "Preset Launcher";

/// Системный шрифт, если файл есть: (имя, путь, приоритет в семействе Proportional).
fn system_fonts() -> Vec<(&'static str, String, egui::epaint::text::FontPriority)> {
    use egui::epaint::text::FontPriority::Lowest;
    #[cfg(windows)]
    use egui::epaint::text::FontPriority::Highest;
    #[cfg(windows)]
    {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        vec![
            // Segoe UI for a native look (falls back to egui's bundled fonts, which also cover Cyrillic).
            ("segoe-ui", format!(r"{windir}\Fonts\segoeui.ttf"), Highest),
            // Segoe UI lacks symbols like ▶ ✕ — use Segoe UI Symbol as a last-resort fallback.
            ("segoe-ui-symbol", format!(r"{windir}\Fonts\seguisym.ttf"), Lowest),
        ]
    }
    #[cfg(target_os = "macos")]
    {
        // Основной шрифт — встроенный egui (покрывает кириллицу); системный SF недоступен как .ttf.
        // Apple Symbols — запасной для символов вроде ▶ ✕.
        vec![("apple-symbols", "/System/Library/Fonts/Apple Symbols.ttf".to_string(), Lowest)]
    }
}

fn setup_style(ctx: &egui::Context) {
    for (name, path, priority) in system_fonts() {
        if let Ok(bytes) = std::fs::read(&path) {
            ctx.add_font(egui::epaint::text::FontInsert::new(
                name,
                egui::FontData::from_owned(bytes),
                vec![egui::epaint::text::InsertFontFamily { family: egui::FontFamily::Proportional, priority }],
            ));
        }
    }
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
        s.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(14.5));
        s.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(14.5));
        s.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(19.0));
    });
}

fn main() -> eframe::Result {
    let args = cli::collect_args().unwrap_or_else(|e| std::process::exit(cli::usage_error(&e)));
    let cli::GuiArgs { autostart, minimized, preset: preset_arg } = match cli::route(&args) {
        cli::Route::Gui(g) => g,
        cli::Route::Cli(cmd) => std::process::exit(cli::execute(cmd)),
        cli::Route::Usage(e) => std::process::exit(cli::usage_error(&e)),
    };

    let before_lock = std::time::SystemTime::now();
    let instance = match platform::acquire() {
        platform::Acquire::Got(i) => i,
        platform::Acquire::Failed(msg) => {
            // Без защиты от второго экземпляра не запускаемся; консоли нет — пишем в журнал.
            let msg = format!("Не удалось защититься от второго запуска: {msg}");
            logging::append_to_file(&logging::format_line(&logging::timestamp(), logging::Level::Error, &msg));
            eprintln!("{msg}");
            std::process::exit(1);
        }
        platform::Acquire::AlreadyRunning => {
        // Already running: hand over the request (switch preset or just show the window).
        if let Some(name) = &preset_arg {
            let _ = config::write_pending_switch(name);
        }
        platform::signal_existing();
        return Ok(());
        }
    };
    // Drop a stale request left over from a previous run (but keep one written after our start).
    config::discard_pending_switch_older_than(before_lock);

    let (mut cfg, load_error) = config::load_config();
    if cfg.settings.api_enabled && cfg.settings.api_token.trim().is_empty() {
        cfg.settings.api_token = config::generate_token();
        let _ = config::save_config(&cfg);
    }

    let icon = egui::IconData { rgba: icon_art::render_icon_rgba(64), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([960.0, 720.0])
            .with_min_inner_size([720.0, 480.0])
            .with_icon(icon)
            .with_visible(!minimized),
        ..Default::default()
    };

    eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| {
            setup_style(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx, cfg.settings.theme, theme::accent_color(&cfg.settings.accent));

            let (tx, rx) = channel();
            tray::install_handlers(tx.clone(), cc.egui_ctx.clone());
            let hotkeys = hotkeys::Hotkeys::new(tx.clone(), cc.egui_ctx.clone());
            {
                let ctx = cc.egui_ctx.clone();
                instance.watch(move || {
                    let ev = match config::take_pending_switch() {
                        Some(name) => tray::UiEvent::Switch(name),
                        None => tray::UiEvent::Show,
                    };
                    let _ = tx.send(ev);
                    ctx.request_repaint();
                });
            }
            // Блокировка single-instance (mutex на Windows, flock на macOS) должна жить весь
            // процесс: `Instance` намеренно не освобождается.
            Box::leak(Box::new(instance));

            let repaint_ctx = cc.egui_ctx.clone();
            let engine = engine::Engine::start(cfg.clone(), move || repaint_ctx.request_repaint());
            if let Some(err) = load_error {
                engine.log_error(err);
            }

            let mut api = api::ApiManager::new(engine.clone());
            api.apply(&cfg.settings);

            let tray = match tray::Tray::new() {
                Ok(t) => Some(t),
                Err(e) => {
                    engine.log_error(format!("Не удалось создать значок в трее: {e}"));
                    None
                }
            };

            if let Some(name) = &preset_arg {
                engine.switch(name);
            } else if autostart {
                let startup = cfg.settings.startup_preset.trim();
                if !startup.is_empty() {
                    engine.switch(startup);
                }
            }

            Ok(Box::new(ui::LauncherApp::new(cfg, engine, api, tray, hotkeys, rx, minimized)))
        }),
    )
}
