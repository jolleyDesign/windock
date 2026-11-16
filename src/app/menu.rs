//! The dock's right-click menu, drawn like the dock rather than as a Windows menu.

use super::*;
use crate::render::Surface;
use windows::Win32::UI::Input::KeyboardAndMouse::{VIRTUAL_KEY, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_SPACE, VK_UP};

pub(super) struct MenuEntry {
    /// 0 for a separator.
    pub(super) id: u32,
    pub(super) label: String,
    /// Marked with a dot: the window in front, in a list of an app's windows.
    pub(super) checked: bool,
}

impl MenuEntry {
    pub(super) fn new(id: u32, label: impl Into<String>) -> Self {
        Self { id, label: label.into(), checked: false }
    }

    /// Adds a separator unless `entries` is empty or already ends with one.
    pub(super) fn separate(entries: &mut Vec<MenuEntry>) {
        if entries.last().is_some_and(|e| e.id != 0) {
            entries.push(Self::new(0, ""));
        }
    }
}

pub(super) struct MenuSpec {
    /// The item that was right-clicked.
    pub(super) item: Option<String>,
    pub(super) entries: Vec<MenuEntry>,
    pub(super) windows: Vec<isize>,
    /// Where the menu opens from, in dock coordinates (see `Axis`).
    pub(super) anchor: (f32, f32),
}

pub(super) struct Menu {
    hwnd: HWND,
    surface: Option<Surface>,
    spec: Option<MenuSpec>,
    /// Each entry's row; separators' rows are their thin line's slot.
    rows: Vec<RectF>,
    size: (f32, f32),
    /// Where the labels start.
    text_left: f32,
    hover: Option<usize>,
    tracking: bool,
}

impl Menu {
    pub(super) fn new(hwnd: HWND) -> Self {
        Self { hwnd, surface: None, spec: None, rows: Vec::new(), size: (0.0, 0.0), text_left: 0.0, hover: None, tracking: false }
    }

    pub(super) fn is_open(&self) -> bool {
        self.spec.is_some()
    }

    /// The renderer was recreated; surfaces from the old one are dead.
    pub(super) fn reset_surface(&mut self) {
        self.surface = None;
    }

    /// The entry under (x, y), skipping separators.
    fn entry_at(&self, x: f32, y: f32) -> Option<usize> {
        let entries = &self.spec.as_ref()?.entries;
        self.rows.iter().position(|r| r.contains(x, y)).filter(|&k| entries[k].id != 0)
    }

    /// The next entry up or down from the hovered one, wrapping around.
    fn step(&self, down: bool) -> Option<usize> {
        let entries = &self.spec.as_ref()?.entries;
        let n = entries.len();
        let start = self.hover.unwrap_or(if down { n - 1 } else { 0 });
        (1..=n).map(|d| if down { (start + d) % n } else { (start + n - d) % n }).find(|&k| entries[k].id != 0)
    }
}

pub(super) fn create_window(instance: HINSTANCE) -> Result<HWND> {
    unsafe {
        let class = w!("WindockMenu");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(menu_proc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        // It takes the focus while open, so clicking anywhere else closes it and the keys work.
        let hwnd =
            CreateWindowExW(WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_TOPMOST, class, w!("Windock menu"), WS_POPUP, 0, 0, 1, 1, None, None, Some(instance), None)?;
        let on = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(17), &on as *const _ as _, 4);
        Ok(hwnd)
    }
}

unsafe extern "system" fn menu_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(Some(r)) = with_app(|a| a.menu_message(msg, wp, lp)) {
        return r;
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

impl App {
    pub(super) fn open_menu(&mut self, spec: MenuSpec) {
        self.close_flyout();
        self.menu.spec = Some(spec);
        self.menu.hover = None;
        self.layout_menu();
        self.position_menu();
        self.draw_menu();
        unsafe {
            let _ = SetForegroundWindow(self.menu.hwnd);
        }
        self.dirty = true;
    }

    /// Closes the menu, handing back what it was showing.
    pub(super) fn close_menu(&mut self) -> Option<MenuSpec> {
        let spec = self.menu.spec.take()?;
        self.menu.hover = None;
        self.menu.tracking = false;
        unsafe {
            let _ = ShowWindow(self.menu.hwnd, SW_HIDE);
        }
        // The dock ignored the mouse leaving while the menu was open; start afresh.
        self.cursor = None;
        self.tracking = false;
        self.dirty = true;
        self.schedule_hide(self.cfg.dock.autohide_delay_ms);
        Some(spec)
    }

    fn layout_menu(&mut self) {
        let s = self.m.s;
        let Some(spec) = &self.menu.spec else { return };
        let pad = 5.0 * s;
        let text_h = self.gpu.as_mut().and_then(|g| g.measure("Ag", 1000.0)).map_or(16.0 * s, |(_, h)| h);
        let row_h = (text_h + 14.0 * s).round();
        let sep_h = (9.0 * s).round();
        // Room for the dot beside the window in front.
        let dots = spec.entries.iter().any(|e| e.checked);
        let text_left = pad + if dots { 26.0 * s } else { 12.0 * s };
        let max_text = 360.0 * s;
        let widest = spec.entries.iter().filter_map(|e| self.gpu.as_mut()?.measure(&e.label, max_text)).map(|(w, _)| w).fold(0.0, f32::max);
        let w = (text_left + widest + 24.0 * s + pad).max(180.0 * s).ceil();
        let mut y = pad;
        self.menu.rows = spec
            .entries
            .iter()
            .map(|e| {
                let h = if e.id == 0 { sep_h } else { row_h };
                let r = RectF::new(pad, y, w - 2.0 * pad, h);
                y += h;
                r
            })
            .collect();
        self.menu.size = (w, (y + pad).ceil());
        self.menu.text_left = text_left;
    }

    fn position_menu(&mut self) {
        let Some((u, v)) = self.menu.spec.as_ref().map(|sp| sp.anchor) else { return };
        let (w, h) = self.menu.size;
        let (x, y) = self.place_beside(u, v, w, h);
        let (x, y, wi, hi) = (x.round() as i32, y.round() as i32, w as i32, h as i32);
        unsafe {
            let _ = SetWindowPos(self.menu.hwnd, Some(HWND_TOPMOST), x, y, wi, hi, SWP_SHOWWINDOW);
        }
        if let Some(sf) = &mut self.menu.surface {
            let _ = sf.resize(wi as u32, hi as u32);
        } else if let Some(gpu) = &self.gpu {
            self.menu.surface = gpu.create_surface(self.menu.hwnd, wi as u32, hi as u32).inspect_err(|e| crate::log!("menu surface: {e}")).ok();
        }
    }

    fn draw_menu(&mut self) {
        let Some(spec) = &self.menu.spec else { return };
        let s = self.m.s;
        let pal = self.palette;
        let m = &self.menu;
        let mut shapes = Vec::new();
        let mut texts = Vec::new();
        for (k, (e, r)) in spec.entries.iter().zip(&m.rows).enumerate() {
            if e.id == 0 {
                let y = ((r.t + r.b) / 2.0).floor();
                shapes.push(Shape { rect: RectF::new(r.l + 8.0 * s, y, r.w() - 16.0 * s, s.max(1.0).round()), radius: 0.0, color: pal.label_text.alpha(0.12) });
                continue;
            }
            if m.hover == Some(k) {
                shapes.push(Shape { rect: *r, radius: 5.0 * s, color: pal.label_text.alpha(0.1) });
            }
            let cy = (r.t + r.b) / 2.0;
            if e.checked {
                let d = 6.0 * s;
                shapes.push(Shape { rect: RectF::new(r.l + 11.0 * s, cy - d / 2.0, d, d), radius: d / 2.0, color: pal.label_text });
            }
            let Some((_, th)) = self.gpu.as_mut().and_then(|g| g.measure(&e.label, 1000.0)) else { continue };
            let l = m.text_left;
            texts.push(TextDraw { rect: RectF::new(l, (cy - th / 2.0).round(), r.r - 12.0 * s - l, th), text: e.label.clone(), color: pal.label_text, font: Font::Label });
        }
        let frame = Frame {
            texts,
            offset: (0.0, 0.0),
            bar: RectF::new(0.0, 0.0, m.size.0, m.size.1),
            bar_radius: 8.0 * s,
            bar_fill: Color { a: pal.background.a.max(0.85), ..pal.background },
            bar_border: pal.border,
            island: None,
            blur: self.blur,
            placeholder: pal.placeholder,
            shapes,
            icons: Vec::new(),
            labels: Vec::new(),
        };
        if let (Some(g), Some(sf)) = (&mut self.gpu, &mut self.menu.surface)
            && let Err(e) = g.draw(sf, &frame, &self.icons)
        {
            crate::log!("menu draw failed: {e}");
        }
    }

    fn set_menu_hover(&mut self, hover: Option<usize>) {
        if hover != self.menu.hover {
            self.menu.hover = hover;
            self.draw_menu();
        }
    }

    /// Carries out the hovered entry.
    fn choose_menu_entry(&mut self) {
        let Some(k) = self.menu.hover else { return };
        // Closed first, so the command's window can take the focus from it.
        let Some(spec) = self.close_menu() else { return };
        let cmd = spec.entries[k].id;
        self.menu_command(&spec, cmd);
    }

    pub(super) fn menu_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let x = (lp.0 & 0xffff) as i16 as f32;
        let y = ((lp.0 >> 16) & 0xffff) as i16 as f32;
        match msg {
            WM_MOUSEMOVE => {
                if !self.menu.tracking {
                    let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.menu.hwnd, dwHoverTime: 0 };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                    self.menu.tracking = true;
                }
                let hover = self.menu.entry_at(x, y);
                self.set_menu_hover(hover);
            }
            WM_MOUSELEAVE => {
                self.menu.tracking = false;
                self.set_menu_hover(None);
            }
            WM_LBUTTONUP | WM_RBUTTONUP => {
                let hover = self.menu.entry_at(x, y);
                self.set_menu_hover(hover);
                self.choose_menu_entry();
            }
            WM_KEYDOWN => match VIRTUAL_KEY(wp.0 as u16) {
                VK_ESCAPE => {
                    self.close_menu();
                }
                key @ (VK_UP | VK_DOWN) => {
                    let next = self.menu.step(key == VK_DOWN);
                    self.set_menu_hover(next);
                }
                VK_RETURN | VK_SPACE => self.choose_menu_entry(),
                _ => return None,
            },
            // Clicking anywhere else, or switching apps, closes it.
            WM_ACTIVATE if wp.0 & 0xffff == WA_INACTIVE as usize => {
                self.close_menu();
            }
            WM_ERASEBKGND => return Some(LRESULT(1)),
            WM_PAINT => unsafe {
                let _ = ValidateRect(Some(self.menu.hwnd), None);
            },
            _ => return None,
        }
        Some(LRESULT(0))
    }
}
