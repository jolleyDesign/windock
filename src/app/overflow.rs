//! The tray icons Windows keeps off the taskbar, in a grid that opens from the arrow beside
//! the dock's own tray icons, like the taskbar's overflow flyout.

use super::*;
use crate::render::Surface;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;

struct Cell {
    id: String,
    name: String,
    image: String,
    rect: RectF,
}

pub(super) struct Flyout {
    hwnd: HWND,
    surface: Option<Surface>,
    open: bool,
    cells: Vec<Cell>,
    size: (f32, f32),
    /// The frosted grid; above it is room for the hovered icon's label.
    grid: RectF,
    hover: Option<usize>,
    tracking: bool,
    region: Vec<RECT>,
    /// The process whose menu a right-click opened. The flyout stays open while the focus is
    /// with it, like the taskbar's.
    lent: Option<u32>,
}

impl Flyout {
    pub(super) fn new(hwnd: HWND) -> Self {
        Self { hwnd, surface: None, open: false, cells: Vec::new(), size: (0.0, 0.0), grid: RectF::default(), hover: None, tracking: false, region: Vec::new(), lent: None }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// The renderer was recreated; surfaces from the old one are dead.
    pub(super) fn reset_surface(&mut self) {
        self.surface = None;
    }
}

pub(super) fn create_window(instance: HINSTANCE) -> Result<HWND> {
    unsafe {
        let class = w!("WindockTrayFlyout");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(flyout_proc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        // Unlike the dock it takes the focus while open, so clicking anywhere else closes it
        // and Esc works.
        let hwnd =
            CreateWindowExW(WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_TOPMOST, class, w!("Hidden icons"), WS_POPUP, 0, 0, 1, 1, None, None, Some(instance), None)?;
        let on = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(17), &on as *const _ as _, 4);
        Ok(hwnd)
    }
}

unsafe extern "system" fn flyout_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(Some(r)) = with_app(|a| a.flyout_message(msg, wp, lp)) {
        return r;
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

impl App {
    /// The icons that belong in the flyout rather than the dock.
    pub(super) fn overflow_icons(&self) -> Vec<&tray::Icon> {
        let all = self.cfg.tray.apps == TrayApps::All;
        self.tray_icons.iter().filter(|i| !i.hidden && !(all || i.promoted)).collect()
    }

    pub(super) fn toggle_flyout(&mut self) {
        if self.flyout.open {
            self.close_flyout();
        } else {
            self.open_flyout();
        }
    }

    pub(super) fn open_flyout(&mut self) {
        self.close_preview();
        let was_open = self.flyout.open;
        self.flyout.open = true;
        self.flyout.hover = None;
        self.layout_flyout();
        if self.flyout.cells.is_empty() {
            self.close_flyout();
            return;
        }
        self.position_flyout();
        self.draw_flyout();
        // Refreshing an open flyout mustn't take the focus back from an app's menu.
        if !was_open {
            unsafe {
                let _ = SetForegroundWindow(self.flyout.hwnd);
            }
        }
        self.dirty = true;
    }

    pub(super) fn close_flyout(&mut self) {
        if !self.flyout.open {
            return;
        }
        self.flyout.open = false;
        self.flyout.hover = None;
        self.flyout.tracking = false;
        self.flyout.lent = None;
        unsafe {
            let _ = ShowWindow(self.flyout.hwnd, SW_HIDE);
        }
        self.dirty = true;
        self.schedule_hide(self.cfg.dock.autohide_delay_ms);
    }

    /// Icons came or went while it's open.
    pub(super) fn refresh_flyout(&mut self) {
        if self.flyout.open {
            self.open_flyout();
        }
    }

    /// A roughly square grid, at most six across, like the taskbar's.
    fn layout_flyout(&mut self) {
        let s = self.m.s;
        let cells: Vec<(String, String, String)> = self
            .overflow_icons()
            .iter()
            .map(|i| {
                let tip = i.tip.lines().next().unwrap_or_default().trim().to_string();
                let name = if tip.is_empty() { apps::process_path(apps::pid_of(HWND(i.owner as _))).map(|p| shell::friendly_name(&p)).unwrap_or_default() } else { tip };
                (i.id(), name, tray_image(i))
            })
            .collect();
        let n = cells.len();
        let cols = ((n as f32).sqrt().ceil() as usize).clamp(1, 6);
        let rows = n.div_ceil(cols);
        let cell = (self.cfg.tray.icon_size * s + 22.0 * s).round();
        let pad = 6.0 * s;
        let (grid_w, grid_h) = (cols as f32 * cell + 2.0 * pad, rows as f32 * cell + 2.0 * pad);
        let text_h = self.gpu.as_mut().and_then(|g| g.measure("Ag", 1000.0)).map_or(16.0 * s, |(_, h)| h);
        let label_room = text_h + 2.0 * 4.0 * s + 8.0 * s;
        // Wide enough for most labels; what's beyond the grid is click-through.
        let w = grid_w.max(360.0 * s);
        self.flyout.size = (w, grid_h + label_room);
        self.flyout.grid = RectF::new(((w - grid_w) / 2.0).round(), label_room, grid_w, grid_h);
        let g = self.flyout.grid;
        self.flyout.cells = cells
            .into_iter()
            .enumerate()
            .map(|(k, (id, name, image))| {
                let (c, r) = ((k % cols) as f32, (k / cols) as f32);
                Cell { id, name, image, rect: RectF::new(g.l + pad + c * cell, g.t + pad + r * cell, cell, cell) }
            })
            .collect();
    }

    /// Puts the grid just beyond the arrow, kept on screen.
    fn position_flyout(&mut self) {
        let (w, h) = self.flyout.size;
        let g = self.flyout.grid;
        let anchor = self.items.iter().position(|it| it.kind == Kind::TrayOverflow);
        let Some((u0, u1, _, v1)) = anchor.and_then(|i| self.icon_uv(i)) else { return };
        let (gx, gy) = self.place_beside((u0 + u1) / 2.0, v1 + 8.0 * self.m.s, g.w(), g.h());
        let (x, y) = ((gx - g.l).round() as i32, (gy - g.t).round() as i32);
        let (wi, hi) = (w.ceil() as i32, h.ceil() as i32);
        self.flyout.region.clear();
        unsafe {
            let _ = SetWindowPos(self.flyout.hwnd, Some(HWND_TOPMOST), x, y, wi, hi, SWP_SHOWWINDOW);
        }
        if let Some(sf) = &mut self.flyout.surface {
            let _ = sf.resize(wi as u32, hi as u32);
        } else if let Some(gpu) = &self.gpu {
            self.flyout.surface = gpu.create_surface(self.flyout.hwnd, wi as u32, hi as u32).inspect_err(|e| crate::log!("flyout surface: {e}")).ok();
        }
    }

    fn draw_flyout(&mut self) {
        // The frame borrows the cells' image names, so the surface is lent out while drawing.
        let mut surface = self.flyout.surface.take();
        let s = self.m.s;
        let pal = self.palette;
        let f = &self.flyout;
        let mut shapes = Vec::new();
        let mut icons = Vec::new();
        let size = (self.cfg.tray.icon_size * s).round();
        for (k, c) in f.cells.iter().enumerate() {
            if f.hover == Some(k) {
                shapes.push(Shape { rect: c.rect.inflate(-2.0 * s), radius: 6.0 * s, color: pal.label_text.alpha(0.09) });
            }
            let (cx, cy) = ((c.rect.l + c.rect.r) / 2.0, (c.rect.t + c.rect.b) / 2.0);
            icons.push(IconDraw { rect: RectF::new((cx - size / 2.0).round(), (cy - size / 2.0).round(), size, size), source: &c.image, opacity: 1.0 });
        }
        let mut region = vec![f.grid];
        let label = f.hover.and_then(|k| f.cells.get(k)).filter(|c| !c.name.is_empty()).map(|c| (c.name.clone(), (c.rect.l + c.rect.r) / 2.0));
        let label = label.and_then(|(text, cx)| {
            let max = f.size.0 - 8.0 * s;
            let (tw, th) = self.gpu.as_mut()?.measure(&text, max)?;
            let pad = (10.0 * s, 4.0 * s);
            let (pw, ph) = ((tw + 2.0 * pad.0).min(f.size.0), th + 2.0 * pad.1);
            let l = (cx - pw / 2.0).clamp(0.0, (f.size.0 - pw).max(0.0));
            let rect = RectF::new(l, f.grid.t - 8.0 * s - ph, pw, ph);
            region.push(rect);
            Some(LabelDraw { rect, radius: ph / 2.0, text, max_width: pw - 2.0 * pad.0, bg: pal.label_bg, fg: pal.label_text, pad })
        });
        let frame = Frame {
            texts: Vec::new(),
            offset: (0.0, 0.0),
            bar: f.grid,
            bar_radius: (self.m.radius * 0.6).max(8.0 * s),
            bar_fill: Color { a: pal.background.a.max(0.8), ..pal.background },
            bar_border: pal.border,
            island: None,
            blur: self.blur,
            placeholder: pal.placeholder,
            shapes,
            icons,
            labels: label.into_iter().collect(),
        };
        if let (Some(g), Some(sf)) = (&mut self.gpu, &mut surface)
            && let Err(e) = g.draw(sf, &frame, &self.icons)
        {
            crate::log!("flyout draw failed: {e}");
        }
        drop(frame);
        self.flyout.surface = surface;
        self.flyout_region(region);
    }

    /// Only the grid and the label take the mouse; the rest of the window is click-through.
    fn flyout_region(&mut self, rects: Vec<RectF>) {
        let list: Vec<RECT> = rects.iter().map(|r| RECT { left: r.l.floor() as i32, top: r.t.floor() as i32, right: r.r.ceil() as i32, bottom: r.b.ceil() as i32 }).collect();
        if list == self.flyout.region {
            return;
        }
        unsafe {
            let rgn = CreateRectRgn(0, 0, 0, 0);
            for r in &list {
                let part = CreateRectRgn(r.left, r.top, r.right, r.bottom);
                CombineRgn(Some(rgn), Some(rgn), Some(part), RGN_OR);
                let _ = DeleteObject(part.into());
            }
            SetWindowRgn(self.flyout.hwnd, Some(rgn), true);
        }
        self.flyout.region = list;
    }

    fn flyout_click(&mut self, right: bool) {
        let Some(id) = self.flyout.hover.and_then(|k| self.flyout.cells.get(k)).map(|c| c.id.clone()) else { return };
        // Click first, while the flyout still has the focus to hand on to the app.
        let Some(icon) = self.tray_icon(&id) else { return };
        tray::click(icon, if right { tray::Click::Right } else { tray::Click::Left }, cursor_pos());
        let pid = apps::pid_of(HWND(icon.owner as _));
        // A right click opens the app's menu over the flyout, which stays open; a left click
        // usually opens the app, so it closes.
        if right {
            self.flyout.lent = Some(pid);
        } else {
            self.close_flyout();
        }
    }

    /// Another app took the focus. While an app's menu is open from the flyout, the menu (or
    /// the hidden window the app brings forward for it) keeps the flyout open; one of the app's
    /// windows opening, or any other app, closes it.
    pub(super) fn flyout_foreground(&mut self, hwnd: HWND) {
        let Some(pid) = self.flyout.lent else { return };
        if apps::pid_of(hwnd) != pid || apps::is_app_window(hwnd, self.own_pid, None) {
            self.close_flyout();
        }
    }

    pub(super) fn flyout_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let x = (lp.0 & 0xffff) as i16 as f32;
        let y = ((lp.0 >> 16) & 0xffff) as i16 as f32;
        match msg {
            WM_MOUSEMOVE => {
                if !self.flyout.tracking {
                    let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.flyout.hwnd, dwHoverTime: 0 };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                    self.flyout.tracking = true;
                }
                let hover = self.flyout.cells.iter().position(|c| c.rect.contains(x, y));
                if hover != self.flyout.hover {
                    self.flyout.hover = hover;
                    if let Some(icon) = hover.and_then(|k| self.tray_icon(&self.flyout.cells[k].id)) {
                        tray::click(icon, tray::Click::Hover, cursor_pos());
                    }
                    self.draw_flyout();
                }
            }
            WM_MOUSELEAVE => {
                self.flyout.tracking = false;
                self.flyout.hover = None;
                self.draw_flyout();
            }
            WM_LBUTTONUP => self.flyout_click(false),
            WM_RBUTTONUP => self.flyout_click(true),
            WM_KEYDOWN if wp.0 as u16 == VK_ESCAPE.0 => self.close_flyout(),
            // Clicking anywhere else, or switching apps, closes it, unless the focus went to the
            // menu of an icon in it; flyout_foreground watches where it goes from there.
            WM_ACTIVATE if wp.0 & 0xffff == WA_INACTIVE as usize => {
                if self.flyout.lent.is_none() {
                    self.close_flyout();
                }
            }
            // Clicked again after an app's menu: it has the focus back.
            WM_ACTIVATE => self.flyout.lent = None,
            WM_ERASEBKGND => return Some(LRESULT(1)),
            WM_PAINT => unsafe {
                let _ = ValidateRect(Some(self.flyout.hwnd), None);
            },
            _ => return None,
        }
        Some(LRESULT(0))
    }
}
