//! The dock window: layout, magnification, input, window tracking and animation.

mod droptarget;
mod preview;

use crate::apps::{self, AppKey};
use crate::clock;
use crate::config::{self, Color, Config, IndicatorStyle, MonitorSel, Palette, Pinned, Placement, Position};
use crate::icons::{IconLoader, IconPixels};
use crate::render::{Font, Frame, Gpu, IconDraw, LabelDraw, RectF, Shape, Surface, TextDraw};
use crate::hotkeys;
use crate::shell;
use crate::status;
use crate::taskbar;
use crate::util::{from_wide, wide};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::f32::consts::PI;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmFlush, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::{FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FindFirstChangeNotificationW, FindNextChangeNotification};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcessId, INFINITE, WaitForSingleObject};
use windows::Win32::System::WinRT::{CreateDispatcherQueueController, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT, DispatcherQueueOptions};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, UnregisterHotKey};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, PCWSTR, Result, w};

const WM_MOUSELEAVE: u32 = 0x02A3;
const HSHELL_FLASH: u32 = 0x8006;
const WM_APP_REFRESH: u32 = WM_APP + 1;
const WM_APP_FOREGROUND: u32 = WM_APP + 2;
const WM_APP_ICONS: u32 = WM_APP + 3;
const WM_APP_CONFIG: u32 = WM_APP + 4;
const WM_APP_APPBAR: u32 = WM_APP + 5;
const WM_APP_DROP: u32 = WM_APP + 6;
const WM_APP_TASKBAR_SHOWN: u32 = WM_APP + 7;
/// Volume, network or battery changed (posted from system threads).
const WM_APP_STATUS: u32 = WM_APP + 8;

const TIMER_REFRESH: usize = 1;
const TIMER_CONFIG: usize = 2;
const TIMER_HIDE: usize = 3;
const TIMER_NOTICE: usize = 4;
// 5 and 6 are the preview's.
const TIMER_CLOCK: usize = 7;
const TIMER_STATUS: usize = 8;

/// Hotkey ids: HOTKEY_APP + n opens pinned app n (0-based); HOTKEY_TOGGLE shows/hides the dock.
const HOTKEY_APP: i32 = 1;
const HOTKEY_TOGGLE: i32 = 20;

const CMD_OPEN: u32 = 1;
const CMD_PIN: u32 = 2;
const CMD_UNPIN: u32 = 3;
const CMD_CLOSE: u32 = 4;
const CMD_SETTINGS: u32 = 5;
const CMD_QUIT: u32 = 6;
const CMD_IMPORT: u32 = 7;
const CMD_NOTIFICATIONS: u32 = 8;
const CMD_DATE_TIME: u32 = 9;
const CMD_SOUND: u32 = 10;
const CMD_MIXER: u32 = 11;
const CMD_NETWORK: u32 = 12;
const CMD_POWER: u32 = 13;
/// Menu ids from here up pick a window: CMD_WINDOW + index into `MenuSpec::windows`.
const CMD_WINDOW: u32 = 100;

/// One hop of the launch bounce, in seconds.
const BOUNCE_PERIOD: f32 = 0.55;
const NOTICE_TIME: Duration = Duration::from_secs(8);
const INFO_TIME: Duration = Duration::from_secs(3);

static DOCK_HWND: AtomicIsize = AtomicIsize::new(0);
static SHELLHOOK_MSG: AtomicU32 = AtomicU32::new(0);
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// Windows currently shown in the dock; read by the WinEvent hook to filter noise.
    static TRACKED: RefCell<HashSet<isize>> = RefCell::new(HashSet::new());
}

/// Runs `f` against the app unless it is already borrowed further up the stack
/// (Win32 re-enters the window procedure from calls like SetWindowPos).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

fn dock_hwnd() -> HWND {
    HWND(DOCK_HWND.load(Ordering::Relaxed) as _)
}

fn post(msg: u32, wp: usize) {
    unsafe {
        let _ = PostMessageW(Some(dock_hwnd()), msg, WPARAM(wp), LPARAM(0));
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Pinned(usize),
    Running,
    Separator,
    Clock,
    /// Network, volume and battery icons.
    Status,
}

#[derive(Clone, Debug)]
struct Launch {
    path: String,
    args: Option<String>,
    cwd: Option<String>,
}

#[derive(Clone, Debug)]
struct Item {
    id: String,
    kind: Kind,
    name: String,
    icon: String,
    key: AppKey,
    /// Windows in the order they were first seen (stable for cycling).
    windows: Vec<isize>,
    launch: Option<Launch>,
    mag: f32,
    appear: f32,
    dying: bool,
    shift: f32,
    bounce: Option<f32>,
    bounce_end: f32,
}

impl Item {
    fn new(id: String, kind: Kind, name: String, icon: String, key: AppKey, launch: Option<Launch>) -> Self {
        Self {
            id,
            kind,
            name,
            icon,
            key,
            windows: Vec::new(),
            launch,
            mag: 1.0,
            appear: 1.0,
            dying: false,
            shift: 0.0,
            bounce: None,
            bounce_end: 0.0,
        }
    }

    fn interactive(&self) -> bool {
        !self.dying && self.kind != Kind::Separator
    }

    /// An app icon, as opposed to the clock, status icons or a separator.
    fn is_app(&self) -> bool {
        matches!(self.kind, Kind::Pinned(_) | Kind::Running)
    }
}

/// Maps dock-relative coordinates to window pixels. `u` runs along the dock,
/// `v` runs away from the screen edge the dock is attached to.
#[derive(Clone, Copy, Default)]
struct Axis {
    pos: Position,
    w: f32,
    h: f32,
}

impl Axis {
    fn horizontal(&self) -> bool {
        matches!(self.pos, Position::Bottom | Position::Top)
    }
    fn main(&self) -> f32 {
        if self.horizontal() { self.w } else { self.h }
    }
    fn xy(&self, u: f32, v: f32) -> (f32, f32) {
        match self.pos {
            Position::Bottom => (u, self.h - v),
            Position::Top => (u, v),
            Position::Left => (v, u),
            Position::Right => (self.w - v, u),
        }
    }
    fn uv(&self, x: f32, y: f32) -> (f32, f32) {
        match self.pos {
            Position::Bottom => (x, self.h - y),
            Position::Top => (x, y),
            Position::Left => (y, x),
            Position::Right => (y, self.w - x),
        }
    }
    fn rect(&self, u0: f32, u1: f32, v0: f32, v1: f32) -> RectF {
        let (ax, ay) = self.xy(u0, v0);
        let (bx, by) = self.xy(u1, v1);
        RectF { l: ax.min(bx), t: ay.min(by), r: ax.max(bx), b: ay.max(by) }
    }
    /// Screen offset that pushes the dock `d` pixels toward its edge.
    fn toward_edge(&self, d: f32) -> (f32, f32) {
        match self.pos {
            Position::Bottom => (0.0, d),
            Position::Top => (0.0, -d),
            Position::Left => (-d, 0.0),
            Position::Right => (d, 0.0),
        }
    }
}

/// Sizes in physical pixels for the current monitor.
#[derive(Clone, Copy, Default)]
struct Metrics {
    s: f32,
    icon: f32,
    gap: f32,
    /// Padding at the ends of the dock (along it).
    pad_main: f32,
    /// Padding beside the icons (across the dock).
    pad_cross: f32,
    margin: f32,
    thick: f32,
    radius: f32,
    sep: f32,
    mag: f32,
    range: f32,
    bounce: f32,
    cross: f32,
}

impl Metrics {
    fn new(cfg: &Config, s: f32, horizontal: bool) -> Self {
        let a = &cfg.appearance;
        let icon = (a.icon_size * s).round();
        // padding_x/padding_y are screen directions, so which one runs along the dock depends on the edge.
        let (px, py) = ((a.padding_x() * s).round(), (a.padding_y() * s).round());
        let (pad_main, pad_cross) = if horizontal { (px, py) } else { (py, px) };
        let gap = a.spacing * s;
        let margin = (a.edge_margin * s).round();
        let thick = icon + 2.0 * pad_cross;
        let mag = if cfg.magnification.enabled { cfg.magnification.scale } else { 1.0 };
        let bounce = icon * 0.5;
        // Room for the hover label (or a notice) beyond the tallest icon.
        let label_room = if horizontal { (a.label_size * 1.4 + 22.0) * s } else { 300.0 * s };
        let cross = margin + pad_cross + (icon * mag).max(icon + bounce) + pad_cross.max(4.0 * s) + label_room;
        Self {
            s,
            icon,
            gap,
            pad_main,
            pad_cross,
            margin,
            thick,
            radius: (a.corner_radius * s).min(thick / 2.0),
            sep: gap.max(4.0 * s),
            mag,
            range: cfg.magnification.range,
            bounce,
            cross,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Slot {
    u0: f32,
    w: f32,
    center: f32,
    size: f32,
}

#[derive(Clone, Default)]
struct Layout {
    slots: Vec<Slot>,
    bar: (f32, f32),
    /// The corner island's extent along the dock, when the clock and status icons are there.
    island: Option<(f32, f32)>,
}

struct Press {
    id: Option<String>,
    x: f32,
    y: f32,
}

struct Drag {
    id: String,
    original: Vec<Pinned>,
    removing: bool,
}

struct MenuEntry {
    /// 0 for a separator.
    id: u32,
    label: String,
    checked: bool,
}

impl MenuEntry {
    fn new(id: u32, label: impl Into<String>) -> Self {
        Self { id, label: label.into(), checked: false }
    }
    fn separator() -> Self {
        Self::new(0, "")
    }
}

pub struct MenuSpec {
    item: Option<String>,
    entries: Vec<MenuEntry>,
    windows: Vec<isize>,
    align: TRACK_POPUP_MENU_FLAGS,
}

fn approach(x: &mut f32, target: f32, k: f32, eps: f32) -> bool {
    if (target - *x).abs() <= eps {
        let moved = *x != target;
        *x = target;
        moved
    } else {
        *x += (target - *x) * k;
        true
    }
}

pub struct App {
    hwnd: HWND,
    own_pid: u32,
    cfg: Config,
    cfg_path: PathBuf,
    cfg_text: String,
    palette: Palette,
    blur: bool,
    gpu: Option<Gpu>,
    /// The dock's drawing surface (the preview popup has its own).
    surface: Option<Surface>,
    preview: preview::Preview,
    /// Screen position of the dock window, and the work area of its monitor.
    origin: (i32, i32),
    work: RECT,
    render_failures: u32,
    monitor: HMONITOR,
    axis: Axis,
    m: Metrics,
    icon_px: u32,
    icons: HashMap<String, Option<IconPixels>>,
    loader: IconLoader,

    windows: Vec<(isize, AppKey)>,
    keys: HashMap<isize, (AppKey, u64)>,
    seq: u64,
    fg: isize,
    attention: HashSet<isize>,
    pins: HashMap<String, (AppKey, String)>,
    names: HashMap<String, String>,
    items: Vec<Item>,
    first_build: bool,
    /// The clock's lines, its hover text, and its length along the dock.
    clock_text: String,
    clock_tip: String,
    clock_len: f32,
    status: Option<status::Watcher>,
    /// One glyph per status icon, and the hover text.
    status_glyphs: String,
    status_tip: String,
    has_battery: bool,

    cursor: Option<(f32, f32)>,
    tracking: bool,
    anchor: f32,
    press: Option<Press>,
    drag: Option<Drag>,
    /// Files being dragged over the dock from another app.
    dragging: Option<droptarget::Dragging>,
    /// A drop waiting to be carried out (see `drag_drop`).
    dropped: Option<(droptarget::DropAction, Vec<String>)>,
    menu_open: bool,
    hotkeys: Vec<i32>,
    /// Hidden with the show/hide shortcut.
    user_hidden: bool,

    layout: Layout,
    hide: f32,
    hide_target: f32,
    fullscreen: bool,
    dirty: bool,
    animating: bool,
    last: Instant,
    region: Vec<RECT>,
    /// Text shown above the dock, and when it disappears.
    notice: Option<(String, Instant)>,
    first_run: bool,
    reserved: Option<RECT>,
    hooks: Vec<HWINEVENTHOOK>,
}

impl App {
    fn new(hwnd: HWND) -> Self {
        let cfg_path = config::config_path();
        let first_run = !cfg_path.exists();
        let (parsed, cfg_text) = config::load(&cfg_path);
        let mut notice = None;
        let cfg = parsed.unwrap_or_else(|e| {
            crate::log!("config error: {e}");
            notice = Some((config_error(&e), Instant::now() + NOTICE_TIME));
            config::parse(&config::default_text()).unwrap_or_default()
        });
        unsafe {
            // Lets the compositor's host-backdrop brush see what's behind the dock.
            let on = BOOL(1);
            let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(17), &on as *const _ as _, 4);
        }
        let gpu = Gpu::new(cfg.dock.gpu).inspect_err(|e| crate::log!("renderer init failed: {e}")).ok();
        let surface = gpu.as_ref().and_then(|g| g.create_surface(hwnd, 1, 1).inspect_err(|e| crate::log!("surface init failed: {e}")).ok());
        let instance = unsafe { GetModuleHandleW(None) }.map(HINSTANCE::from).unwrap_or_default();
        let preview_hwnd = preview::create_window(instance).inspect_err(|e| crate::log!("preview window failed: {e}")).unwrap_or_default();
        Self {
            hwnd,
            own_pid: unsafe { GetCurrentProcessId() },
            palette: cfg.palette(true, true),
            cfg,
            cfg_path,
            cfg_text,
            blur: true,
            gpu,
            surface,
            preview: preview::Preview::new(preview_hwnd),
            origin: (0, 0),
            work: RECT::default(),
            render_failures: 0,
            monitor: HMONITOR::default(),
            axis: Axis::default(),
            m: Metrics::default(),
            icon_px: 0,
            icons: HashMap::new(),
            loader: IconLoader::new(hwnd, WM_APP_ICONS),
            windows: Vec::new(),
            keys: HashMap::new(),
            seq: 0,
            fg: 0,
            attention: HashSet::new(),
            pins: HashMap::new(),
            names: HashMap::new(),
            items: Vec::new(),
            first_build: true,
            clock_text: String::new(),
            clock_tip: String::new(),
            clock_len: 0.0,
            status: None,
            status_glyphs: String::new(),
            status_tip: String::new(),
            has_battery: false,
            cursor: None,
            tracking: false,
            anchor: 0.0,
            press: None,
            drag: None,
            dragging: None,
            dropped: None,
            menu_open: false,
            hotkeys: Vec::new(),
            user_hidden: false,
            layout: Layout::default(),
            hide: 0.0,
            hide_target: 0.0,
            fullscreen: false,
            dirty: true,
            animating: false,
            last: Instant::now(),
            region: Vec::new(),
            notice,
            first_run,
            reserved: None,
            hooks: Vec::new(),
        }
    }

    fn start(&mut self) {
        unsafe {
            SHELLHOOK_MSG.store(RegisterWindowMessageW(w!("SHELLHOOK")), Ordering::Relaxed);
            TASKBAR_CREATED_MSG.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
            let _ = RegisterShellHookWindow(self.hwnd);
            for (lo, hi) in [
                (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
                (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
                (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
                (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
            ] {
                let hook = SetWinEventHook(lo, hi, None, Some(win_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
                if !hook.is_invalid() {
                    self.hooks.push(hook);
                }
            }
        }
        self.appbar_register();
        self.apply_taskbar();
        self.apply_theme();
        shell::set_autostart(self.cfg.dock.autostart);
        if self.first_run {
            // Start from whatever is pinned to the taskbar, keeping the built-in defaults otherwise.
            let defaults = std::mem::take(&mut self.cfg.pinned);
            if self.import_taskbar_pins() == 0 {
                self.cfg.pinned = defaults;
            }
        }
        self.place();
        self.update_status();
        self.refresh_windows();
        droptarget::register(self.hwnd);
        self.register_hotkeys();
        watch_config(self.cfg_path.parent().unwrap_or(Path::new(".")).to_path_buf());
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        self.check_fullscreen();
        if self.cfg.dock.autohide {
            self.schedule_hide(self.cfg.dock.autohide_delay_ms + 1500);
        }
        if self.notice.is_some() {
            self.set_timer(TIMER_NOTICE, NOTICE_TIME.as_millis() as u32);
        }
    }

    fn shutdown(&mut self) {
        self.close_preview();
        droptarget::unregister(self.hwnd);
        self.unregister_hotkeys();
        unsafe {
            for h in self.hooks.drain(..) {
                let _ = UnhookWinEvent(h);
            }
            let _ = DeregisterShellHookWindow(self.hwnd);
        }
        self.appbar_message(ABM_REMOVE);
        taskbar::restore();
    }

    /// Also cleans up after a previous run that hid the taskbar and then didn't exit cleanly.
    fn apply_taskbar(&self) {
        if self.cfg.dock.hide_taskbar {
            taskbar::hide();
        } else {
            taskbar::restore();
        }
    }

    fn wants_frame(&self) -> bool {
        self.dirty || self.animating
    }

    fn set_timer(&self, id: usize, ms: u32) {
        unsafe {
            SetTimer(Some(self.hwnd), id, ms, None);
        }
    }

    fn kill_timer(&self, id: usize) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), id);
        }
    }

    // ---------------------------------------------------------------- config

    fn reload_config(&mut self) {
        let (parsed, text) = config::load(&self.cfg_path);
        if text == self.cfg_text {
            return;
        }
        self.cfg_text = text;
        match parsed {
            Ok(cfg) => {
                let old = std::mem::replace(&mut self.cfg, cfg);
                if old.dock.autostart != self.cfg.dock.autostart {
                    shell::set_autostart(self.cfg.dock.autostart);
                }
                if old.shortcuts != self.cfg.shortcuts {
                    self.register_hotkeys();
                }
                if old.dock.hide_taskbar != self.cfg.dock.hide_taskbar {
                    self.apply_taskbar();
                }
                if old.dock.show_all_desktops != self.cfg.dock.show_all_desktops {
                    self.refresh_windows();
                }
                if old.dock.gpu != self.cfg.dock.gpu {
                    self.gpu = None;
                    self.surface = None;
                    self.recreate_renderer();
                }
                if !self.cfg.dock.autohide {
                    self.hide_target = 0.0;
                } else if !old.dock.autohide {
                    self.schedule_hide(self.cfg.dock.autohide_delay_ms);
                }
                self.notice = None;
                self.apply_theme();
                self.place();
                self.update_status();
                self.rebuild_items();
                self.check_fullscreen();
                crate::log!("config reloaded");
            }
            Err(e) => {
                crate::log!("config error: {e}");
                self.show_notice(config_error(&e), NOTICE_TIME);
            }
        }
    }

    fn show_notice(&mut self, text: String, duration: Duration) {
        self.notice = Some((text, Instant::now() + duration));
        self.set_timer(TIMER_NOTICE, duration.as_millis() as u32);
        self.hide_target = 0.0;
        self.dirty = true;
    }

    fn save_pins(&mut self) {
        match config::save_pinned(&self.cfg_path, &self.cfg.pinned) {
            Ok(text) => self.cfg_text = text,
            Err(e) => self.show_notice(format!("Couldn't save config: {e}"), NOTICE_TIME),
        }
    }

    /// Adds taskbar pins that aren't in the dock yet. Shortcuts are copied into Windock's own
    /// folder so they keep working if they're later unpinned from the taskbar.
    fn import_taskbar_pins(&mut self) -> usize {
        let mut existing: Vec<(AppKey, String, String)> = Vec::new();
        for p in self.cfg.pinned.clone() {
            let path = shell::expand_env(&p.path);
            let (key, name) = self.pin_info(&path);
            existing.push((key, p.name.unwrap_or(name), path));
        }
        let folder = crate::util::data_dir().join("Pinned");
        let mut added = 0;
        for pin in taskbar::pinned() {
            let (source, file) = match &pin {
                taskbar::Pin::Shortcut(p) => (p.to_string_lossy().into_owned(), p.file_name().map(|f| f.to_string_lossy().into_owned())),
                taskbar::Pin::App(aumid) => (format!("{}{aumid}", shell::APPS_FOLDER), None),
            };
            if matches!(pin, taskbar::Pin::App(_)) && shell::display_name(&source).is_none() {
                continue; // not an installed app
            }
            let (key, name) = self.pin_info(&source);
            let duplicate = existing
                .iter()
                .any(|(k, n, p)| k.matches(&key) || n.eq_ignore_ascii_case(&name) || p.eq_ignore_ascii_case(&source));
            if duplicate {
                continue;
            }
            let path = match file {
                Some(file) => {
                    let copied = std::fs::create_dir_all(&folder).and_then(|_| std::fs::copy(&source, folder.join(&file)));
                    if let Err(e) = copied {
                        crate::log!("couldn't copy {source}: {e}");
                        continue;
                    }
                    format!(r"%APPDATA%\Windock\Pinned\{file}")
                }
                None => source.clone(),
            };
            self.cfg.pinned.push(Pinned { path, ..Default::default() });
            existing.push((key, name, source));
            added += 1;
        }
        crate::log!("imported {added} taskbar pins");
        if added > 0 {
            self.save_pins();
            self.rebuild_items();
        }
        added
    }

    fn apply_theme(&mut self) {
        let dark = match self.cfg.appearance.theme {
            config::Theme::Auto => shell::system_dark_mode(),
            config::Theme::Dark => true,
            config::Theme::Light => false,
        };
        self.blur = self.cfg.appearance.blur && shell::transparency_enabled();
        self.palette = self.cfg.palette(dark, self.blur);
        shell::set_menu_theme(self.cfg.appearance.theme);
        self.dirty = true;
    }

    // ------------------------------------------------------------- placement

    fn place(&mut self) {
        let monitors = monitors();
        let primary = monitors.iter().find(|(_, mi)| mi.dwFlags & MONITORINFOF_PRIMARY != 0).or(monitors.first());
        let chosen = match &self.cfg.dock.monitor {
            MonitorSel::Index(i) => monitors.get(*i).or(primary),
            MonitorSel::Name(_) => primary,
        };
        let Some(&(hmon, mi)) = chosen else { return };
        self.monitor = hmon;

        let (mut dx, mut dy) = (96u32, 96u32);
        unsafe {
            let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        }
        let s = dx as f32 / 96.0;
        let pos = self.cfg.dock.position;
        let horizontal = matches!(pos, Position::Bottom | Position::Top);
        self.m = Metrics::new(&self.cfg, s, horizontal);
        let cross = self.m.cross.ceil() as i32;

        let mut area = mi.rcWork;
        if self.cfg.dock.reserve_space && !self.cfg.dock.autohide {
            let rc = self.appbar_reserve(mi.rcMonitor, (self.m.margin + self.m.thick).ceil() as i32);
            match pos {
                Position::Bottom => area.bottom = rc.bottom,
                Position::Top => area.top = rc.top,
                Position::Left => area.left = rc.left,
                Position::Right => area.right = rc.right,
            }
        } else if self.reserved.take().is_some() {
            // Give the space back, but stay registered for full-screen notifications.
            self.appbar_message(ABM_REMOVE);
            self.appbar_register();
        }

        let (x, y, w, h) = match pos {
            Position::Bottom => (area.left, area.bottom - cross, area.right - area.left, cross),
            Position::Top => (area.left, area.top, area.right - area.left, cross),
            Position::Left => (area.left, area.top, cross, area.bottom - area.top),
            Position::Right => (area.right - cross, area.top, cross, area.bottom - area.top),
        };
        let z = if self.cfg.dock.always_on_top { HWND_TOPMOST } else { HWND_NOTOPMOST };
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(z), x, y, w, h, SWP_NOACTIVATE);
        }
        self.axis = Axis { pos, w: w as f32, h: h as f32 };
        self.origin = (x, y);
        self.work = mi.rcWork;
        self.close_preview();
        if let Some(sf) = &mut self.surface {
            let _ = sf.resize(w as u32, h as u32);
        }
        self.set_fonts();

        // Load icons at the largest size they'll be drawn.
        let px = (((self.m.icon * self.m.mag).ceil() as u32).div_ceil(16) * 16).clamp(32, 256);
        if px != self.icon_px {
            self.icon_px = px;
            self.icons.clear();
            self.loader.reset();
            if let Some(g) = &mut self.gpu {
                g.clear_bitmaps();
            }
            self.request_icons();
        }
        self.region.clear();
        self.layout = self.compute_layout();
        self.dirty = true;
    }

    fn appbar_data(&self) -> APPBARDATA {
        APPBARDATA {
            cbSize: size_of::<APPBARDATA>() as u32,
            hWnd: self.hwnd,
            uCallbackMessage: WM_APP_APPBAR,
            uEdge: match self.cfg.dock.position {
                Position::Left => ABE_LEFT,
                Position::Top => ABE_TOP,
                Position::Right => ABE_RIGHT,
                Position::Bottom => ABE_BOTTOM,
            },
            ..Default::default()
        }
    }

    fn appbar_message(&self, msg: u32) {
        let mut abd = self.appbar_data();
        unsafe {
            SHAppBarMessage(msg, &mut abd);
        }
    }

    /// Registering as an app bar gets us full-screen notifications even without reserving space.
    fn appbar_register(&self) {
        self.appbar_message(ABM_NEW);
    }

    fn appbar_reserve(&mut self, mon: RECT, thick: i32) -> RECT {
        let mut abd = self.appbar_data();
        abd.rc = match self.cfg.dock.position {
            Position::Bottom => RECT { top: mon.bottom - thick, ..mon },
            Position::Top => RECT { bottom: mon.top + thick, ..mon },
            Position::Left => RECT { right: mon.left + thick, ..mon },
            Position::Right => RECT { left: mon.right - thick, ..mon },
        };
        unsafe {
            SHAppBarMessage(ABM_QUERYPOS, &mut abd);
            match self.cfg.dock.position {
                Position::Bottom => abd.rc.top = abd.rc.bottom - thick,
                Position::Top => abd.rc.bottom = abd.rc.top + thick,
                Position::Left => abd.rc.right = abd.rc.left + thick,
                Position::Right => abd.rc.left = abd.rc.right - thick,
            }
            if self.reserved != Some(abd.rc) {
                SHAppBarMessage(ABM_SETPOS, &mut abd);
                self.reserved = Some(abd.rc);
            }
        }
        abd.rc
    }

    fn check_fullscreen(&mut self) {
        let fs = self.cfg.dock.hide_on_fullscreen && apps::fullscreen_app_on(self.monitor);
        if fs != self.fullscreen {
            self.fullscreen = fs;
            self.apply_visibility();
        }
    }

    /// The dock window is hidden while a full-screen app runs or after the show/hide shortcut.
    fn apply_visibility(&mut self) {
        let hidden = self.fullscreen || self.user_hidden;
        if hidden {
            self.close_preview();
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, if hidden { SW_HIDE } else { SW_SHOWNOACTIVATE });
        }
        self.dirty = true;
    }

    // -------------------------------------------------------------- hotkeys

    fn unregister_hotkeys(&mut self) {
        for id in self.hotkeys.drain(..) {
            unsafe {
                let _ = UnregisterHotKey(Some(self.hwnd), id);
            }
        }
    }

    fn register_hotkeys(&mut self) {
        self.unregister_hotkeys();
        let mut wanted: Vec<(i32, u32, u32)> = Vec::new();
        let mut problems: Vec<String> = Vec::new();
        match hotkeys::parse_modifiers(&self.cfg.shortcuts.open_app) {
            // 1-9, then 0 for the tenth pinned app.
            Ok(Some(mods)) => wanted.extend((0..10).map(|n| (HOTKEY_APP + n, mods, if n == 9 { 0x30 } else { 0x31 + n as u32 }))),
            Ok(None) => {}
            Err(e) => problems.push(format!("open_app: {e}")),
        }
        match hotkeys::parse_shortcut(&self.cfg.shortcuts.toggle_dock) {
            Ok(Some((mods, vk))) => wanted.push((HOTKEY_TOGGLE, mods, vk)),
            Ok(None) => {}
            Err(e) => problems.push(format!("toggle_dock: {e}")),
        }
        let mut taken = Vec::new();
        for (id, mods, vk) in wanted {
            let flags = HOT_KEY_MODIFIERS(mods) | MOD_NOREPEAT;
            if unsafe { RegisterHotKey(Some(self.hwnd), id, flags, vk) }.is_ok() {
                self.hotkeys.push(id);
            } else {
                taken.push(hotkeys::describe(mods, vk));
            }
        }
        if !taken.is_empty() {
            let list = if taken.len() > 3 { format!("{} and {} more", taken[..2].join(", "), taken.len() - 2) } else { taken.join(", ") };
            problems.push(format!("{list} already used by another app"));
        }
        if !problems.is_empty() {
            crate::log!("shortcuts: {}", problems.join("; "));
            self.show_notice(format!("Shortcuts: {}", problems.join("; ")), NOTICE_TIME);
        }
    }

    fn on_hotkey(&mut self, id: i32) {
        if id == HOTKEY_TOGGLE {
            self.toggle_dock();
            return;
        }
        let n = (id - HOTKEY_APP) as usize;
        if let Some(i) = self.items.iter().position(|it| it.kind == Kind::Pinned(n) && !it.dying) {
            self.click(i);
        }
    }

    /// With auto-hide, slides the dock in (or away); otherwise hides or shows it.
    fn toggle_dock(&mut self) {
        if self.cfg.dock.autohide {
            if self.hide_target >= 1.0 {
                self.hide_target = 0.0;
                self.schedule_hide(self.cfg.dock.autohide_delay_ms + 2000);
            } else {
                self.close_preview();
                self.hide_target = 1.0;
            }
            self.dirty = true;
        } else {
            self.user_hidden = !self.user_hidden;
            self.apply_visibility();
        }
    }

    // ---------------------------------------------------------------- model

    fn refresh_windows(&mut self) {
        let hwnds = apps::app_windows(self.own_pid, self.cfg.dock.show_all_desktops);
        let mut list = Vec::with_capacity(hwnds.len());
        for h in hwnds {
            let id = h.0 as isize;
            let cached = self.keys.get(&id).filter(|(k, _)| {
                // A UWP frame's app process can appear after the frame; keep re-checking those.
                !k.exe.as_deref().is_some_and(|e| e.to_lowercase().ends_with("applicationframehost.exe"))
            });
            let key = match cached {
                Some((k, _)) => k.clone(),
                None => {
                    let k = apps::window_key(h);
                    let seq = self.keys.get(&id).map(|(_, s)| *s).unwrap_or_else(|| {
                        self.seq += 1;
                        self.seq
                    });
                    self.keys.insert(id, (k.clone(), seq));
                    k
                }
            };
            list.push((id, key));
        }
        self.keys.retain(|id, _| list.iter().any(|(h, _)| h == id));
        self.attention.retain(|id| list.iter().any(|(h, _)| h == id));
        TRACKED.with(|t| *t.borrow_mut() = list.iter().map(|(h, _)| *h).collect());
        self.windows = list;
        self.fg = apps::foreground().0 as isize;
        self.rebuild_items();
        self.refresh_preview();
    }

    fn pin_info(&mut self, path: &str) -> (AppKey, String) {
        if let Some(info) = self.pins.get(path) {
            return info.clone();
        }
        let info = if let Some(aumid) = shell::apps_folder_aumid(path) {
            let key = AppKey { aumid: Some(aumid.to_string()), exe: None, packaged: true, ..Default::default() };
            (key, shell::display_name(path).unwrap_or_else(|| aumid.to_string()))
        } else if path.to_lowercase().ends_with(".lnk") {
            let (target, aumid) = shell::resolve_lnk(path);
            let name = Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            (AppKey { aumid, exe: target, packaged: false, ..Default::default() }, name)
        } else {
            (AppKey { aumid: None, exe: Some(path.to_string()), packaged: false, ..Default::default() }, shell::friendly_name(path))
        };
        self.pins.insert(path.to_string(), info.clone());
        info
    }

    fn running_name(&mut self, key: &AppKey, hwnd: isize) -> String {
        let gid = key.group_id();
        if let Some(n) = self.names.get(&gid) {
            return n.clone();
        }
        let name = match (&key.aumid, &key.exe) {
            (Some(a), _) if key.packaged => shell::display_name(&format!("{}{a}", shell::APPS_FOLDER)),
            (_, Some(exe)) => Some(shell::friendly_name(exe)),
            _ => None,
        }
        .unwrap_or_else(|| apps::title(HWND(hwnd as _)));
        self.names.insert(gid, name.clone());
        name
    }

    fn rebuild_items(&mut self) {
        let mut items = Vec::new();
        for (i, p) in self.cfg.pinned.clone().iter().enumerate() {
            let path = shell::expand_env(&p.path);
            let (key, default_name) = self.pin_info(&path);
            let launch = Launch { path: path.clone(), args: p.args.clone(), cwd: p.working_dir.clone() };
            items.push(Item::new(
                format!("p:{}", path.to_lowercase()),
                Kind::Pinned(i),
                p.name.clone().unwrap_or(default_name),
                path,
                key,
                Some(launch),
            ));
        }
        let pinned = items.len();

        let mut wins: Vec<(isize, AppKey, u64)> = self
            .windows
            .iter()
            .map(|(h, k)| (*h, k.clone(), self.keys.get(h).map(|(_, s)| *s).unwrap_or(0)))
            .collect();
        wins.sort_by_key(|w| w.2);
        let mut running: Vec<Item> = Vec::new();
        for (h, key, _) in wins {
            // Exact matches first; then pinned apps claiming their helper processes' windows.
            let owner = items[..pinned]
                .iter()
                .position(|it| it.key.matches(&key))
                .or_else(|| items[..pinned].iter().position(|it| it.key.adopts(&key)));
            if let Some(i) = owner {
                items[i].windows.push(h);
                continue;
            }
            if !self.cfg.dock.show_running {
                continue;
            }
            let gid = key.group_id();
            if let Some(it) = running.iter_mut().find(|it| it.id == gid) {
                it.windows.push(h);
                continue;
            }
            let name = self.running_name(&key, h);
            let source = match (&key.aumid, &key.exe) {
                (Some(a), _) if key.packaged => Some(format!("{}{a}", shell::APPS_FOLDER)),
                (_, Some(exe)) => Some(exe.clone()),
                _ => None,
            };
            let launch = source.clone().map(|path| Launch { path, args: None, cwd: None });
            let mut it = Item::new(gid, Kind::Running, name, source.unwrap_or_default(), key, launch);
            it.windows.push(h);
            running.push(it);
        }
        if pinned > 0 && !running.is_empty() {
            items.push(Item::new("sep".into(), Kind::Separator, String::new(), String::new(), AppKey::default(), None));
        }
        items.extend(running);
        let status = self.cfg.shows_status() && !self.status_glyphs.is_empty();
        let clock = self.cfg.shows_clock();
        if (status || clock) && !items.is_empty() && self.cfg.tray.placement == Placement::Dock {
            items.push(Item::new("sep:system".into(), Kind::Separator, String::new(), String::new(), AppKey::default(), None));
        }
        if status {
            items.push(Item::new("status".into(), Kind::Status, self.status_tip.clone(), String::new(), AppKey::default(), None));
        }
        if clock {
            items.push(Item::new("clock".into(), Kind::Clock, self.clock_tip.clone(), String::new(), AppKey::default(), None));
        }

        // Carry animation state across rebuilds; new items grow in, removed ones shrink out.
        let old = std::mem::take(&mut self.items);
        for it in &mut items {
            match old.iter().find(|o| o.id == it.id) {
                Some(o) => {
                    it.mag = o.mag;
                    it.appear = o.appear;
                    it.shift = o.shift;
                    it.bounce = o.bounce;
                    it.bounce_end = o.bounce_end;
                    // The app opened a window: finish the current hop and stop bouncing.
                    if let Some(t) = it.bounce.filter(|_| it.windows.len() > o.windows.len()) {
                        it.bounce_end = it.bounce_end.min(((t / BOUNCE_PERIOD).ceil() * BOUNCE_PERIOD).max(BOUNCE_PERIOD));
                    }
                }
                None => it.appear = if self.first_build { 1.0 } else { 0.0 },
            }
        }
        for (i, mut o) in old.into_iter().enumerate() {
            if o.appear > 0.0 && !items.iter().any(|n| n.id == o.id) {
                o.dying = true;
                o.windows.clear();
                items.insert(i.min(items.len()), o);
            }
        }
        self.items = items;
        self.first_build = false;
        self.request_icons();
        self.layout = self.compute_layout();
        self.dirty = true;
    }

    fn request_icons(&mut self) {
        for it in &self.items {
            if it.is_app() && !it.icon.is_empty() && !self.icons.contains_key(&it.icon) {
                self.loader.request(&it.icon, self.icon_px);
            }
        }
    }

    fn take_icons(&mut self) {
        for l in self.loader.take_loaded() {
            self.icons.insert(l.source, l.pixels);
        }
        self.dirty = true;
    }

    // --------------------------------------------------------------- layout

    /// Length along the dock, before magnification.
    fn base_size(&self, it: &Item) -> f32 {
        match it.kind {
            Kind::Separator => self.m.sep,
            Kind::Clock => self.clock_len,
            Kind::Status => self.status_glyphs.chars().count() as f32 * self.status_cell(),
            Kind::Pinned(_) | Kind::Running => self.m.icon,
        }
    }

    /// In the corner island rather than the dock itself.
    fn in_island(&self, it: &Item) -> bool {
        self.cfg.tray.placement == Placement::Corner && matches!(it.kind, Kind::Clock | Kind::Status)
    }

    /// Lays icons out at their current magnification, anchored so the point under the
    /// cursor stays put (the macOS behaviour) and centred when nothing is magnified.
    /// Items in the corner island are left out here and placed at the end of the edge.
    fn compute_layout(&self) -> Layout {
        let m = &self.m;
        let center = self.axis.main() / 2.0;
        let width = |it: &Item, mag: f32| if self.in_island(it) { 0.0 } else { it.appear * (self.base_size(it) * mag + m.gap) };
        let w0: Vec<f32> = self.items.iter().map(|it| width(it, 1.0)).collect();
        let w1: Vec<f32> = self.items.iter().map(|it| width(it, it.mag)).collect();
        let (l0, l1): (f32, f32) = (w0.iter().sum(), w1.iter().sum());
        let s0 = center - l0 / 2.0;
        let c = self.anchor;
        let start = if l0 <= 0.0 || c <= s0 {
            s0 - (l1 - l0) * if l0 <= 0.0 { 0.5 } else { 0.0 }
        } else if c >= s0 + l0 {
            s0 + l0 - l1
        } else {
            let (mut acc0, mut acc1, mut start) = (s0, 0.0, s0);
            for i in 0..w0.len() {
                if c < acc0 + w0[i] || i + 1 == w0.len() {
                    let t = if w0[i] > 0.0 { (c - acc0) / w0[i] } else { 0.0 };
                    start = c - (acc1 + t * w1[i]);
                    break;
                }
                acc0 += w0[i];
                acc1 += w1[i];
            }
            start
        };
        let mut u = start;
        let mut slots: Vec<Slot> = self
            .items
            .iter()
            .zip(&w1)
            .map(|(it, &w)| {
                let slot = Slot { u0: u, w, center: u + w / 2.0, size: self.base_size(it) * it.mag * it.appear };
                u += w;
                slot
            })
            .collect();
        // Each slot already has half a gap either side of its icon.
        let edge = m.pad_main - m.gap / 2.0;
        let bar = if l1 <= 0.0 {
            (center - m.icon / 2.0 - m.pad_main, center + m.icon / 2.0 + m.pad_main)
        } else {
            (start - edge, start + l1 + edge)
        };

        // The island sits as far from the end of the edge as the dock sits from the edge.
        let island_len: f32 = self.items.iter().filter(|it| self.in_island(it)).map(|it| it.appear * (self.base_size(it) + m.gap)).sum();
        let island = (island_len > 0.0).then(|| {
            let end = self.axis.main() - m.margin;
            let mut u = end - edge - island_len;
            for (it, slot) in self.items.iter().zip(&mut slots) {
                if self.in_island(it) {
                    let w = it.appear * (self.base_size(it) + m.gap);
                    *slot = Slot { u0: u, w, center: u + w / 2.0, size: self.base_size(it) * it.appear };
                    u += w;
                }
            }
            (end - island_len - 2.0 * edge, end)
        });
        Layout { slots, bar, island }
    }

    fn slide_distance(&self) -> f32 {
        self.hide * (self.m.margin + self.m.thick + 4.0 * self.m.s)
    }

    /// Cursor position in dock coordinates, compensating for the auto-hide slide.
    fn cursor_uv(&self) -> Option<(f32, f32)> {
        let (x, y) = self.cursor?;
        let (dx, dy) = self.axis.toward_edge(self.slide_distance());
        Some(self.axis.uv(x - dx, y - dy))
    }

    fn lift(&self, it: &Item) -> f32 {
        it.bounce.map_or(0.0, |t| (PI * t / BOUNCE_PERIOD).sin().abs() * self.m.bounce)
    }

    fn icon_uv(&self, i: usize) -> Option<(f32, f32, f32, f32)> {
        let (it, s) = (self.items.get(i)?, self.layout.slots.get(i)?);
        let cu = s.center + it.shift;
        if matches!(it.kind, Kind::Clock | Kind::Status) {
            // Text gets the whole thickness of the dock, so two lines fit beside small icons.
            return Some((cu - s.size / 2.0, cu + s.size / 2.0, self.m.margin, self.m.margin + self.m.thick));
        }
        let v0 = self.m.margin + self.m.pad_cross + self.lift(it);
        Some((cu - s.size / 2.0, cu + s.size / 2.0, v0, v0 + s.size))
    }

    fn hovering(&self) -> bool {
        let Some((u, v)) = self.cursor_uv() else { return false };
        if self.drag.as_ref().is_some_and(|d| !d.removing) {
            return true;
        }
        let on_bar = |(b0, b1): (f32, f32)| u >= b0 && u <= b1 && v <= self.m.margin + self.m.thick;
        if on_bar(self.layout.bar) || self.layout.island.is_some_and(on_bar) {
            return true;
        }
        (0..self.items.len()).any(|i| {
            self.icon_uv(i).is_some_and(|(u0, u1, v0, v1)| u >= u0 && u <= u1 && v >= v0 && v <= v1)
        })
    }

    fn hovered_index(&self) -> Option<usize> {
        if !self.hovering() {
            return None;
        }
        let (u, _) = self.cursor_uv()?;
        self.layout
            .slots
            .iter()
            .zip(&self.items)
            .position(|(s, it)| it.interactive() && u >= s.u0 && u < s.u0 + s.w)
    }

    // ------------------------------------------------------------ animation

    fn frame(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32().min(0.05);
        self.last = now;
        self.dirty = false;
        let speed = if self.cfg.animation.enabled { self.cfg.animation.speed } else { 1000.0 };
        let k = |rate: f32| 1.0 - (-dt * rate * speed).exp();
        let mut moving = approach(&mut self.hide, self.hide_target, k(9.0), 0.002);

        let removing = self.drag.as_ref().filter(|d| d.removing).map(|d| d.id.clone());
        for it in &mut self.items {
            let target = if it.dying || removing.as_deref() == Some(it.id.as_str()) { 0.0 } else { 1.0 };
            moving |= approach(&mut it.appear, target, k(12.0), 0.002);
            moving |= approach(&mut it.shift, 0.0, k(14.0), 0.1);
            if let Some(t) = &mut it.bounce {
                *t += dt * self.cfg.animation.speed;
                if *t >= it.bounce_end {
                    it.bounce = None;
                }
                moving = true;
            }
        }
        self.items.retain(|it| !(it.dying && it.appear <= 0.0));

        let hovering = self.hovering();
        if hovering
            && let Some((u, _)) = self.cursor_uv() {
                self.anchor = u;
            }
        let targets = self.magnification_targets(hovering);
        for (it, t) in self.items.iter_mut().zip(targets) {
            moving |= approach(&mut it.mag, t, k(16.0), 0.001);
        }
        if let Some((_, until)) = &self.notice
            && Instant::now() >= *until {
                self.notice = None;
            }

        self.layout = self.compute_layout();
        self.render();
        self.follow_hover_for_preview();
        self.animating = moving;
    }

    fn magnification_targets(&self, hovering: bool) -> Vec<f32> {
        let m = &self.m;
        let (cu, ok) = match self.cursor_uv() {
            Some((u, _)) if hovering && m.mag > 1.0 => (u, true),
            _ => (0.0, false),
        };
        // Distances are measured on the unmagnified layout so the effect doesn't feed back on itself.
        let width = |it: &Item| if self.in_island(it) { 0.0 } else { it.appear * (self.base_size(it) + m.gap) };
        let total: f32 = self.items.iter().map(width).sum();
        let mut u = self.axis.main() / 2.0 - total / 2.0;
        self.items
            .iter()
            .map(|it| {
                let w = width(it);
                let center = u + w / 2.0;
                u += w;
                if !ok || !it.is_app() {
                    return 1.0;
                }
                let d = (cu - center).abs() / (m.icon + m.gap);
                let f = if d < m.range { 0.5 * (1.0 + (PI * d / m.range).cos()) } else { 0.0 };
                1.0 + (m.mag - 1.0) * f
            })
            .collect()
    }

    fn render(&mut self) {
        let m = self.m;
        let ax = self.axis;
        let pal = self.palette;
        let offset = ax.toward_edge(self.slide_distance());
        let (b0, b1) = self.layout.bar;
        let bar = ax.rect(b0, b1, m.margin, m.margin + m.thick);
        let mut region = vec![ax.rect(b0, b1, 0.0, m.margin + m.thick)];
        let island = self.layout.island.map(|(i0, i1)| {
            region.push(ax.rect(i0, i1, 0.0, m.margin + m.thick));
            ax.rect(i0, i1, m.margin, m.margin + m.thick)
        });

        // Label: the hovered app's name, or a notice (e.g. a config error).
        let hovered = self
            .hovered_index()
            .filter(|&i| self.cfg.dock.show_labels && self.drag.is_none() && !self.menu_open && !self.preview.showing(&self.items[i].id));
        let label_req = match (&self.notice, hovered) {
            (Some((text, _)), _) => Some((text.clone(), (b0 + b1) / 2.0, m.margin + m.thick)),
            _ if self.dragging.is_some() => self.drop_label(),
            (None, Some(i)) => self.icon_uv(i).map(|(u0, u1, _, v1)| (self.items[i].name.clone(), (u0 + u1) / 2.0, v1)),
            _ => None,
        };
        let label = label_req.and_then(|(text, cu, top)| {
            let (tw, th) = self.gpu.as_mut()?.measure(&text, 4000.0)?;
            let pad = (10.0 * m.s, 4.0 * m.s);
            let (pw, ph) = (tw + 2.0 * pad.0, th + 2.0 * pad.1);
            let v0 = top + 8.0 * m.s;
            let rect = if ax.horizontal() {
                let u0 = (cu - pw / 2.0).clamp(4.0 * m.s, (ax.main() - pw - 4.0 * m.s).max(0.0));
                ax.rect(u0, u0 + pw, v0, v0 + ph)
            } else {
                ax.rect(cu - ph / 2.0, cu + ph / 2.0, v0, v0 + pw)
            };
            region.push(rect);
            let radius = rect.h().min(rect.w()) / 2.0;
            Some(LabelDraw { rect, radius, text, max_width: 4000.0, bg: pal.label_bg, fg: pal.label_text, pad })
        });

        let mut shapes = Vec::new();
        let mut icons = Vec::new();
        let mut texts = Vec::new();
        let hovered_any = if self.drag.is_none() && !self.menu_open { self.hovered_index() } else { None };
        let drag_id = self.drag.as_ref().map(|d| d.id.as_str());
        for (i, it) in self.items.iter().enumerate() {
            let Some(slot) = self.layout.slots.get(i) else { continue };
            let cu = slot.center + it.shift;
            if it.kind == Kind::Separator {
                let t = m.s.max(1.0);
                shapes.push(Shape {
                    rect: ax.rect(cu - t / 2.0, cu + t / 2.0, m.margin + m.pad_cross + m.icon * 0.15, m.margin + m.pad_cross + m.icon * 0.85),
                    radius: t / 2.0,
                    color: pal.separator.alpha(it.appear),
                });
                continue;
            }
            if matches!(it.kind, Kind::Clock | Kind::Status) {
                let Some((u0, u1, v0, v1)) = self.icon_uv(i) else { continue };
                // These don't magnify, so a soft highlight shows what's under the cursor.
                if hovered_any == Some(i) {
                    let inset = 4.0 * m.s;
                    shapes.push(Shape { rect: ax.rect(u0, u1, v0 + inset, v1 - inset), radius: 6.0 * m.s, color: pal.separator.alpha(0.6) });
                }
                if it.kind == Kind::Clock {
                    texts.push(TextDraw { rect: ax.rect(u0, u1, v0, v1), text: self.clock_text.clone(), color: pal.clock.alpha(it.appear), font: Font::Clock });
                } else {
                    let n = self.status_glyphs.chars().count().max(1) as f32;
                    let cell = (u1 - u0) / n;
                    for (k, g) in self.status_glyphs.chars().enumerate() {
                        let a = u0 + k as f32 * cell;
                        texts.push(TextDraw { rect: ax.rect(a, a + cell, v0, v1), text: g.to_string(), color: pal.status.alpha(it.appear), font: Font::Icons });
                    }
                }
                continue;
            }
            if drag_id != Some(it.id.as_str())
                && let Some((u0, u1, v0, v1)) = self.icon_uv(i) {
                    let rect = ax.rect(u0, u1, v0, v1);
                    region.push(rect);
                    icons.push(IconDraw { rect, source: &it.icon, opacity: it.appear.min(1.0) });
                }
            if it.windows.is_empty() || self.cfg.appearance.indicator == IndicatorStyle::None {
                continue;
            }
            let active = it.windows.contains(&self.fg);
            let col = if it.windows.iter().any(|w| self.attention.contains(w)) {
                pal.attention
            } else if active {
                pal.active
            } else {
                pal.indicator
            };
            let vc = m.margin + m.pad_cross * 0.45;
            match self.cfg.appearance.indicator {
                IndicatorStyle::Dot => {
                    let d = (m.icon * 0.08).max(3.0 * m.s);
                    let n = it.windows.len().min(3);
                    for k in 0..n {
                        let uc = cu + (k as f32 - (n - 1) as f32 / 2.0) * d * 1.9;
                        shapes.push(Shape { rect: ax.rect(uc - d / 2.0, uc + d / 2.0, vc - d / 2.0, vc + d / 2.0), radius: d / 2.0, color: col });
                    }
                }
                _ => {
                    let len = m.icon * if active { 0.42 } else { 0.2 };
                    let t = 3.0 * m.s;
                    shapes.push(Shape { rect: ax.rect(cu - len / 2.0, cu + len / 2.0, vc - t / 2.0, vc + t / 2.0), radius: t / 2.0, color: col });
                }
            }
        }
        // The icon being dragged follows the cursor.
        if let (Some(d), Some((x, y))) = (&self.drag, self.cursor)
            && let Some(it) = self.items.iter().find(|it| it.id == d.id) {
                let size = m.icon * if d.removing { 1.0 } else { it.mag };
                let rect = RectF { l: x - size / 2.0, t: y - size / 2.0, r: x + size / 2.0, b: y + size / 2.0 };
                icons.push(IconDraw { rect, source: &it.icon, opacity: if d.removing { 0.5 } else { 0.92 } });
            }

        let frame = Frame {
            offset,
            bar,
            bar_radius: m.radius,
            bar_fill: pal.background,
            bar_border: pal.border,
            island,
            blur: self.blur,
            placeholder: pal.placeholder,
            shapes,
            icons,
            labels: label.into_iter().collect(),
            texts,
        };
        if let (Some(g), Some(sf)) = (&mut self.gpu, &mut self.surface) {
            match g.draw(sf, &frame, &self.icons) {
                Ok(()) => self.render_failures = 0,
                Err(e) => {
                    crate::log!("draw failed: {e}; recreating renderer");
                    self.gpu = None;
                    self.surface = None;
                    self.render_failures += 1;
                }
            }
        }
        drop(frame);
        // e.g. the GPU was reset; retry a few times rather than spinning forever.
        if (self.gpu.is_none() || self.surface.is_none()) && self.render_failures < 5 {
            self.recreate_renderer();
        }
        self.update_region(region, offset);
    }

    fn set_fonts(&mut self) {
        let (a, s) = (&self.cfg.appearance, self.m.s);
        if let Some(g) = &mut self.gpu {
            g.set_label_font(&a.label_font, a.label_size * s);
            g.set_clock_font(&a.label_font, self.cfg.clock.size * s);
            g.set_icon_font(self.cfg.tray.icon_size * s);
        }
        self.update_clock();
    }

    // ----------------------------------------------------------------- clock

    /// Reads the time, measures it, and sets a timer for when it next changes.
    fn update_clock(&mut self) {
        self.kill_timer(TIMER_CLOCK);
        if !self.cfg.shows_clock() {
            return;
        }
        let face = clock::now(&self.cfg.clock);
        let m = self.m;
        let horizontal = self.axis.horizontal();
        let mut text = match &face.date {
            Some(d) => format!("{}\n{d}", face.time),
            None => face.time.clone(),
        };
        let mut len = m.icon;
        if let Some(g) = &mut self.gpu {
            let mut size = g.measure_clock(&text).unwrap_or_default();
            // Across a side dock there's only its thickness; keep the time and drop the date.
            if !horizontal && face.date.is_some() && size.0 > m.thick - 8.0 * m.s {
                text = face.time.clone();
                size = g.measure_clock(&text).unwrap_or_default();
            }
            len = if horizontal { size.0 + 8.0 * m.s } else { size.1 + 4.0 * m.s };
        }
        if text != self.clock_text || len != self.clock_len || face.tooltip != self.clock_tip {
            self.clock_text = text;
            self.clock_len = len;
            self.clock_tip = face.tooltip;
            if let Some(it) = self.items.iter_mut().find(|it| it.kind == Kind::Clock) {
                it.name = self.clock_tip.clone();
            }
            self.layout = self.compute_layout();
            self.dirty = true;
        }
        self.set_timer(TIMER_CLOCK, face.next_change.as_millis() as u32 + 15);
    }

    // ---------------------------------------------------------------- status

    /// Each status icon's share of the dock's length.
    fn status_cell(&self) -> f32 {
        (self.cfg.tray.icon_size * self.m.s * 1.75).round()
    }

    /// Reads volume, network and battery; starts or stops watching them as the setting changes.
    fn update_status(&mut self) {
        if !self.cfg.shows_status() {
            self.status = None;
            self.status_glyphs.clear();
            return;
        }
        let watcher = self.status.get_or_insert_with(|| status::Watcher::new(|| post(WM_APP_STATUS, 0)));
        let s = watcher.read();
        let t = &self.cfg.tray;
        let mut glyphs = String::new();
        if t.network {
            glyphs.push(status::network_glyph(&s.network));
        }
        if t.volume && let Some(v) = &s.volume {
            glyphs.push(status::volume_glyph(v));
        }
        if t.battery && let Some(b) = &s.battery {
            glyphs.push(status::battery_glyph(b));
        }
        let tip = status::describe(&s, t.volume, t.network, t.battery);
        self.has_battery = s.battery.is_some();
        if glyphs != self.status_glyphs || tip != self.status_tip {
            // A new icon (or one fewer) changes the item list; otherwise just redraw.
            let count_changed = glyphs.is_empty() != self.status_glyphs.is_empty();
            self.status_glyphs = glyphs;
            self.status_tip = tip;
            if let Some(it) = self.items.iter_mut().find(|it| it.kind == Kind::Status) {
                it.name = self.status_tip.clone();
            }
            if count_changed {
                self.rebuild_items();
            }
            self.layout = self.compute_layout();
            self.dirty = true;
        }
    }

    fn recreate_renderer(&mut self) {
        let made = Gpu::new(self.cfg.dock.gpu).and_then(|g| {
            let sf = g.create_surface(self.hwnd, self.axis.w as u32, self.axis.h as u32)?;
            Ok((g, sf))
        });
        match made {
            Ok((g, sf)) => {
                self.gpu = Some(g);
                self.surface = Some(sf);
                self.set_fonts();
                self.preview.reset_surface();
                self.close_preview();
                self.dirty = true;
            }
            Err(e) => crate::log!("renderer init failed: {e}"),
        }
    }

    /// Only the visible parts of the dock take mouse input; the rest of the window is click-through.
    fn update_region(&mut self, rects: Vec<RectF>, offset: (f32, f32)) {
        let ax = self.axis;
        let to_rect = |r: RectF| RECT { left: r.l.floor() as i32, top: r.t.floor() as i32, right: r.r.ceil() as i32, bottom: r.b.ceil() as i32 };
        let list: Vec<RECT> = if self.drag.is_some() || self.press.is_some() {
            vec![RECT { left: 0, top: 0, right: ax.w as i32, bottom: ax.h as i32 }]
        } else if self.cfg.dock.autohide && self.hide_target >= 1.0 && self.hide > 0.95 {
            // Fully hidden: a thin strip along the screen edge to reveal it.
            let strip = (2.0 * self.m.s).max(2.0);
            std::iter::once(self.layout.bar).chain(self.layout.island).map(|(b0, b1)| to_rect(ax.rect(b0, b1, 0.0, strip))).collect()
        } else {
            rects.into_iter().map(|r| to_rect(r.translate(offset.0, offset.1).inflate(1.0))).collect()
        };
        if list == self.region {
            return;
        }
        unsafe {
            let rgn = CreateRectRgn(0, 0, 0, 0);
            for r in &list {
                let part = CreateRectRgn(r.left, r.top, r.right, r.bottom);
                CombineRgn(Some(rgn), Some(rgn), Some(part), RGN_OR);
                let _ = DeleteObject(part.into());
            }
            SetWindowRgn(self.hwnd, Some(rgn), true);
        }
        self.region = list;
    }

    // ---------------------------------------------------------------- input

    fn track_leave(&mut self) {
        let mut tme = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: self.hwnd,
            dwHoverTime: 0,
        };
        unsafe {
            let _ = TrackMouseEvent(&mut tme);
        }
        self.tracking = true;
    }

    fn schedule_hide(&self, ms: u32) {
        if self.cfg.dock.autohide {
            self.set_timer(TIMER_HIDE, ms.max(1));
        }
    }

    fn on_move(&mut self, x: f32, y: f32) {
        self.cursor = Some((x, y));
        if !self.tracking {
            self.track_leave();
        }
        self.kill_timer(TIMER_HIDE);
        self.hide_target = 0.0;
        if let Some(p) = &self.press {
            let moved = (x - p.x).hypot(y - p.y) > 6.0 * self.m.s;
            if self.drag.is_none() && moved {
                let pinned = p.id.as_ref().and_then(|id| self.items.iter().find(|it| &it.id == id && matches!(it.kind, Kind::Pinned(_))));
                if let Some(it) = pinned {
                    self.drag = Some(Drag { id: it.id.clone(), original: self.cfg.pinned.clone(), removing: false });
                }
            }
        }
        if self.drag.is_some() {
            self.drag_update();
        }
        self.dirty = true;
    }

    fn on_leave(&mut self) {
        self.tracking = false;
        if self.press.is_some() || self.drag.is_some() || self.menu_open {
            return;
        }
        self.cursor = None;
        self.preview_dock_left();
        self.schedule_hide(self.cfg.dock.autohide_delay_ms);
        self.dirty = true;
    }

    fn on_down(&mut self, x: f32, y: f32) {
        self.cursor = Some((x, y));
        let id = self.hovered_index().map(|i| self.items[i].id.clone());
        self.close_preview();
        self.press = Some(Press { id, x, y });
        unsafe {
            SetCapture(self.hwnd);
        }
    }

    fn on_up(&mut self, x: f32, y: f32) {
        self.cursor = Some((x, y));
        let press = self.press.take();
        unsafe {
            let _ = ReleaseCapture();
        }
        if self.drag.is_some() {
            self.finish_drag();
        } else if let Some(id) = press.and_then(|p| p.id)
            && let Some(i) = self.hovered_index().filter(|&i| self.items[i].id == id) {
                self.click(i);
            }
        // Re-arm leave tracking; it reports right away if the cursor is already outside.
        self.track_leave();
        self.dirty = true;
    }

    fn click(&mut self, i: usize) {
        match self.items[i].kind {
            Kind::Clock => {
                shell::launch("ms-actioncenter:", None, None);
                return;
            }
            Kind::Status => {
                shell::open_quick_settings();
                return;
            }
            _ => {}
        }
        let wins = self.items[i].windows.clone();
        if wins.is_empty() {
            self.launch(i);
            return;
        }
        let fg = apps::foreground().0 as isize;
        if let Some(p) = wins.iter().position(|&w| w == fg) {
            if wins.len() > 1 {
                apps::activate(HWND(wins[(p + 1) % wins.len()] as _));
            } else if self.cfg.dock.click_minimizes {
                apps::minimize(HWND(fg as _));
            }
        } else {
            // Bring back the one used most recently (highest in z-order).
            let best = self.windows.iter().map(|(h, _)| *h).find(|h| wins.contains(h)).unwrap_or(wins[0]);
            apps::activate(HWND(best as _));
        }
    }

    fn launch(&mut self, i: usize) {
        let Some(l) = self.items[i].launch.clone() else { return };
        if shell::launch(&l.path, l.args.as_deref(), l.cwd.as_deref()) && self.cfg.animation.bounce_on_launch {
            let it = &mut self.items[i];
            it.bounce = Some(0.0);
            it.bounce_end = BOUNCE_PERIOD * 3.0;
            self.dirty = true;
        }
    }

    fn pinned_index(&self, id: &str) -> Option<usize> {
        self.items.iter().find(|it| it.id == id).and_then(|it| match it.kind {
            Kind::Pinned(i) => Some(i),
            _ => None,
        })
    }

    fn drag_update(&mut self) {
        let (Some((x, y)), Some(d)) = (self.cursor, &self.drag) else { return };
        let id = d.id.clone();
        let (u, v) = self.axis.uv(x, y);
        let m = self.m;
        let removing = v > m.margin + m.thick + m.icon * 1.2 || v < -m.icon || u < 0.0 || u > self.axis.main();
        if let Some(d) = &mut self.drag {
            d.removing = removing;
        }
        let Some(from) = self.pinned_index(&id) else { return };
        if removing {
            return;
        }
        // Drop target: the pinned slot whose centre is closest to the cursor.
        let target = self
            .items
            .iter()
            .zip(&self.layout.slots)
            .filter_map(|(it, s)| match it.kind {
                Kind::Pinned(j) if !it.dying => Some((j, (s.center - u).abs())),
                _ => None,
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(j, _)| j)
            .unwrap_or(from);
        if target == from {
            return;
        }
        let width = self
            .items
            .iter()
            .position(|it| it.id == id)
            .and_then(|i| self.layout.slots.get(i))
            .map_or(m.icon + m.gap, |s| s.w);
        // Items the dragged icon passes over slide out of its way.
        let (range, shift) = if target > from { (from + 1..=target, width) } else { (target..=from - 1, -width) };
        let moved: Vec<String> = self
            .items
            .iter()
            .filter(|it| matches!(it.kind, Kind::Pinned(j) if range.contains(&j)) && !it.dying)
            .map(|it| it.id.clone())
            .collect();
        let p = self.cfg.pinned.remove(from);
        self.cfg.pinned.insert(target, p);
        self.rebuild_items();
        for it in &mut self.items {
            if moved.contains(&it.id) {
                it.shift += shift;
            }
        }
    }

    fn finish_drag(&mut self) {
        let Some(d) = self.drag.take() else { return };
        if d.removing
            && let Some(i) = self.pinned_index(&d.id) {
                self.cfg.pinned.remove(i);
            }
        if self.cfg.pinned != d.original {
            self.save_pins();
        }
        self.rebuild_items();
    }

    fn cancel_drag(&mut self) {
        if let Some(d) = self.drag.take() {
            self.cfg.pinned = d.original;
            self.press = None;
            self.rebuild_items();
        }
    }

    // ----------------------------------------------------------------- menu

    fn menu_spec(&mut self, x: f32, y: f32) -> MenuSpec {
        self.cursor = Some((x, y));
        self.close_preview();
        let idx = self.hovered_index();
        let mut entries = Vec::new();
        let mut windows = Vec::new();
        if let Some(it) = idx.map(|i| &self.items[i]) {
            // With several windows open, list them so any one is a click away.
            if it.windows.len() > 1 {
                let fg = apps::foreground().0 as isize;
                for (n, &w) in it.windows.iter().enumerate() {
                    let mut title = apps::title(HWND(w as _));
                    if title.chars().count() > 60 {
                        title = title.chars().take(59).collect::<String>() + "…";
                    }
                    // `&` marks a keyboard shortcut in menus; `&&` is a literal ampersand.
                    let label = title.replace('&', "&&");
                    entries.push(MenuEntry { id: CMD_WINDOW + n as u32, label, checked: w == fg });
                    windows.push(w);
                }
                entries.push(MenuEntry::separator());
            }
            if it.launch.is_some() {
                entries.push(MenuEntry::new(CMD_OPEN, if it.windows.is_empty() { "Open" } else { "New window" }));
            }
            match it.kind {
                Kind::Pinned(_) => entries.push(MenuEntry::new(CMD_UNPIN, "Remove from dock")),
                Kind::Running if it.launch.is_some() => entries.push(MenuEntry::new(CMD_PIN, "Keep in dock")),
                _ => {}
            }
            if it.kind == Kind::Clock {
                entries.push(MenuEntry::new(CMD_NOTIFICATIONS, "Notifications and calendar"));
                entries.push(MenuEntry::new(CMD_DATE_TIME, "Adjust date and time"));
            }
            if it.kind == Kind::Status {
                entries.push(MenuEntry::new(CMD_SOUND, "Sound settings"));
                entries.push(MenuEntry::new(CMD_MIXER, "Volume mixer"));
                entries.push(MenuEntry::new(CMD_NETWORK, "Network settings"));
                if self.has_battery {
                    entries.push(MenuEntry::new(CMD_POWER, "Power & battery"));
                }
            }
            match it.windows.len() {
                0 => {}
                1 => entries.push(MenuEntry::new(CMD_CLOSE, "Close window")),
                n => entries.push(MenuEntry::new(CMD_CLOSE, format!("Close all {n} windows"))),
            }
            entries.push(MenuEntry::separator());
        }
        entries.push(MenuEntry::new(CMD_IMPORT, "Import taskbar pins"));
        entries.push(MenuEntry::new(CMD_SETTINGS, "Dock settings…"));
        entries.push(MenuEntry::new(CMD_QUIT, "Quit Windock"));
        self.menu_open = true;
        self.dirty = true;
        let align = match self.cfg.dock.position {
            Position::Bottom => TPM_BOTTOMALIGN | TPM_CENTERALIGN,
            Position::Top => TPM_TOPALIGN | TPM_CENTERALIGN,
            Position::Left => TPM_LEFTALIGN | TPM_VCENTERALIGN,
            Position::Right => TPM_RIGHTALIGN | TPM_VCENTERALIGN,
        };
        MenuSpec { item: idx.map(|i| self.items[i].id.clone()), entries, windows, align }
    }

    fn menu_command(&mut self, spec: &MenuSpec, cmd: u32) {
        self.menu_open = false;
        self.cursor = None;
        self.tracking = false;
        self.dirty = true;
        let idx = spec.item.as_ref().and_then(|id| self.items.iter().position(|it| &it.id == id && !it.dying));
        match (cmd, idx) {
            (CMD_OPEN, Some(i)) => self.launch(i),
            (CMD_PIN, Some(i)) => {
                if let Some(l) = self.items[i].launch.clone() {
                    self.cfg.pinned.push(Pinned { path: l.path, ..Default::default() });
                    self.save_pins();
                    self.rebuild_items();
                }
            }
            (CMD_UNPIN, Some(i)) => {
                if let Kind::Pinned(p) = self.items[i].kind {
                    self.cfg.pinned.remove(p);
                    self.save_pins();
                    self.rebuild_items();
                }
            }
            (CMD_CLOSE, Some(i)) => self.items[i].windows.iter().for_each(|&w| apps::close(HWND(w as _))),
            (CMD_IMPORT, _) => {
                let text = match self.import_taskbar_pins() {
                    0 => "Everything pinned to the taskbar is already here".to_string(),
                    1 => "Added 1 app from the taskbar".to_string(),
                    n => format!("Added {n} apps from the taskbar"),
                };
                self.show_notice(text, INFO_TIME);
            }
            (CMD_SETTINGS, _) => open_settings(),
            (CMD_NOTIFICATIONS, _) => {
                shell::launch("ms-actioncenter:", None, None);
            }
            (CMD_DATE_TIME, _) => {
                shell::launch("ms-settings:dateandtime", None, None);
            }
            (CMD_SOUND | CMD_MIXER | CMD_NETWORK | CMD_POWER, _) => {
                let page = match cmd {
                    CMD_SOUND => "ms-settings:sound",
                    CMD_MIXER => "ms-settings:apps-volume",
                    CMD_NETWORK => "ms-settings:network",
                    _ => "ms-settings:batterysaver",
                };
                shell::launch(page, None, None);
            }
            (c, _) if c >= CMD_WINDOW => {
                if let Some(&w) = spec.windows.get((c - CMD_WINDOW) as usize) {
                    apps::activate(HWND(w as _));
                }
            }
            (CMD_QUIT, _) => unsafe {
                let _ = DestroyWindow(self.hwnd);
            },
            _ => {}
        }
        self.schedule_hide(self.cfg.dock.autohide_delay_ms);
    }

    // -------------------------------------------------------------- messages

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let x = (lp.0 & 0xffff) as i16 as f32;
        let y = ((lp.0 >> 16) & 0xffff) as i16 as f32;
        match msg {
            WM_MOUSEMOVE => self.on_move(x, y),
            WM_MOUSELEAVE => self.on_leave(),
            WM_LBUTTONDOWN => self.on_down(x, y),
            WM_LBUTTONUP => self.on_up(x, y),
            WM_MBUTTONUP => {
                self.cursor = Some((x, y));
                match self.hovered_index() {
                    Some(i) if self.items[i].kind == Kind::Status => {
                        if let Some(w) = &self.status {
                            w.toggle_mute();
                        }
                    }
                    Some(i) => self.launch(i),
                    None => {}
                }
            }
            // Scrolling over the status icons changes the volume, 2% a notch.
            WM_MOUSEWHEEL => {
                let notches = ((wp.0 >> 16) as u16 as i16) as f32 / WHEEL_DELTA as f32;
                match (self.hovered_index(), &self.status) {
                    (Some(i), Some(w)) if self.items[i].kind == Kind::Status => w.change_volume(notches * 0.02),
                    _ => return None,
                }
            }
            WM_CAPTURECHANGED => {
                if lp.0 != self.hwnd.0 as isize && self.drag.is_some() {
                    self.cancel_drag();
                }
            }
            WM_MOUSEACTIVATE => return Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_HOTKEY => self.on_hotkey(wp.0 as i32),
            WM_APP_DROP => self.finish_drop(),
            WM_TIMER => {
                self.kill_timer(wp.0);
                match wp.0 {
                    TIMER_REFRESH => self.refresh_windows(),
                    TIMER_CONFIG => self.reload_config(),
                    preview::TIMER_PREVIEW => self.preview_timer(),
                    preview::TIMER_PREVIEW_CLOSE => self.close_preview(),
                    TIMER_HIDE => {
                        if self.cursor.is_none() && self.drag.is_none() && !self.menu_open && self.notice.is_none() && !self.preview.is_open() {
                            self.hide_target = 1.0;
                            self.dirty = true;
                        }
                    }
                    TIMER_CLOCK => self.update_clock(),
                    TIMER_STATUS => self.update_status(),
                    TIMER_NOTICE => {
                        self.notice = None;
                        self.dirty = true;
                        self.schedule_hide(self.cfg.dock.autohide_delay_ms);
                    }
                    _ => {}
                }
            }
            WM_APP_REFRESH => self.set_timer(TIMER_REFRESH, 60),
            WM_APP_CONFIG => self.set_timer(TIMER_CONFIG, 150),
            WM_APP_ICONS => self.take_icons(),
            WM_APP_FOREGROUND => {
                let h = wp.0 as isize;
                self.fg = h;
                self.attention.remove(&h);
                if !self.windows.iter().any(|(w, _)| *w == h) {
                    self.set_timer(TIMER_REFRESH, 60);
                }
                self.check_fullscreen();
                self.dirty = true;
            }
            WM_APP_APPBAR => match wp.0 as u32 {
                ABN_POSCHANGED => self.place(),
                ABN_FULLSCREENAPP => self.check_fullscreen(),
                _ => {}
            },
            // Explorer shows the taskbar again now and then, e.g. for a new monitor.
            WM_APP_TASKBAR_SHOWN => {
                if self.cfg.dock.hide_taskbar {
                    taskbar::hide();
                }
            }
            // Signing out: put the taskbar's auto-hide setting back, as quitting would.
            WM_ENDSESSION if wp.0 != 0 => taskbar::restore(),
            WM_DISPLAYCHANGE | WM_DPICHANGED => self.place(),
            WM_TIMECHANGE => self.update_clock(),
            // Changes arrive in bursts (e.g. a volume slider dragging); read once they settle.
            WM_APP_STATUS => self.set_timer(TIMER_STATUS, 100),
            WM_POWERBROADCAST => {
                if wp.0 as u32 == PBT_APMPOWERSTATUSCHANGE {
                    self.set_timer(TIMER_STATUS, 100);
                }
                return None;
            }
            WM_SETTINGCHANGE => {
                let area = if lp.0 != 0 { unsafe { PCWSTR(lp.0 as _).to_string() }.unwrap_or_default() } else { String::new() };
                if wp.0 as u32 == SPI_SETWORKAREA.0 {
                    self.place();
                } else if area == "ImmersiveColorSet" {
                    self.apply_theme();
                } else if area == "intl" {
                    // Regional formats changed.
                    self.update_clock();
                }
                return None;
            }
            WM_PAINT => unsafe {
                let _ = ValidateRect(Some(self.hwnd), None);
                self.dirty = true;
            },
            WM_ERASEBKGND => return Some(LRESULT(1)),
            WM_DESTROY => unsafe { PostQuitMessage(0) },
            m if m != 0 && m == SHELLHOOK_MSG.load(Ordering::Relaxed) => match wp.0 as u32 {
                HSHELL_WINDOWCREATED | HSHELL_WINDOWDESTROYED => self.set_timer(TIMER_REFRESH, 60),
                HSHELL_FLASH => {
                    let h = lp.0;
                    if self.attention.insert(h) && self.cfg.animation.bounce_on_launch
                        && let Some(it) = self.items.iter_mut().find(|it| it.windows.contains(&h)) {
                            it.bounce = Some(0.0);
                            it.bounce_end = BOUNCE_PERIOD * 2.0;
                        }
                    self.dirty = true;
                }
                _ => {}
            },
            m if m != 0 && m == TASKBAR_CREATED_MSG.load(Ordering::Relaxed) => {
                // Explorer restarted: our app bar registration is gone.
                self.reserved = None;
                unsafe {
                    let _ = RegisterShellHookWindow(self.hwnd);
                }
                self.appbar_register();
                self.apply_taskbar();
                self.place();
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

/// The settings window runs as a separate process so the dock stays lean.
fn open_settings() {
    match std::env::current_exe().and_then(|exe| std::process::Command::new(exe).arg("--settings").spawn()) {
        Ok(_) => {}
        Err(e) => crate::log!("couldn't open settings: {e}"),
    }
}

fn config_error(e: &str) -> String {
    format!("Config error: {}", e.lines().next().unwrap_or(e))
}

/// All monitors, left to right (the order `monitor = N` in the config refers to).
pub(crate) fn monitors() -> Vec<(HMONITOR, MONITORINFO)> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        unsafe { (*(lp.0 as *mut Vec<HMONITOR>)).push(h) };
        true.into()
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut handles as *mut _ as isize));
    }
    let mut out: Vec<(HMONITOR, MONITORINFO)> = handles
        .into_iter()
        .filter_map(|h| {
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            unsafe { GetMonitorInfoW(h, &mut mi) }.as_bool().then_some((h, mi))
        })
        .collect();
    out.sort_by_key(|(_, mi)| (mi.rcMonitor.left, mi.rcMonitor.top));
    out
}

/// Posts a message whenever a file in the config folder changes.
fn watch_config(dir: PathBuf) {
    std::thread::spawn(move || unsafe {
        let Ok(handle) = FindFirstChangeNotificationW(
            &HSTRING::from(dir.as_os_str()),
            false,
            FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_FILE_NAME,
        ) else {
            crate::log!("cannot watch {}", dir.display());
            return;
        };
        while WaitForSingleObject(handle, INFINITE) == WAIT_OBJECT_0 {
            post(WM_APP_CONFIG, 0);
            if FindNextChangeNotification(handle).is_err() {
                break;
            }
        }
    });
}

unsafe extern "system" fn win_event(_: HWINEVENTHOOK, event: u32, hwnd: HWND, id_object: i32, id_child: i32, _: u32, _: u32) {
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || hwnd.is_invalid() {
        return;
    }
    let id = hwnd.0 as isize;
    let tracked = TRACKED.with(|t| t.borrow().contains(&id));
    match event {
        EVENT_SYSTEM_FOREGROUND => post(WM_APP_FOREGROUND, id as usize),
        EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED if tracked => post(WM_APP_REFRESH, 0),
        EVENT_OBJECT_SHOW if taskbar::is_taskbar(hwnd) => post(WM_APP_TASKBAR_SHOWN, 0),
        EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED | EVENT_OBJECT_NAMECHANGE if !tracked => unsafe {
            if GetAncestor(hwnd, GA_ROOT) == hwnd && IsWindowVisible(hwnd).as_bool() {
                post(WM_APP_REFRESH, 0);
            }
        },
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_RBUTTONUP {
        let (x, y) = ((lp.0 & 0xffff) as i16 as f32, ((lp.0 >> 16) & 0xffff) as i16 as f32);
        // The menu runs its own message loop, so the app must not stay borrowed while it's open.
        if let Some(spec) = with_app(|a| a.menu_spec(x, y)) {
            let cmd = unsafe { show_menu(hwnd, &spec) };
            with_app(|a| a.menu_command(&spec, cmd));
        }
        return LRESULT(0);
    }
    if let Some(Some(r)) = with_app(|a| a.handle(msg, wp, lp)) {
        return r;
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

unsafe fn show_menu(hwnd: HWND, spec: &MenuSpec) -> u32 {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        for e in &spec.entries {
            if e.id == 0 {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            } else {
                let text = wide(&e.label);
                let flags = if e.checked { MF_STRING | MF_CHECKED } else { MF_STRING };
                let _ = AppendMenuW(menu, flags, e.id as usize, PCWSTR(text.as_ptr()));
            }
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | spec.align, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0 as u32
    }
}

pub fn run() -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("WindockDock");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            // Resource 1 is the icon build.rs embeds.
            hIcon: LoadIconW(Some(instance.into()), PCWSTR(1 as _)).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class,
            w!("Windock"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        DOCK_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
        // Windows.UI.Composition needs a dispatcher queue on the UI thread.
        let _queue = CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_NONE,
        })?;
        let app = App::new(hwnd);
        APP.with(|a| *a.borrow_mut() = Some(app));
        with_app(|a| a.start());
        message_loop();
        with_app(|a| a.shutdown());
        APP.with(|a| a.borrow_mut().take());
    }
    Ok(())
}

/// Blocks in GetMessage while idle; while animating, renders once per display refresh.
fn message_loop() {
    let mut msg = MSG::default();
    unsafe {
        loop {
            if with_app(|a| a.wants_frame()).unwrap_or(false) {
                with_app(|a| a.frame());
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        return;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if DwmFlush().is_err() {
                    std::thread::sleep(Duration::from_millis(8));
                }
            } else {
                if GetMessageW(&mut msg, None, 0, 0).0 <= 0 {
                    return;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
