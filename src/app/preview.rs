//! Live window previews: hovering a running app shows a popup with a thumbnail of each of
//! its windows. The thumbnails are DWM's own live copies of the windows (what the taskbar uses).

use super::*;
use crate::render::Surface;
use windows::Win32::Graphics::Dwm::{
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
    DwmQueryThumbnailSourceSize, DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
};

pub(super) const TIMER_PREVIEW: usize = 5;
pub(super) const TIMER_PREVIEW_CLOSE: usize = 6;
const OPEN_DELAY_MS: u32 = 400;
/// Grace period for moving the mouse between the dock and the popup.
const CLOSE_DELAY_MS: u32 = 300;

struct Card {
    hwnd: isize,
    title: String,
    thumb: isize,
    rect: RectF,
    icon: RectF,
    title_rect: RectF,
    close: RectF,
    shot: RectF,
}

pub(super) struct Preview {
    hwnd: HWND,
    surface: Option<Surface>,
    /// The item whose windows are showing.
    item: Option<String>,
    /// The item we'll open for once the hover delay passes.
    pending: Option<String>,
    icon: String,
    cards: Vec<Card>,
    size: (f32, f32),
    /// Hovered card, and whether the mouse is on its close button.
    hover: Option<(usize, bool)>,
    tracking: bool,
}

impl Preview {
    pub(super) fn new(hwnd: HWND) -> Self {
        Self {
            hwnd,
            surface: None,
            item: None,
            pending: None,
            icon: String::new(),
            cards: Vec::new(),
            size: (0.0, 0.0),
            hover: None,
            tracking: false,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.item.is_some()
    }

    pub(super) fn showing(&self, id: &str) -> bool {
        self.item.as_deref() == Some(id)
    }

    /// The renderer was recreated; surfaces from the old one are dead.
    pub(super) fn reset_surface(&mut self) {
        self.surface = None;
    }

    fn unregister(&mut self) {
        for c in self.cards.drain(..) {
            unsafe {
                let _ = DwmUnregisterThumbnail(c.thumb);
            }
        }
    }
}

pub(super) fn create_window(instance: HINSTANCE) -> Result<HWND> {
    unsafe {
        let class = w!("WindockPreview");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(preview_proc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class,
            w!("Windock Preview"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance),
            None,
        )?;
        let on = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(17), &on as *const _ as _, 4);
        Ok(hwnd)
    }
}

unsafe extern "system" fn preview_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(Some(r)) = with_app(|a| a.preview_message(msg, lp)) {
        return r;
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

impl App {
    /// Called every frame: opens, switches or closes the preview to follow the hovered app.
    pub(super) fn follow_hover_for_preview(&mut self) {
        let enabled = self.cfg.dock.previews && self.drag.is_none() && self.dragging.is_none() && self.press.is_none() && !self.menu.is_open();
        let hovered = self.hovered_index().filter(|_| enabled);
        let target = hovered.filter(|&i| !self.items[i].windows.is_empty()).map(|i| self.items[i].id.clone());
        match (self.preview.item.clone(), target) {
            (Some(open), Some(t)) => {
                self.kill_timer(TIMER_PREVIEW_CLOSE);
                if open != t {
                    // Already browsing previews: switch straight away.
                    self.open_preview(&t);
                }
            }
            // On an app with no windows (or previews just got disabled): close.
            (Some(_), None) => {
                if hovered.is_some() || !enabled {
                    self.close_preview();
                }
            }
            (None, Some(t)) => {
                if self.preview.pending.as_ref() != Some(&t) {
                    self.preview.pending = Some(t);
                    self.set_timer(TIMER_PREVIEW, OPEN_DELAY_MS);
                }
            }
            (None, None) => {
                if self.preview.pending.take().is_some() {
                    self.kill_timer(TIMER_PREVIEW);
                }
            }
        }
    }

    /// The hover delay passed; open if the mouse is still on the same app.
    pub(super) fn preview_timer(&mut self) {
        let Some(id) = self.preview.pending.take() else { return };
        let still = self.hovered_index().is_some_and(|i| self.items[i].id == id);
        if still {
            self.open_preview(&id);
        }
    }

    /// Mouse left the dock: give it a moment to reach the popup.
    pub(super) fn preview_dock_left(&mut self) {
        if self.preview.pending.take().is_some() {
            self.kill_timer(TIMER_PREVIEW);
        }
        if self.preview.is_open() {
            self.set_timer(TIMER_PREVIEW_CLOSE, CLOSE_DELAY_MS);
        }
    }

    pub(super) fn close_preview(&mut self) {
        self.kill_timer(TIMER_PREVIEW);
        self.kill_timer(TIMER_PREVIEW_CLOSE);
        self.preview.pending = None;
        if self.preview.item.take().is_none() {
            return;
        }
        self.preview.unregister();
        self.preview.hover = None;
        self.preview.tracking = false;
        unsafe {
            let _ = ShowWindow(self.preview.hwnd, SW_HIDE);
        }
        self.dirty = true;
        self.schedule_hide(self.cfg.dock.autohide_delay_ms);
    }

    /// Windows came or went: update the open preview, or close it if nothing's left.
    pub(super) fn refresh_preview(&mut self) {
        if let Some(id) = self.preview.item.clone() {
            self.open_preview(&id);
        }
    }

    pub(super) fn open_preview(&mut self, id: &str) {
        let Some(i) = self.items.iter().position(|it| it.id == id && it.interactive()) else {
            self.close_preview();
            return;
        };
        if self.items[i].windows.is_empty() {
            self.close_preview();
            return;
        }
        // Note: no cancelling TIMER_PREVIEW_CLOSE here. This also runs when windows change,
        // and that mustn't keep a preview open after the mouse has left.
        self.preview.unregister();
        self.preview.item = Some(id.to_string());
        self.preview.icon = self.items[i].icon.clone();
        self.preview.hover = None;
        self.layout_cards(i);
        self.position_preview(i);
        self.draw_preview();
        self.dirty = true;
    }

    /// Sizes each card from its window's aspect ratio; cards run along the dock.
    fn layout_cards(&mut self, item: usize) {
        let s = self.m.s;
        let horizontal = self.axis.horizontal();
        let (pad, gap, inner) = (8.0 * s, 8.0 * s, 8.0 * s);
        let row = self.gpu.as_mut().and_then(|g| g.measure("Ag", 1000.0)).map_or(18.0 * s, |(_, h)| h).max(20.0 * s);
        let available = if horizontal { self.work.right - self.work.left } else { self.work.bottom - self.work.top } as f32 - 32.0 * s;

        let windows = self.items[item].windows.clone();
        let mut sources = Vec::new();
        for &w in &windows {
            let Ok(thumb) = (unsafe { DwmRegisterThumbnail(self.preview.hwnd, HWND(w as _)) }) else { continue };
            let size = unsafe { DwmQueryThumbnailSourceSize(thumb) }
                .ok()
                .filter(|s| s.cx > 0 && s.cy > 0)
                .map_or((16.0, 9.0), |s| (s.cx as f32, s.cy as f32));
            sources.push((w, thumb, size));
        }

        // Shrink the thumbnails until the popup fits on screen.
        let mut scale = 1.0f32;
        let (mut shots, mut total) = (Vec::new(), 0.0);
        for _ in 0..8 {
            let (max_w, max_h) = (220.0 * s * scale, 140.0 * s * scale);
            shots = sources
                .iter()
                .map(|&(_, _, (sw, sh))| {
                    let k = (max_w / sw).min(max_h / sh).min(1.0);
                    (sw * k, sh * k)
                })
                .collect();
            let card_main: Vec<f32> = shots
                .iter()
                .map(|&(w, h)| if horizontal { w.max(150.0 * s * scale) + 2.0 * inner } else { row + 6.0 * s + h + 2.0 * inner })
                .collect();
            total = card_main.iter().sum::<f32>() + gap * card_main.len().saturating_sub(1) as f32 + 2.0 * pad;
            if total <= available || scale < 0.35 {
                break;
            }
            scale *= 0.8;
        }

        let shot_h = shots.iter().map(|s| s.1).fold(0.0f32, f32::max);
        let card_w = shots.iter().map(|shot| shot.0.max(150.0 * s * scale)).fold(0.0f32, f32::max) + 2.0 * inner;
        let mut cards = Vec::new();
        let mut at = pad;
        for (&(hwnd, thumb, _), &(tw, th)) in sources.iter().zip(&shots) {
            let (w, h) = if horizontal {
                (tw.max(150.0 * s * scale) + 2.0 * inner, row + 6.0 * s + shot_h + 2.0 * inner)
            } else {
                (card_w, row + 6.0 * s + th + 2.0 * inner)
            };
            let rect = if horizontal { RectF::new(at, pad, w, h) } else { RectF::new(pad, at, w, h) };
            at += if horizontal { w } else { h } + gap;
            let icon_size = (16.0 * s).round();
            let icon = RectF::new(rect.l + inner, rect.t + inner + (row - icon_size) / 2.0, icon_size, icon_size);
            let close = RectF::new(rect.r - inner - row, rect.t + inner, row, row);
            let title_rect = RectF { l: icon.r + 6.0 * s, t: rect.t + inner, r: close.l - 4.0 * s, b: rect.t + inner + row };
            let area_top = rect.t + inner + row + 6.0 * s;
            let area_h = if horizontal { shot_h } else { th };
            let shot = RectF::new(rect.l + (w - tw) / 2.0, area_top + (area_h - th) / 2.0, tw, th);
            cards.push(Card { hwnd, title: apps::title(HWND(hwnd as _)), thumb, rect, icon, title_rect, close, shot });
        }
        let cross = cards.iter().map(|c| if horizontal { c.rect.h() } else { c.rect.w() }).fold(0.0f32, f32::max);
        self.preview.size = if horizontal { (total, cross + 2.0 * pad) } else { (cross + 2.0 * pad, total) };
        self.preview.cards = cards;
    }

    /// Places the popup just beyond the hovered icon, kept on screen.
    fn position_preview(&mut self, item: usize) {
        let (w, h) = self.preview.size;
        let Some((u0, u1, _, v1)) = self.icon_uv(item) else { return };
        let (dx, dy) = self.axis.toward_edge(self.slide_distance());
        let (ax, ay) = self.axis.xy((u0 + u1) / 2.0, v1 + 8.0 * self.m.s);
        let (ax, ay) = (self.origin.0 as f32 + ax + dx, self.origin.1 as f32 + ay + dy);
        let (x, y) = match self.axis.pos {
            Position::Bottom => (ax - w / 2.0, ay - h),
            Position::Top => (ax - w / 2.0, ay),
            Position::Left => (ax, ay - h / 2.0),
            Position::Right => (ax - w, ay - h / 2.0),
        };
        let margin = 8.0 * self.m.s;
        let wk = self.work;
        let x = x.clamp(wk.left as f32 + margin, (wk.right as f32 - w - margin).max(wk.left as f32 + margin));
        let y = y.clamp(wk.top as f32 + margin, (wk.bottom as f32 - h - margin).max(wk.top as f32 + margin));
        let (wi, hi) = (w.ceil() as i32, h.ceil() as i32);
        unsafe {
            let _ = SetWindowPos(self.preview.hwnd, Some(HWND_TOPMOST), x.round() as i32, y.round() as i32, wi, hi, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
        if let Some(sf) = &mut self.preview.surface {
            let _ = sf.resize(wi as u32, hi as u32);
        } else if let Some(g) = &self.gpu {
            self.preview.surface = g.create_surface(self.preview.hwnd, wi as u32, hi as u32).inspect_err(|e| crate::log!("preview surface: {e}")).ok();
        }
        for c in &self.preview.cards {
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: RECT { left: c.shot.l.round() as i32, top: c.shot.t.round() as i32, right: c.shot.r.round() as i32, bottom: c.shot.b.round() as i32 },
                opacity: 255,
                fVisible: true.into(),
                fSourceClientAreaOnly: false.into(),
                ..Default::default()
            };
            unsafe {
                let _ = DwmUpdateThumbnailProperties(c.thumb, &props);
            }
        }
    }

    fn draw_preview(&mut self) {
        let s = self.m.s;
        let pal = self.palette;
        let icon = self.preview.icon.clone();
        let p = &self.preview;
        let mut shapes = Vec::new();
        let mut icons = Vec::new();
        let mut labels = Vec::new();
        for (i, c) in p.cards.iter().enumerate() {
            let hovered = p.hover.map(|(h, _)| h) == Some(i);
            if hovered {
                shapes.push(Shape { rect: c.rect, radius: 8.0 * s, color: pal.label_text.alpha(0.09) });
            }
            icons.push(IconDraw { rect: c.icon, source: &icon, opacity: 1.0 });
            labels.push(LabelDraw {
                rect: c.title_rect,
                radius: 0.0,
                text: c.title.clone(),
                max_width: c.title_rect.w().max(1.0),
                bg: Color::rgba(0, 0, 0, 0),
                fg: pal.label_text,
                pad: (0.0, 0.0),
            });
            if hovered {
                let on_close = p.hover == Some((i, true));
                if on_close {
                    shapes.push(Shape { rect: c.close, radius: 4.0 * s, color: Color::rgba(0xc4, 0x2b, 0x1c, 0xff) });
                }
                let fg = if on_close { Color::rgba(255, 255, 255, 255) } else { pal.label_text.alpha(0.8) };
                let (tw, th) = self.gpu.as_mut().and_then(|g| g.measure("✕", 1000.0)).unwrap_or((c.close.w(), c.close.h()));
                let pad = ((c.close.w() - tw) / 2.0, (c.close.h() - th) / 2.0);
                labels.push(LabelDraw { rect: c.close, radius: 0.0, text: "✕".into(), max_width: 1000.0, bg: Color::rgba(0, 0, 0, 0), fg, pad });
            }
        }
        let p = &self.preview;
        let frame = Frame {
            texts: Vec::new(),
            offset: (0.0, 0.0),
            bar: RectF::new(0.0, 0.0, p.size.0, p.size.1),
            bar_radius: 12.0 * s,
            // A touch more opaque than the dock so titles stay readable over busy windows.
            bar_fill: Color { a: pal.background.a.max(0.8), ..pal.background },
            bar_border: pal.border,
            island: None,
            blur: self.blur,
            placeholder: pal.placeholder,
            shapes,
            icons,
            labels,
        };
        if let (Some(g), Some(sf)) = (&mut self.gpu, &mut self.preview.surface)
            && let Err(e) = g.draw(sf, &frame, &self.icons) {
                crate::log!("preview draw failed: {e}");
            }
    }

    pub(super) fn preview_message(&mut self, msg: u32, lp: LPARAM) -> Option<LRESULT> {
        let x = (lp.0 & 0xffff) as i16 as f32;
        let y = ((lp.0 >> 16) & 0xffff) as i16 as f32;
        match msg {
            WM_MOUSEMOVE => {
                if !self.preview.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.preview.hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                    self.preview.tracking = true;
                }
                self.kill_timer(TIMER_PREVIEW_CLOSE);
                let hover = self.preview.cards.iter().position(|c| c.rect.contains(x, y)).map(|i| (i, self.preview.cards[i].close.contains(x, y)));
                if hover != self.preview.hover {
                    self.preview.hover = hover;
                    self.draw_preview();
                }
            }
            WM_MOUSELEAVE => {
                self.preview.tracking = false;
                self.preview.hover = None;
                self.draw_preview();
                self.set_timer(TIMER_PREVIEW_CLOSE, CLOSE_DELAY_MS);
            }
            WM_LBUTTONUP => {
                if let Some((i, on_close)) = self.preview.hover {
                    let w = HWND(self.preview.cards[i].hwnd as _);
                    if on_close {
                        apps::close(w);
                    } else {
                        apps::activate(w);
                        self.close_preview();
                    }
                }
            }
            WM_MBUTTONUP => {
                if let Some((i, _)) = self.preview.hover {
                    apps::close(HWND(self.preview.cards[i].hwnd as _));
                }
            }
            WM_MOUSEACTIVATE => return Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_ERASEBKGND => return Some(LRESULT(1)),
            WM_PAINT => unsafe {
                let _ = ValidateRect(Some(self.preview.hwnd), None);
            },
            _ => return None,
        }
        Some(LRESULT(0))
    }
}
