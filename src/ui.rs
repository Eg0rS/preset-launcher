//! egui window: preset editor, activation, settings, log.

use std::collections::HashSet;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::api::{self, ApiManager};
use crate::config::{self, AppEntry, Config, Preset, State, ThemeChoice};
use crate::engine::Engine;
use crate::hotkeys::{self, Hotkeys};
use crate::logging::{self, Level};
use crate::theme;
use crate::tray::{Tray, UiEvent};
use crate::winproc;

const GREEN: Color32 = Color32::from_rgb(0x3c, 0xc8, 0x8a);
const AMBER: Color32 = Color32::from_rgb(0xe8, 0xa8, 0x3a);
const RED: Color32 = Color32::from_rgb(0xe0, 0x5a, 0x4e);

enum AppAction {
    Up(usize),
    Down(usize),
    Remove(usize),
}

pub struct LauncherApp {
    cfg: Config,
    saved: Config,
    last_save_attempt: Option<Config>,
    save_error: Option<String>,
    engine: Engine,
    api: ApiManager,
    show_token: bool,
    local_ip: Option<std::net::IpAddr>,
    tray: Option<Tray>,
    hotkeys: Hotkeys,
    /// Редактируемый текст хоткея выбранного пресета (в конфиг попадает по мере ввода валидной строки).
    hotkey_buf: String,
    /// Применённые к egui (тема, акцент), чтобы не дёргать контекст каждый кадр.
    applied_look: Option<(ThemeChoice, String)>,
    accent: Color32,
    events: Receiver<UiEvent>,
    selected: usize,
    running: HashSet<String>,
    running_checked: Option<Instant>,
    /// eframe always shows the window after the first painted frame, so a
    /// `--minimized` start hides it again during the first frames.
    hide_frames_left: u8,
}

impl LauncherApp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        engine: Engine,
        api: ApiManager,
        tray: Option<Tray>,
        hotkeys: Hotkeys,
        events: Receiver<UiEvent>,
        start_hidden: bool,
    ) -> Self {
        let mut app = Self {
            hide_frames_left: if start_hidden { 3 } else { 0 },
            saved: cfg.clone(),
            cfg,
            last_save_attempt: None,
            save_error: None,
            engine,
            api,
            show_token: false,
            local_ip: api::local_ip(),
            tray,
            hotkeys,
            hotkey_buf: String::new(),
            applied_look: None,
            accent: theme::accent_color(config::DEFAULT_ACCENT),
            events,
            selected: 0,
            running: HashSet::new(),
            running_checked: None,
        };
        if let Some(active) = app.engine.shared.lock().unwrap().active.clone()
            && let Some(i) = app.cfg.presets.iter().position(|p| p.name == active) {
                app.selected = i;
            }
        app.hotkeys.apply(&app.cfg.settings, &app.cfg.presets, &app.engine);
        app.sync_tray();
        app
    }

    fn sync_tray(&mut self) {
        let (active, busy) = {
            let s = self.engine.shared.lock().unwrap();
            (s.active.clone(), s.busy)
        };
        let names = self.saved.presets.iter().map(|p| (p.name.clone(), p.icon.clone())).collect();
        if let Some(t) = &mut self.tray {
            t.sync(names, active, busy);
        }
    }

    fn show_window(ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn quit(&mut self) {
        self.autosave();
        // Dropping the tray icon removes it from the notification area immediately.
        self.tray = None;
        std::process::exit(0);
    }

    fn autosave(&mut self) {
        if self.cfg == self.saved || self.last_save_attempt.as_ref() == Some(&self.cfg) {
            return;
        }
        self.last_save_attempt = Some(self.cfg.clone());
        match config::save_config(&self.cfg) {
            Ok(()) => {
                self.saved = self.cfg.clone();
                self.save_error = None;
                self.engine.update_config(self.cfg.clone());
                self.api.apply(&self.cfg.settings);
                self.hotkeys.apply(&self.cfg.settings, &self.cfg.presets, &self.engine);
            }
            Err(e) => self.save_error = Some(format!("Не удалось сохранить конфиг: {e}")),
        }
    }

    fn refresh_running(&mut self) {
        let stale = self.running_checked.map(|t| t.elapsed() > Duration::from_secs(2)).unwrap_or(true);
        if stale {
            self.running = winproc::snapshot().into_keys().collect();
            self.running_checked = Some(Instant::now());
        }
    }

    /// Keeps active/startup preset references valid when a preset gets renamed.
    fn on_preset_renamed(&mut self, old: &str, new: &str) {
        if self.cfg.settings.startup_preset == old {
            self.cfg.settings.startup_preset = new.to_string();
        }
        let mut s = self.engine.shared.lock().unwrap();
        if s.active.as_deref() == Some(old) {
            s.active = Some(new.to_string());
            let _ = config::save_state(&State { active_preset: s.active.clone() });
        }
    }

    // ------------------------------------------------------------------ panels

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let (active, busy, status) = {
            let s = self.engine.shared.lock().unwrap();
            (s.active.clone(), s.busy, s.status.clone())
        };
        ui.horizontal(|ui| {
            ui.heading("Preset Launcher");
            ui.add_space(12.0);
            match &active {
                Some(a) => {
                    ui.label("Активный пресет:");
                    ui.label(RichText::new(a).strong().color(self.accent));
                }
                None => {
                    ui.label(RichText::new("Пресет не выбран").weak());
                }
            }
            if busy {
                ui.add_space(8.0);
                ui.add(egui::Spinner::new());
                ui.label(status);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Выход").on_hover_text("Закрыть Preset Launcher (программы пресета не трогаются)").clicked() {
                    self.quit();
                }
                if ui.button("Свернуть в трей").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
            });
        });
    }

    fn presets_panel(&mut self, ui: &mut egui::Ui) {
        let (active, busy) = {
            let s = self.engine.shared.lock().unwrap();
            (s.active.clone(), s.busy)
        };
        ui.add_space(4.0);
        ui.label(RichText::new("Пресеты").strong());
        ui.add_space(4.0);

        let mut activate: Option<String> = None;
        egui::ScrollArea::vertical().id_salt("presets_scroll").max_height(ui.available_height() - 80.0).show(ui, |ui| {
            for (i, p) in self.cfg.presets.iter().enumerate() {
                let is_active = active.as_deref() == Some(p.name.as_str());
                let label = preset_label(ui, p, is_active, self.accent);
                let resp = ui
                    .add_sized([ui.available_width(), 26.0], egui::Button::selectable(self.selected == i, label))
                    .on_hover_text("Двойной клик — включить пресет");
                if resp.clicked() {
                    self.selected = i;
                }
                if resp.double_clicked() && !busy {
                    activate = Some(p.name.clone());
                }
            }
        });
        if let Some(name) = activate {
            self.engine.switch(&name);
        }

        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if ui.button("+ Новый").clicked() {
                let mut n = 1;
                let name = loop {
                    let candidate = if n == 1 { "Новый пресет".to_string() } else { format!("Новый пресет {n}") };
                    if !self.cfg.presets.iter().any(|p| p.name == candidate) {
                        break candidate;
                    }
                    n += 1;
                };
                self.cfg.presets.push(Preset::new(name, Vec::new()));
                self.selected = self.cfg.presets.len() - 1;
            }
            let has_sel = self.selected < self.cfg.presets.len();
            if ui.add_enabled(has_sel, egui::Button::new("Копия")).clicked() {
                let mut copy = self.cfg.presets[self.selected].clone();
                copy.name = format!("{} (копия)", copy.name);
                self.cfg.presets.insert(self.selected + 1, copy);
                self.selected += 1;
            }
            if ui.add_enabled(self.selected > 0 && has_sel, egui::Button::new("↑")).clicked() {
                self.cfg.presets.swap(self.selected, self.selected - 1);
                self.selected -= 1;
            }
            if ui.add_enabled(has_sel && self.selected + 1 < self.cfg.presets.len(), egui::Button::new("↓")).clicked() {
                self.cfg.presets.swap(self.selected, self.selected + 1);
                self.selected += 1;
            }
        });
    }

    fn preset_editor(&mut self, ui: &mut egui::Ui) {
        if self.cfg.presets.is_empty() {
            ui.centered_and_justified(|ui| ui.label("Создайте пресет слева"));
            return;
        }
        self.selected = self.selected.min(self.cfg.presets.len() - 1);
        let idx = self.selected;
        let busy = self.engine.shared.lock().unwrap().busy;
        self.refresh_running();

        // Header: name + activate + delete
        let old_name = self.cfg.presets[idx].name.clone();
        let mut delete = false;
        ui.horizontal(|ui| {
            ui.label("Название:");
            ui.add(egui::TextEdit::singleline(&mut self.cfg.presets[idx].name).desired_width(220.0));
            let run = egui::Button::new(RichText::new("▶  Включить пресет").strong()).min_size(egui::vec2(160.0, 28.0));
            if ui.add_enabled(!busy, run).clicked() {
                self.autosave();
                self.engine.switch(&self.cfg.presets[idx].name);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(RichText::new("Удалить пресет").color(RED))
                    .on_hover_text("Удаляется сразу, без подтверждения")
                    .clicked()
                {
                    delete = true;
                }
            });
        });
        let new_name = self.cfg.presets[idx].name.clone();
        if new_name != old_name {
            self.on_preset_renamed(&old_name, &new_name);
        }
        let dup = self.cfg.presets.iter().filter(|p| p.name == new_name).count() > 1;
        if new_name.trim().is_empty() || dup {
            ui.label(RichText::new("Название должно быть непустым и уникальным").color(AMBER));
        }
        if delete {
            self.cfg.presets.remove(idx);
            self.selected = idx.saturating_sub(1);
            return;
        }
        self.preset_look_row(ui, idx);

        ui.label(
            RichText::new(
                "Программы запускаются сверху вниз. При переключении на другой пресет программы этого пресета, \
                 которых нет в новом, закрываются.",
            )
            .weak(),
        );
        ui.add_space(6.0);

        let mut action: Option<AppAction> = None;
        let count = self.cfg.presets[idx].apps.len();
        egui::ScrollArea::vertical().id_salt("apps_scroll").auto_shrink([false, false]).show(ui, |ui| {
            for i in 0..count {
                let app = &mut self.cfg.presets[idx].apps[i];
                let running = app.effective_process_name().map(|p| self.running.contains(&p)).unwrap_or(false);
                egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if let Some(a) = app_card(ui, app, i, count, running, idx) {
                        action = Some(a);
                    }
                });
                ui.add_space(4.0);
            }

            ui.horizontal(|ui| {
                if ui.button("+ Добавить программу…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_title("Выберите программу")
                        .add_filter("Программы и ярлыки", &["exe", "lnk", "bat", "cmd", "url"])
                        .add_filter("Все файлы", &["*"])
                        .pick_file()
                    {
                        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        self.cfg.presets[idx].apps.push(AppEntry {
                            name,
                            path: path.to_string_lossy().into_owned(),
                            ..Default::default()
                        });
                    }
                if ui.button("+ Пустая запись").clicked() {
                    self.cfg.presets[idx].apps.push(AppEntry { name: "Новая программа".into(), ..Default::default() });
                }
            });
        });

        let apps = &mut self.cfg.presets[idx].apps;
        match action {
            Some(AppAction::Up(i)) if i > 0 => apps.swap(i, i - 1),
            Some(AppAction::Down(i)) if i + 1 < apps.len() => apps.swap(i, i + 1),
            Some(AppAction::Remove(i)) => {
                apps.remove(i);
            }
            _ => {}
        }
    }

    /// Иконка, цвет и горячая клавиша выбранного пресета.
    fn preset_look_row(&mut self, ui: &mut egui::Ui, idx: usize) {
        let hotkeys_on = self.cfg.settings.hotkeys_enabled;
        let accent = self.accent;
        ui.horizontal_wrapped(|ui| {
            let p = &mut self.cfg.presets[idx];
            ui.label("Иконка:");
            ui.add(egui::TextEdit::singleline(&mut p.icon).desired_width(48.0).char_limit(8).hint_text("🎮"))
                .on_hover_text("Эмодзи или 1–2 символа; показывается в списке и в меню трея");
            ui.add_space(8.0);
            ui.label("Цвет:");
            match config::parse_hex_color(&p.color) {
                Some(mut rgb) => {
                    if egui::color_picker::color_edit_button_srgb(ui, &mut rgb).changed() {
                        p.color = config::format_hex_color(rgb);
                    }
                    if ui.small_button("✕").on_hover_text("Убрать цвет").clicked() {
                        p.color.clear();
                    }
                }
                None => {
                    if ui.small_button("+ цвет").clicked() {
                        let c = accent;
                        p.color = config::format_hex_color([c.r(), c.g(), c.b()]);
                    }
                }
            }
            ui.add_space(8.0);
            ui.label("Горячая клавиша:");
        });
        // Текст хоткея правится в буфере; в конфиг уходит пустая или корректная строка,
        // чтобы промежуточный ввод не регистрировался и не засорял журнал.
        let id = ui.make_persistent_id(("hotkey_edit", idx));
        if !ui.memory(|m| m.has_focus(id)) {
            self.hotkey_buf = self.cfg.presets[idx].hotkey.clone();
        }
        ui.horizontal(|ui| {
            let resp = ui
                .add(egui::TextEdit::singleline(&mut self.hotkey_buf).id(id).desired_width(180.0).hint_text("Ctrl+Alt+1"))
                .on_hover_text("Например Ctrl+Alt+1, Ctrl+Shift+F5. Нужен хотя бы один модификатор.");
            if resp.changed() {
                let t = self.hotkey_buf.trim();
                if t.is_empty() || hotkeys::parse_hotkey(t).is_ok() {
                    self.cfg.presets[idx].hotkey = t.to_string();
                }
            }
            if !hotkeys_on {
                ui.label(RichText::new("выключено: включите «Горячие клавиши» внизу").weak().small());
            }
        });
        let typed = self.hotkey_buf.trim();
        let err = if hotkeys_on && !typed.is_empty() {
            hotkeys::parse_hotkey(typed)
                .err()
                .or_else(|| self.hotkeys.error_for(&self.cfg.presets[idx].name).map(str::to_string))
        } else {
            None
        };
        if let Some(e) = err {
            ui.label(RichText::new(format!("Горячая клавиша: {e}")).color(RED));
        }
        ui.add_space(2.0);
    }

    fn api_block(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Удалённое управление").strong());
        ui.horizontal(|ui| {
            if ui.checkbox(&mut self.cfg.settings.api_enabled, "Включить API").changed()
                && self.cfg.settings.api_enabled
                && self.cfg.settings.api_token.trim().is_empty()
            {
                self.cfg.settings.api_token = config::generate_token();
            }
            ui.add_space(12.0);
            ui.label("Порт:");
            ui.add(egui::DragValue::new(&mut self.cfg.settings.api_port).range(1024..=65535));
            ui.add_space(12.0);
            let port = self.cfg.settings.api_port;
            let addr = match self.local_ip {
                Some(ip) => format!("http://{ip}:{port}"),
                None => format!("порт {port}"),
            };
            if self.api.is_running() {
                ui.label(RichText::new(format!("● {addr}")).color(GREEN));
            } else {
                ui.label(RichText::new(addr).weak());
            }
        });
        ui.horizontal(|ui| {
            ui.label("Токен:");
            let token = self.cfg.settings.api_token.clone();
            let shown = if self.show_token { token.clone() } else { "•".repeat(token.chars().count().min(32)) };
            ui.label(RichText::new(if shown.is_empty() { "—".to_string() } else { shown }).monospace());
            if ui.button(if self.show_token { "Скрыть" } else { "Показать" }).clicked() {
                self.show_token = !self.show_token;
            }
            if ui.add_enabled(!token.is_empty(), egui::Button::new("Скопировать")).clicked() {
                ui.ctx().copy_text(token);
            }
            if ui.button("Новый токен").on_hover_text("Старый токен перестанет работать сразу").clicked() {
                self.cfg.settings.api_token = config::generate_token();
            }
        });
        if let Some(e) = &self.api.error {
            ui.label(RichText::new(e).color(RED));
        }
    }

    fn look_block(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Внешний вид").strong());
            ui.add_space(8.0);
            ui.label("Тема:");
            let label = |t: ThemeChoice| match t {
                ThemeChoice::System => "Системная",
                ThemeChoice::Light => "Светлая",
                ThemeChoice::Dark => "Тёмная",
            };
            egui::ComboBox::from_id_salt("theme").selected_text(label(self.cfg.settings.theme)).show_ui(ui, |ui| {
                for t in ThemeChoice::ALL {
                    ui.selectable_value(&mut self.cfg.settings.theme, t, label(t));
                }
            });
            ui.add_space(8.0);
            ui.label("Акцент:");
            let c = self.accent;
            let mut rgb = [c.r(), c.g(), c.b()];
            if egui::color_picker::color_edit_button_srgb(ui, &mut rgb).changed() {
                self.cfg.settings.accent = config::format_hex_color(rgb);
            }
            if ui.small_button("Сброс").on_hover_text("Вернуть акцент по умолчанию").clicked() {
                self.cfg.settings.accent = config::DEFAULT_ACCENT.to_string();
            }
            ui.add_space(16.0);
            ui.checkbox(&mut self.cfg.settings.hotkeys_enabled, "Горячие клавиши пресетов")
                .on_hover_text("Глобальные сочетания клавиш, заданные у пресетов. По умолчанию выключено.");
        });
    }

    fn bottom_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Ждать закрытия программ:");
            ui.add(egui::DragValue::new(&mut self.cfg.settings.close_timeout_secs).range(1..=120).suffix(" с"));
            ui.add_space(16.0);
            ui.label("При автозапуске включать:");
            let current =
                if self.cfg.settings.startup_preset.is_empty() { "—".to_string() } else { self.cfg.settings.startup_preset.clone() };
            egui::ComboBox::from_id_salt("startup_preset").selected_text(current).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.cfg.settings.startup_preset, String::new(), "— ничего");
                for p in &self.cfg.presets {
                    ui.selectable_value(&mut self.cfg.settings.startup_preset, p.name.clone(), &p.name);
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Папка конфига").on_hover_text(config::app_dir().display().to_string()).clicked() {
                    let _ = winproc::launch("explorer.exe", &format!("\"{}\"", config::app_dir().display()), "");
                }
            });
        });
        if let Some(err) = &self.save_error {
            ui.label(RichText::new(err).color(RED));
        }
        ui.separator();
        self.api_block(ui);
        ui.separator();
        self.look_block(ui);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Журнал").strong());
            ui.add_space(8.0);
            ui.label("Уровень:");
            egui::ComboBox::from_id_salt("log_level").selected_text(self.cfg.settings.log_level.as_str()).show_ui(ui, |ui| {
                for l in Level::ALL {
                    ui.selectable_value(&mut self.cfg.settings.log_level, l, l.as_str());
                }
            });
            if ui.button("Открыть журнал").on_hover_text(logging::log_path().display().to_string()).clicked() {
                let path = logging::ensure_log_file();
                if let Err(e) = winproc::launch(&path.to_string_lossy(), "", "") {
                    self.engine.log_error(format!("Не удалось открыть журнал: {e}"));
                }
            }
        });
        let log: Vec<String> = self.engine.shared.lock().unwrap().log.iter().cloned().collect();
        egui::ScrollArea::vertical()
            .id_salt("log_scroll")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for line in &log {
                    ui.label(RichText::new(line).monospace().size(12.0));
                }
            });
    }
}

/// Строка пресета в списке: маркер активного, цветная метка, иконка, название.
fn preset_label(ui: &egui::Ui, p: &Preset, is_active: bool, accent: Color32) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let font = egui::TextStyle::Button.resolve(ui.style());
    let base = ui.visuals().text_color();
    let fmt = |color| TextFormat { font_id: font.clone(), color, ..Default::default() };
    let mut job = LayoutJob::default();
    if is_active {
        job.append("● ", 0.0, fmt(accent));
    } else {
        job.append("   ", 0.0, fmt(base));
    }
    if let Some([r, g, b]) = config::parse_hex_color(&p.color) {
        job.append("■ ", 0.0, fmt(Color32::from_rgb(r, g, b)));
    }
    let icon = p.icon.trim();
    if !icon.is_empty() {
        job.append(&format!("{icon} "), 0.0, fmt(base));
    }
    job.append(&p.name, 0.0, fmt(if is_active { accent } else { base }));
    job
}

/// One program row. Returns a reorder/remove action if requested.
fn app_card(ui: &mut egui::Ui, app: &mut AppEntry, i: usize, count: usize, running: bool, preset_idx: usize) -> Option<AppAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        ui.checkbox(&mut app.enabled, "").on_hover_text("Включено в пресет");
        ui.label(RichText::new(format!("{}.", i + 1)).weak());
        ui.add(egui::TextEdit::singleline(&mut app.name).desired_width(200.0).hint_text("Название"));
        if running {
            ui.label(RichText::new("● запущено").color(GREEN).small());
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("✕").on_hover_text("Удалить из пресета").clicked() {
                action = Some(AppAction::Remove(i));
            }
            if ui.add_enabled(i + 1 < count, egui::Button::new("↓").small()).clicked() {
                action = Some(AppAction::Down(i));
            }
            if ui.add_enabled(i > 0, egui::Button::new("↑").small()).clicked() {
                action = Some(AppAction::Up(i));
            }
        });
    });

    egui::Grid::new(("app_grid", preset_idx, i)).num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
        ui.label("Программа");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.path)
                    .desired_width((ui.available_width() - 70.0).max(120.0))
                    .hint_text("C:\\…\\program.exe, ярлык или URL"),
            );
            if ui.button("Обзор").clicked()
                && let Some(p) = rfd::FileDialog::new().add_filter("Программы", &["exe", "lnk", "bat", "cmd"]).pick_file() {
                    app.path = p.to_string_lossy().into_owned();
                }
        });
        ui.end_row();

        ui.label("Аргументы");
        ui.add(
            egui::TextEdit::singleline(&mut app.args)
                .desired_width(f32::INFINITY)
                .hint_text("например: --new-window https://… или -d C:\\work"),
        );
        ui.end_row();

        ui.label("Рабочая папка");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.working_dir)
                    .desired_width((ui.available_width() - 70.0).max(120.0))
                    .hint_text("необязательно"),
            );
            if ui.button("Папка").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder() {
                    app.working_dir = p.to_string_lossy().into_owned();
                }
        });
        ui.end_row();
    });

    egui::CollapsingHeader::new("Дополнительно").id_salt(("app_more", preset_idx, i)).show(ui, |ui| {
        egui::Grid::new(("app_more_grid", preset_idx, i)).num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
            ui.label("Имя процесса");
            let hint = app.effective_process_name().unwrap_or_else(|| "напр. Discord.exe".into());
            ui.add(egui::TextEdit::singleline(&mut app.process_name).desired_width(220.0).hint_text(hint))
                .on_hover_text("По нему программа определяется как запущенная и закрывается. Пусто — имя файла из пути.");
            ui.end_row();

            ui.label("Пауза после запуска");
            ui.add(egui::DragValue::new(&mut app.delay_ms).range(0..=120_000).speed(50).suffix(" мс"));
            ui.end_row();

            ui.label("Команда закрытия");
            ui.add(
                egui::TextEdit::singleline(&mut app.close_command)
                    .desired_width(f32::INFINITY)
                    .hint_text("вместо закрытия окон, напр. \"C:\\Steam\\steam.exe\" -shutdown"),
            );
            ui.end_row();
        });
        ui.checkbox(&mut app.launch_if_running, "Запускать, даже если уже запущена")
            .on_hover_text("Полезно для браузера: откроет новое окно с вкладками");
        ui.checkbox(&mut app.close_on_switch, "Закрывать при переключении на другой пресет");
        ui.checkbox(&mut app.force_kill, "Завершать принудительно, если не закрылась сама")
            .on_hover_text("Также для программ, которые прячутся в трей вместо закрытия");
    });
    action
}

impl eframe::App for LauncherApp {
    /// Runs every frame and also while the window is hidden (on request_repaint),
    /// so tray events are handled without the window being visible.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.hide_frames_left > 0 {
            self.hide_frames_left -= 1;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.request_repaint();
        }
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                UiEvent::Show => {
                    self.hide_frames_left = 0;
                    Self::show_window(ctx)
                }
                UiEvent::Switch(name) => self.engine.switch(&name),
                UiEvent::Hotkey(name) => {
                    self.engine.log(format!("Горячая клавиша: переключение на «{name}»"));
                    self.engine.switch(&name);
                }
                UiEvent::Quit => self.quit(),
            }
        }
        let look = (self.cfg.settings.theme, self.cfg.settings.accent.clone());
        if self.applied_look.as_ref() != Some(&look) {
            self.accent = theme::accent_color(&look.1);
            theme::apply(ctx, look.0, self.accent);
            self.applied_look = Some(look);
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            // The window's X button hides to tray; "Выход" really quits.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        self.sync_tray();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("bottom").resizable(true).default_size(250.0).show(ui, |ui| self.bottom_panel(ui));
        egui::Panel::left("presets").resizable(true).default_size(200.0).show(ui, |ui| self.presets_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| self.preset_editor(ui));

        self.autosave();
        // Keep the "running" markers fresh while the window is visible.
        ui.ctx().request_repaint_after(Duration::from_secs(2));
    }
}
