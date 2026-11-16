//! Loads high-resolution icons from the shell on a background thread.

use std::collections::HashSet;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HWND, LPARAM, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY};
use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, HICON, ICONINFO, PostMessageW};
use windows::core::HSTRING;

/// Premultiplied BGRA pixels, top-down.
pub struct IconPixels {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

pub struct Loaded {
    pub source: String,
    pub pixels: Option<IconPixels>,
}

pub struct IconLoader {
    tx: Sender<(String, u32)>,
    done: Arc<Mutex<Vec<Loaded>>>,
    pending: HashSet<String>,
}

impl IconLoader {
    /// Results are announced by posting `notify_msg` to `hwnd`.
    pub fn new(hwnd: HWND, notify_msg: u32) -> Self {
        let (tx, rx) = channel::<(String, u32)>();
        let done = Arc::new(Mutex::new(Vec::new()));
        let out = done.clone();
        let hwnd = hwnd.0 as isize;
        std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            while let Ok((source, size)) = rx.recv() {
                let pixels = load(&source, size);
                if pixels.is_none() {
                    crate::log!("no icon for {source}");
                }
                out.lock().unwrap().push(Loaded { source, pixels });
                unsafe {
                    let _ = PostMessageW(Some(HWND(hwnd as _)), notify_msg, WPARAM(0), LPARAM(0));
                }
            }
        });
        Self { tx, done, pending: HashSet::new() }
    }

    pub fn request(&mut self, source: &str, size: u32) {
        if self.pending.insert(source.to_string()) {
            let _ = self.tx.send((source.to_string(), size));
        }
    }

    pub fn take_loaded(&mut self) -> Vec<Loaded> {
        let loaded = std::mem::take(&mut *self.done.lock().unwrap());
        for l in &loaded {
            self.pending.remove(&l.source);
        }
        loaded
    }

    /// Forget in-flight requests so they're asked for again (e.g. at a new size).
    pub fn reset(&mut self) {
        self.pending.clear();
    }
}

fn load(source: &str, size: u32) -> Option<IconPixels> {
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(source), None).ok()?;
        let hbm = factory.GetImage(SIZE { cx: size as i32, cy: size as i32 }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK).ok()?;
        let px = bitmap_pixels(hbm);
        let _ = DeleteObject(hbm.into());
        px.map(trim)
    }
}

unsafe fn bitmap_pixels(hbm: HBITMAP) -> Option<IconPixels> {
    let mut px = unsafe { raw_pixels(hbm) }?;
    if px.data.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        // No alpha channel at all: the image is opaque.
        px.data.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 255);
    } else {
        premultiply(&mut px);
    }
    Some(px)
}

/// A bitmap's pixels as 32-bit BGRA, alpha untouched.
unsafe fn raw_pixels(hbm: HBITMAP) -> Option<IconPixels> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(hbm.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as _)) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth.unsigned_abs(), bm.bmHeight.unsigned_abs());
        if w == 0 || h == 0 {
            return None;
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut data = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(None);
        let lines = GetDIBits(dc, hbm, 0, h, Some(data.as_mut_ptr() as _), &mut info, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        (lines != 0).then_some(IconPixels { width: w, height: h, data })
    }
}

/// Direct2D wants premultiplied alpha; leaves pixels that already are alone.
fn premultiply(px: &mut IconPixels) {
    if px.data.as_chunks::<4>().0.iter().any(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3]) {
        for p in px.data.as_chunks_mut::<4>().0.iter_mut() {
            let a = p[3] as u32;
            for c in &mut p[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
    }
}

/// An icon handle's picture at its own size. Icons without alpha take transparency from
/// their mask.
pub fn icon_pixels(icon: HICON) -> Option<IconPixels> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info).ok()?;
        let color = if info.hbmColor.is_invalid() { None } else { raw_pixels(info.hbmColor) };
        let mask = raw_pixels(info.hbmMask);
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        let mut px = color?;
        if px.data.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
            let mask = mask.filter(|m| m.width == px.width && m.height >= px.height)?;
            for (p, m) in px.data.as_chunks_mut::<4>().0.iter_mut().zip(mask.data.as_chunks::<4>().0.iter()) {
                if m[0] != 0 {
                    p.copy_from_slice(&[0, 0, 0, 0]);
                } else {
                    p[3] = 255;
                }
            }
        } else {
            premultiply(&mut px);
        }
        Some(px)
    }
}

/// Old apps without large icons come back as a small icon centred in a big transparent
/// square. Crop to the visible content so it fills the dock slot like everything else.
fn trim(px: IconPixels) -> IconPixels {
    let (w, h) = (px.width as usize, px.height as usize);
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            if px.data[(y * w + x) * 4 + 3] > 8 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return px;
    }
    let side = (x1 - x0).max(y1 - y0);
    if side as f32 >= w.min(h) as f32 * 0.75 {
        return px;
    }
    let side = (side + side / 8).min(w.min(h));
    let cx = (x0 + x1) / 2;
    let cy = (y0 + y1) / 2;
    let sx = cx.saturating_sub(side / 2).min(w - side);
    let sy = cy.saturating_sub(side / 2).min(h - side);
    let mut data = Vec::with_capacity(side * side * 4);
    for y in sy..sy + side {
        let row = (y * w + sx) * 4;
        data.extend_from_slice(&px.data[row..row + side * 4]);
    }
    IconPixels { width: side as u32, height: side as u32, data }
}
