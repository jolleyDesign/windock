//! The settings window (`windock.exe --settings`). It edits config.toml; the running dock
//! reloads the file, so every change shows up live.

use crate::config::{self, Color, Config, IndicatorStyle, MonitorSel, Pinned, Position, Theme};
use crate::hotkeys;
use crate::shell;
use eframe::egui;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{FOS_FILEMUSTEXIST, FOS_NODEREFERENCELINKS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow};
use windows::core::w;

const TITLE: &str = "Windock Settings";
/// Wait this long after the last edit before writing, so dragging a slider doesn't hammer the disk.
const SAVE_DELAY: Duration = Duration::from_millis(250);

pub fn run() {
    unsafe {
        let _instance = CreateMutexW(None, true, w!("Local\\WindockSettings"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(existing) = FindWindowW(None, w!("Windock Settings")) {
                let _ = ShowWindow(existing, SW_RESTORE);
                let _ = SetForegroundWindow(existing);
            }
            return;
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_icon(egui::IconData { rgba: crate::logo::rgba(64), width: 64, height: 64 })
            .with_inner_size([580.0, 680.0])
            .with_min_inner_size([480.0, 420.0]),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native(TITLE, options, Box::new(|cc| Ok(Box::new(Settings::new(&cc.egui_ctx))))) {
        crate::log!("settings window failed: {e}");
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    General,
    Appearance,
    Apps,
}

struct Settings {
    path: PathBuf,
    cfg: Config,
    /// The file as it was when the window opened, for Revert.
    original: String,
    /// The file as we last wrote or read it; anything else means someone else changed it.
    known: String,
    /// The file couldn't be parsed; editing is disabled so we don't overwrite it with defaults.
    broken: Option<String>,
    status: Option<String>,
    pending: Option<Instant>,
    last_poll: Instant,
    tab: Tab,
    monitors: Vec<String>,
    names: HashMap<String, String>,
}

impl Settings {
    fn new(ctx: &egui::Context) -> Self {
        install_fonts(ctx);
        ctx.set_theme(egui::ThemePreference::System);
        ctx.all_styles_mut(|style| {
            use egui::{FontId, TextStyle};
            style.text_styles.insert(TextStyle::Body, FontId::proportional(14.5));
            style.text_styles.insert(TextStyle::Button, FontId::proportional(14.5));
            style.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
            style.spacing.item_spacing.y = 6.0;
        });
        let path = config::config_path();
        let (parsed, text) = config::load(&path);
        let monitors = crate::app::monitors()
            .iter()
            .enumerate()
            .map(|(i, (_, mi))| {
                let r = mi.rcMonitor;
                let primary = if mi.dwFlags & 1 != 0 { ", primary" } else { "" };
                format!("Monitor {i} ({}×{}{primary})", r.right - r.left, r.bottom - r.top)
            })
            .collect();
        let mut s = Self {
            path,
            cfg: Config::default(),
            original: text.clone(),
            known: text,
            broken: None,
            status: None,
            pending: None,
            last_poll: Instant::now(),
            tab: Tab::General,
            monitors,
            names: HashMap::new(),
        };
        s.adopt(parsed);
        s
    }

    fn adopt(&mut self, parsed: Result<Config, String>) {
        match parsed {
            Ok(cfg) => {
                self.cfg = cfg;
                self.broken = None;
            }
            Err(e) => self.broken = Some(e.lines().next().unwrap_or(&e).to_string()),
        }
    }

    fn changed(&mut self) {
        self.pending = Some(Instant::now());
    }

    fn save_now(&mut self) {
        self.pending = None;
        if self.broken.is_some() {
            return;
        }
        match config::save(&self.path, &self.cfg) {
            Ok(text) => {
                self.known = text;
                self.status = None;
            }
            Err(e) => self.status = Some(format!("Couldn't save: {e}")),
        }
    }

    /// Picks up edits made elsewhere (by hand, or the dock saving a drag-and-drop).
    fn poll_file(&mut self) {
        if self.pending.is_some() || self.last_poll.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_poll = Instant::now();
        let (parsed, text) = config::load(&self.path);
        if text != self.known {
            self.known = text;
            self.adopt(parsed);
        }
    }

    fn revert(&mut self) {
        self.pending = None;
        match std::fs::write(&self.path, &self.original) {
            Ok(()) => {
                self.known = self.original.clone();
                self.adopt(config::parse(&self.original));
                self.status = Some("Reverted to how it was when this window opened".into());
            }
            Err(e) => self.status = Some(format!("Couldn't revert: {e}")),
        }
    }

    fn default_name(&mut self, path: &str) -> String {
        self.names
            .entry(path.to_string())
            .or_insert_with(|| {
                let p = shell::expand_env(path);
                if p.to_lowercase().ends_with(".lnk") {
                    std::path::Path::new(&p).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(p)
                } else {
                    shell::friendly_name(&p)
                }
            })
            .clone()
    }

    // ------------------------------------------------------------------ tabs

    fn general(&mut self, ui: &mut egui::Ui) -> bool {
        let d = &mut self.cfg.dock;
        let mut c = false;
        section(ui, "Placement");
        grid(ui, "placement", |ui| {
            ui.label("Screen edge");
            ui.horizontal(|ui| {
                for (p, label) in [(Position::Bottom, "Bottom"), (Position::Left, "Left"), (Position::Right, "Right"), (Position::Top, "Top")] {
                    c |= ui.selectable_value(&mut d.position, p, label).changed();
                }
            });
            ui.end_row();

            ui.label("Monitor");
            let current = match &d.monitor {
                MonitorSel::Index(i) => self.monitors.get(*i).cloned().unwrap_or_else(|| format!("Monitor {i}")),
                MonitorSel::Name(_) => "Primary monitor".to_string(),
            };
            egui::ComboBox::from_id_salt("monitor").selected_text(current).width(240.0).show_ui(ui, |ui| {
                c |= ui.selectable_value(&mut d.monitor, MonitorSel::Name("primary".into()), "Primary monitor").changed();
                for (i, name) in self.monitors.iter().enumerate() {
                    c |= ui.selectable_value(&mut d.monitor, MonitorSel::Index(i), name).changed();
                }
            });
            ui.end_row();
        });

        section(ui, "Behaviour");
        c |= toggle(ui, &mut d.autohide, "Auto-hide", "Shows when the mouse touches the screen edge");
        if d.autohide {
            ui.indent("delay", |ui| {
                let mut ms = d.autohide_delay_ms as f32;
                if ui.add(egui::Slider::new(&mut ms, 0.0..=2000.0).step_by(50.0).suffix(" ms").text("Hide delay")).changed() {
                    d.autohide_delay_ms = ms as u32;
                    c = true;
                }
            });
            ui.add_space(4.0);
        }
        ui.add_enabled_ui(!d.autohide, |ui| {
            let help = if d.autohide { "Not used with auto-hide" } else { "Maximized windows stop at the dock" };
            c |= toggle(ui, &mut d.reserve_space, "Reserve space", help);
        });
        c |= toggle(ui, &mut d.always_on_top, "Always on top", "");
        c |= toggle(ui, &mut d.hide_on_fullscreen, "Hide for full screen", "Games and videos");
        c |= toggle(ui, &mut d.show_running, "Show running apps", "Includes apps that aren't pinned");
        c |= toggle(ui, &mut d.show_all_desktops, "All virtual desktops", "Clicking an app switches to its desktop");
        c |= toggle(ui, &mut d.show_labels, "Show app names", "On hover");
        c |= toggle(ui, &mut d.previews, "Window previews", "Hover a running app to see its windows");
        c |= toggle(ui, &mut d.click_minimizes, "Click to minimize", "Clicking the focused app minimizes it");

        section(ui, "Shortcuts");
        let s = &mut self.cfg.shortcuts;
        c |= shortcut_field(ui, "Open pinned app", &mut s.open_app, "With 1–9, e.g. ctrl+alt", |t| hotkeys::parse_modifiers(t).map(|_| ()));
        c |= shortcut_field(ui, "Show or hide dock", &mut s.toggle_dock, "e.g. win+shift+d", |t| hotkeys::parse_shortcut(t).map(|_| ()));
        let d = &mut self.cfg.dock;

        section(ui, "System");
        c |= toggle(ui, &mut d.autostart, "Start at sign-in", "");
        c |= toggle(ui, &mut d.gpu, "GPU rendering", "Uses about 40 MB more memory");
        c
    }

    fn appearance(&mut self, ui: &mut egui::Ui) -> bool {
        let mut c = false;
        let a = &mut self.cfg.appearance;
        section(ui, "Style");
        grid(ui, "style", |ui| {
            ui.label("Theme");
            ui.horizontal(|ui| {
                for (t, label) in [(Theme::Auto, "Match Windows"), (Theme::Dark, "Dark"), (Theme::Light, "Light")] {
                    c |= ui.selectable_value(&mut a.theme, t, label).changed();
                }
            });
            ui.end_row();
            ui.label("Running indicator");
            ui.horizontal(|ui| {
                for (s, label) in [(IndicatorStyle::Dot, "Dots"), (IndicatorStyle::Line, "Line"), (IndicatorStyle::None, "None")] {
                    c |= ui.selectable_value(&mut a.indicator, s, label).changed();
                }
            });
            ui.end_row();
        });
        ui.add_space(4.0);
        c |= toggle(ui, &mut a.blur, "Frosted glass", "Needs Windows transparency effects");

        section(ui, "Size");
        grid(ui, "size", |ui| {
            c |= slider(ui, "Icon size", &mut a.icon_size, 16.0..=128.0, " px");
            c |= slider(ui, "Spacing", &mut a.spacing, 0.0..=32.0, " px");
            let (mut px, mut py) = (a.padding_x(), a.padding_y());
            let x = slider(ui, "Padding left/right", &mut px, 0.0..=32.0, " px");
            let y = slider(ui, "Padding top/bottom", &mut py, 0.0..=32.0, " px");
            if x || y {
                // Write both directions explicitly and retire the old single value.
                (a.padding, a.padding_x, a.padding_y) = (None, Some(px), Some(py));
                c = true;
            }
            c |= slider(ui, "Distance from edge", &mut a.edge_margin, 0.0..=64.0, " px");
            c |= slider(ui, "Corner radius", &mut a.corner_radius, 0.0..=48.0, " px");
        });

        section(ui, "Magnification");
        let m = &mut self.cfg.magnification;
        c |= toggle(ui, &mut m.enabled, "Magnify on hover", "");
        ui.add_enabled_ui(m.enabled, |ui| {
            grid(ui, "mag", |ui| {
                ui.label("Size");
                c |= ui.add(egui::Slider::new(&mut m.scale, 1.0..=3.0).step_by(0.05).custom_formatter(|v, _| format!("{v:.2}×"))).changed();
                ui.end_row();
                ui.label("Spread");
                c |= ui
                    .add(egui::Slider::new(&mut m.range, 1.0..=6.0).step_by(0.5).suffix(" icons"))
                    .on_hover_text("How many neighbouring icons grow too")
                    .changed();
                ui.end_row();
            });
        });

        section(ui, "Animation");
        let an = &mut self.cfg.animation;
        c |= toggle(ui, &mut an.enabled, "Animations", "");
        ui.add_enabled_ui(an.enabled, |ui| {
            grid(ui, "anim", |ui| {
                ui.label("Speed");
                c |= ui.add(egui::Slider::new(&mut an.speed, 0.25..=3.0).step_by(0.05).custom_formatter(|v, _| format!("{v:.2}×"))).changed();
                ui.end_row();
            });
            c |= toggle(ui, &mut an.bounce_on_launch, "Bounce on launch", "");
        });

        section(ui, "Labels");
        let a = &mut self.cfg.appearance;
        grid(ui, "labels", |ui| {
            ui.label("Font");
            c |= ui.add(egui::TextEdit::singleline(&mut a.label_font).desired_width(220.0)).changed();
            ui.end_row();
            c |= slider(ui, "Size", &mut a.label_size, 8.0..=24.0, " px");
        });

        section(ui, "Colours");
        ui.label(egui::RichText::new("Unticked colours follow the theme.").weak());
        let dark = match self.cfg.appearance.theme {
            Theme::Auto => shell::system_dark_mode(),
            Theme::Dark => true,
            Theme::Light => false,
        };
        let theme = Config { appearance: config::Appearance { ..Default::default() }, ..Default::default() }
            .palette(dark, self.cfg.appearance.blur);
        let a = &mut self.cfg.appearance;
        grid(ui, "colours", |ui| {
            let slots: [(&str, &mut Option<Color>, Color); 7] = [
                ("Background", &mut a.background, theme.background),
                ("Border", &mut a.border, theme.border),
                ("Running indicator", &mut a.indicator_color, theme.indicator),
                ("Focused app indicator", &mut a.active_indicator_color, theme.active),
                ("Needs attention", &mut a.attention_color, theme.attention),
                ("Label background", &mut a.label_background, theme.label_bg),
                ("Label text", &mut a.label_text, theme.label_text),
            ];
            for (label, slot, default) in slots {
                let mut custom = slot.is_some();
                if ui.checkbox(&mut custom, label).changed() {
                    *slot = custom.then_some(default);
                    c = true;
                }
                let mut rgba = to_rgba(slot.unwrap_or(default));
                let r = ui.add_enabled_ui(custom, |ui| ui.color_edit_button_srgba_unmultiplied(&mut rgba)).inner;
                if r.changed() {
                    *slot = Some(Color::rgba(rgba[0], rgba[1], rgba[2], rgba[3]));
                    c = true;
                }
                ui.end_row();
            }
        });
        c
    }

    fn apps(&mut self, ui: &mut egui::Ui) -> bool {
        let mut c = false;
        ui.label(egui::RichText::new("Drop files on the dock to pin them, or right-click it › Import taskbar pins.").weak());
        ui.add_space(8.0);
        let mut action: Option<(usize, i32)> = None; // (index, -1 up / +1 down / 0 remove)
        let count = self.cfg.pinned.len();
        let defaults: Vec<String> = self.cfg.pinned.clone().iter().map(|p| self.default_name(&p.path)).collect();
        for (i, p) in self.cfg.pinned.iter_mut().enumerate() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let mut name = p.name.clone().unwrap_or_default();
                    let edit = egui::TextEdit::singleline(&mut name).hint_text(&defaults[i]).desired_width(200.0);
                    if ui.add(edit).changed() {
                        p.name = (!name.trim().is_empty()).then_some(name);
                        c = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Remove").clicked() {
                            action = Some((i, 0));
                        }
                        if ui.add_enabled(i + 1 < count, egui::Button::new("↓")).on_hover_text("Move right/down").clicked() {
                            action = Some((i, 1));
                        }
                        if ui.add_enabled(i > 0, egui::Button::new("↑")).on_hover_text("Move left/up").clicked() {
                            action = Some((i, -1));
                        }
                    });
                });
                ui.label(egui::RichText::new(&p.path).small().weak());
            });
        }
        if let Some((i, op)) = action {
            match op {
                0 => {
                    self.cfg.pinned.remove(i);
                }
                -1 => self.cfg.pinned.swap(i, i - 1),
                _ => self.cfg.pinned.swap(i, i + 1),
            }
            c = true;
        }
        ui.add_space(8.0);
        if ui.button("Add app…").clicked()
            && let Some(path) = pick_app()
                && !self.cfg.pinned.iter().any(|p| p.path.eq_ignore_ascii_case(&path)) {
                    self.cfg.pinned.push(Pinned { path, ..Default::default() });
                    c = true;
                }
        c
    }
}

impl eframe::App for Settings {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_file();
        if self.pending.is_some_and(|t| t.elapsed() >= SAVE_DELAY) {
            self.save_now();
        }

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for (t, label) in [(Tab::General, "General"), (Tab::Appearance, "Appearance"), (Tab::Apps, "Apps")] {
                    ui.selectable_value(&mut self.tab, t, egui::RichText::new(label).size(15.0));
                }
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let note = match (&self.broken, &self.status) {
                    (Some(_), _) => egui::RichText::new("Fix config.toml to edit here").color(ui.visuals().warn_fg_color),
                    (None, Some(s)) => egui::RichText::new(s).weak(),
                    (None, None) => egui::RichText::new("Changes apply instantly").weak(),
                };
                ui.label(note);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if ui.add_enabled(self.known != self.original, egui::Button::new("Revert")).clicked() {
                        self.revert();
                    }
                    if ui.button("Open config file").clicked() {
                        shell::edit_file(&self.path);
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(err) = &self.broken {
                egui::Frame::new()
                    .fill(ui.visuals().extreme_bg_color)
                    .inner_margin(10.0)
                    .corner_radius(6.0)
                    .show(ui, |ui| {
                        ui.colored_label(ui.visuals().warn_fg_color, format!("config.toml has an error: {err}"));
                        ui.label("Fix it in the file (Open config file below); this window updates once it's valid.");
                    });
                ui.add_space(8.0);
            }
            let enabled = self.broken.is_none();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_enabled_ui(enabled, |ui| {
                    let changed = match self.tab {
                        Tab::General => self.general(ui),
                        Tab::Appearance => self.appearance(ui),
                        Tab::Apps => self.apps(ui),
                    };
                    if changed {
                        self.changed();
                    }
                });
                ui.add_space(12.0);
            });
        });

        // Keep polling the file, and wake up in time to save pending edits.
        let wait = if self.pending.is_some() { SAVE_DELAY } else { Duration::from_secs(1) };
        ui.ctx().request_repaint_after(wait);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.pending.is_some() {
            self.save_now();
        }
    }
}

/// A shortcut text box with helper text, or the problem if it can't be read.
fn shortcut_field(ui: &mut egui::Ui, label: &str, value: &mut String, help: &str, check: impl Fn(&str) -> Result<(), String>) -> bool {
    let mut changed = false;
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(egui::vec2(150.0, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_min_width(150.0);
                ui.label(label);
            });
            changed = ui.add(egui::TextEdit::singleline(value).desired_width(180.0).hint_text("off")).changed();
        });
        let note = match check(value) {
            Ok(()) => egui::RichText::new(help).small().weak(),
            Err(e) => egui::RichText::new(e).small().color(ui.visuals().warn_fg_color),
        };
        ui.horizontal(|ui| {
            ui.add_space(150.0 + ui.spacing().item_spacing.x);
            ui.label(note);
        });
    });
    ui.add_space(4.0);
    changed
}

/// A checkbox with a short label and, optionally, a line of helper text beneath it.
fn toggle(ui: &mut egui::Ui, value: &mut bool, label: &str, help: &str) -> bool {
    let changed = ui
        .vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            let changed = ui.checkbox(value, label).changed();
            if !help.is_empty() {
                let indent = ui.spacing().icon_width + ui.spacing().icon_spacing;
                ui.horizontal(|ui| {
                    ui.add_space(indent);
                    ui.label(egui::RichText::new(help).small().weak());
                });
            }
            changed
        })
        .inner;
    ui.add_space(4.0);
    changed
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(14.0);
    ui.label(egui::RichText::new(title).strong().size(15.0));
    ui.add_space(4.0);
}

fn grid(ui: &mut egui::Ui, id: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id).num_columns(2).spacing([24.0, 8.0]).min_col_width(140.0).show(ui, body);
}

fn slider(ui: &mut egui::Ui, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>, suffix: &str) -> bool {
    ui.label(label);
    let changed = ui.add(egui::Slider::new(value, range).step_by(1.0).suffix(suffix)).changed();
    ui.end_row();
    changed
}

fn to_rgba(c: Color) -> [u8; 4] {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [b(c.r), b(c.g), b(c.b), b(c.a)]
}

/// Segoe UI to match the rest of Windows, with egui's fonts kept as fallbacks.
fn install_fonts(ctx: &egui::Context) {
    let fonts_dir = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join("Fonts");
    let Some(bytes) = ["SegUIVar.ttf", "segoeui.ttf"].iter().find_map(|f| std::fs::read(fonts_dir.join(f)).ok()) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("segoe".into(), Arc::new(egui::FontData::from_owned(bytes)));
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        family.insert(0, "segoe".into());
    }
    ctx.set_fonts(fonts);
}

/// Native "Open" dialog for an app, shortcut or script. Shortcuts are returned as-is.
fn pick_app() -> Option<String> {
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let filters = [
            COMDLG_FILTERSPEC { pszName: w!("Apps and shortcuts"), pszSpec: w!("*.exe;*.lnk;*.bat;*.cmd;*.url") },
            COMDLG_FILTERSPEC { pszName: w!("All files"), pszSpec: w!("*.*") },
        ];
        dialog.SetFileTypes(&filters).ok()?;
        dialog.SetOptions(FOS_FILEMUSTEXIST | FOS_NODEREFERENCELINKS).ok()?;
        dialog.SetTitle(w!("Add an app to the dock")).ok()?;
        dialog.Show(None).ok()?;
        let path = dialog.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as _));
        s
    }
}
