#![windows_subsystem = "windows"]

mod api;
mod config;
mod engine;
mod hotkeys;
mod icon_art;
mod logging;
mod single;
mod theme;
mod tray;
mod ui;
mod winproc;
mod winsys;

use std::sync::mpsc::channel;

use eframe::egui;

const TITLE: &str = "Preset Launcher";

fn setup_style(ctx: &egui::Context) {
    // Segoe UI for a native look (falls back to egui's bundled fonts, which also cover Cyrillic).
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    if let Ok(bytes) = std::fs::read(format!("{windir}\\Fonts\\segoeui.ttf")) {
        ctx.add_font(egui::epaint::text::FontInsert::new(
            "segoe-ui",
            egui::FontData::from_owned(bytes),
            vec![egui::epaint::text::InsertFontFamily {
                family: egui::FontFamily::Proportional,
                priority: egui::epaint::text::FontPriority::Highest,
            }],
        ));
    }
    // Segoe UI lacks symbols like ▶ ✕ — use Segoe UI Symbol as a last-resort fallback.
    if let Ok(bytes) = std::fs::read(format!("{windir}\\Fonts\\seguisym.ttf")) {
        ctx.add_font(egui::epaint::text::FontInsert::new(
            "segoe-ui-symbol",
            egui::FontData::from_owned(bytes),
            vec![egui::epaint::text::InsertFontFamily {
                family: egui::FontFamily::Proportional,
                priority: egui::epaint::text::FontPriority::Lowest,
            }],
        ));
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let autostart = args.iter().any(|a| a == "--autostart");
    let preset_arg = args.iter().position(|a| a == "--preset").and_then(|i| args.get(i + 1)).cloned();
    let minimized = autostart || preset_arg.is_some() || args.iter().any(|a| a == "--minimized");

    let Some(instance) = single::acquire() else {
        // Already running: hand over the request (switch preset or just show the window).
        if let Some(name) = &preset_arg {
            let _ = config::write_pending_switch(name);
        }
        single::signal_existing();
        return Ok(());
    };
    // Drop a stale request left over from a previous run.
    let _ = config::take_pending_switch();

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
            // `Instance` holds raw HANDLEs without Drop: the singleton mutex stays
            // owned for the whole process lifetime.

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
