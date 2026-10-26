//! GPU rendering: Windows.UI.Composition hosts a host-backdrop (frosted glass) visual
//! plus a Direct2D-drawn surface with the dock itself.

use crate::config::Color;
use crate::icons::IconPixels;
use std::collections::HashMap;
use windows::Foundation::Size;
use windows::Graphics::DirectX::{DirectXAlphaMode, DirectXPixelFormat};
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM};
use windows::Win32::System::WinRT::Composition::*;
use windows::core::{HSTRING, Interface, Result, w};
use windows_numerics::{Matrix3x2, Vector2, Vector3};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RectF {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl RectF {
    pub fn w(&self) -> f32 {
        self.r - self.l
    }
    pub fn h(&self) -> f32 {
        self.b - self.t
    }
    pub fn translate(&self, dx: f32, dy: f32) -> RectF {
        RectF { l: self.l + dx, t: self.t + dy, r: self.r + dx, b: self.b + dy }
    }
    pub fn inflate(&self, d: f32) -> RectF {
        RectF { l: self.l - d, t: self.t - d, r: self.r + d, b: self.b + d }
    }
    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.l, top: self.t, right: self.r, bottom: self.b }
    }
}

pub struct Shape {
    pub rect: RectF,
    pub radius: f32,
    pub color: Color,
}

pub struct IconDraw<'a> {
    pub rect: RectF,
    pub source: &'a str,
    pub opacity: f32,
}

pub struct LabelDraw {
    pub rect: RectF,
    pub text: String,
    pub bg: Color,
    pub fg: Color,
    pub pad: (f32, f32),
}

pub struct Frame<'a> {
    /// Offset of the whole dock (used to slide it away when auto-hiding).
    pub offset: (f32, f32),
    pub bar: RectF,
    pub bar_radius: f32,
    pub bar_fill: Color,
    pub bar_border: Color,
    pub blur: bool,
    pub placeholder: Color,
    pub shapes: Vec<Shape>,
    pub icons: Vec<IconDraw<'a>>,
    pub label: Option<LabelDraw>,
}

pub struct Renderer {
    _target: DesktopWindowTarget,
    root: ContainerVisual,
    backdrop: SpriteVisual,
    backdrop_shape: CompositionRoundedRectangleGeometry,
    content: SpriteVisual,
    _graphics: CompositionGraphicsDevice,
    surface: ICompositionDrawingSurfaceInterop,
    resources: ID2D1DeviceContext,
    brush: ID2D1SolidColorBrush,
    dwrite: IDWriteFactory,
    label_format: Option<IDWriteTextFormat>,
    label_layout: Option<(String, IDWriteTextLayout, f32, f32)>,
    bitmaps: HashMap<String, ID2D1Bitmap1>,
    size: (u32, u32),
    /// Area drawn last frame; it must be cleared when the dock shrinks or moves.
    drawn: Option<RECT>,
}

fn color(c: Color) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: c.r, g: c.g, b: c.b, a: c.a }
}

/// WARP (software) keeps ~40 MB of GPU driver state out of the process; the dock only
/// redraws a small area for a moment at a time, so it's the better default.
fn d3d_device(gpu: bool) -> Result<ID3D11Device> {
    let mut last = Err(windows::core::Error::from(E_FAIL));
    let drivers: &[D3D_DRIVER_TYPE] = if gpu { &[D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] } else { &[D3D_DRIVER_TYPE_WARP] };
    for &driver in drivers {
        let mut device = None;
        let r = unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        };
        match (r, device) {
            (Ok(()), Some(d)) => return Ok(d),
            (Err(e), _) => last = Err(e),
            _ => {}
        }
    }
    last
}

impl Renderer {
    /// Needs a DispatcherQueue on the calling thread.
    pub fn new(hwnd: HWND, width: u32, height: u32, gpu: bool) -> Result<Self> {
        unsafe {
            let compositor = Compositor::new()?;
            let target = compositor
                .cast::<ICompositorDesktopInterop>()?
                .CreateDesktopWindowTarget(hwnd, true)?;
            let root = compositor.CreateContainerVisual()?;
            target.SetRoot(&root)?;

            // Frosted glass: DWM's blurred view of whatever is behind the window,
            // clipped to the dock's rounded rectangle.
            let backdrop = compositor.CreateSpriteVisual()?;
            backdrop.SetBrush(&compositor.CreateHostBackdropBrush()?)?;
            let backdrop_shape = compositor.CreateRoundedRectangleGeometry()?;
            backdrop.SetClip(&compositor.CreateGeometricClipWithGeometry(&backdrop_shape)?)?;
            root.Children()?.InsertAtTop(&backdrop)?;

            let d3d = d3d_device(gpu)?;
            let dxgi: IDXGIDevice = d3d.cast()?;
            let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d = factory.CreateDevice(&dxgi)?;
            let resources = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let brush = resources.CreateSolidColorBrush(&color(Color::rgba(0, 0, 0, 0)), None)?;

            let graphics = compositor.cast::<ICompositorInterop>()?.CreateGraphicsDevice(&d2d)?;
            let surface = graphics.CreateDrawingSurface(
                Size { Width: width.max(1) as f32, Height: height.max(1) as f32 },
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                DirectXAlphaMode::Premultiplied,
            )?;
            let content = compositor.CreateSpriteVisual()?;
            content.SetBrush(&compositor.CreateSurfaceBrushWithSurface(&surface)?)?;
            content.SetSize(Vector2 { X: width as f32, Y: height as f32 })?;
            root.Children()?.InsertAtTop(&content)?;

            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            Ok(Self {
                _target: target,
                root,
                backdrop,
                backdrop_shape,
                content,
                _graphics: graphics,
                surface: surface.cast()?,
                resources,
                brush,
                dwrite,
                label_format: None,
                label_layout: None,
                bitmaps: HashMap::new(),
                size: (width.max(1), height.max(1)),
                drawn: Some(RECT { left: 0, top: 0, right: width.max(1) as i32, bottom: height.max(1) as i32 }),
            })
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let size = (width.max(1), height.max(1));
        if size == self.size {
            return Ok(());
        }
        self.size = size;
        // Resizing discards the surface contents, so the next frame redraws everything.
        self.drawn = Some(RECT { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 });
        unsafe {
            self.surface.Resize(SIZE { cx: size.0 as i32, cy: size.1 as i32 })?;
        }
        self.content.SetSize(Vector2 { X: size.0 as f32, Y: size.1 as f32 })
    }

    pub fn set_label_font(&mut self, family: &str, size_px: f32) {
        self.label_layout = None;
        self.label_format = unsafe {
            self.dwrite
                .CreateTextFormat(
                    &HSTRING::from(family),
                    None,
                    DWRITE_FONT_WEIGHT_MEDIUM,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size_px,
                    w!(""),
                )
                .inspect(|f| {
                    let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                })
                .ok()
        };
    }

    /// Size of `text` in the label font, in pixels.
    pub fn measure_label(&mut self, text: &str) -> Option<(f32, f32)> {
        if let Some((t, _, w, h)) = &self.label_layout
            && t == text {
                return Some((*w, *h));
            }
        let format = self.label_format.as_ref()?;
        let utf16: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let layout = self.dwrite.CreateTextLayout(&utf16, format, 4000.0, 1000.0).ok()?;
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m).ok()?;
            let (w, h) = (m.widthIncludingTrailingWhitespace.ceil(), m.height.ceil());
            self.label_layout = Some((text.to_string(), layout, w, h));
            Some((w, h))
        }
    }

    pub fn clear_bitmaps(&mut self) {
        self.bitmaps.clear();
    }

    fn bitmap(&mut self, source: &str, pixels: &HashMap<String, Option<IconPixels>>) -> Option<ID2D1Bitmap1> {
        if let Some(b) = self.bitmaps.get(source) {
            return Some(b.clone());
        }
        let px = pixels.get(source)?.as_ref()?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let bmp = unsafe {
            self.resources
                .CreateBitmap(
                    D2D_SIZE_U { width: px.width, height: px.height },
                    Some(px.data.as_ptr() as _),
                    px.width * 4,
                    &props,
                )
                .ok()?
        };
        self.bitmaps.insert(source.to_string(), bmp.clone());
        Some(bmp)
    }

    pub fn draw(&mut self, f: &Frame, pixels: &HashMap<String, Option<IconPixels>>) -> Result<()> {
        self.root.SetOffset(Vector3 { X: f.offset.0, Y: f.offset.1, Z: 0.0 })?;
        let show_backdrop = f.blur && f.bar.w() > 0.0;
        self.backdrop.SetIsVisible(show_backdrop)?;
        if show_backdrop {
            let size = Vector2 { X: f.bar.w(), Y: f.bar.h() };
            self.backdrop.SetOffset(Vector3 { X: f.bar.l, Y: f.bar.t, Z: 0.0 })?;
            self.backdrop.SetSize(size)?;
            self.backdrop_shape.SetSize(size)?;
            self.backdrop_shape.SetCornerRadius(Vector2 { X: f.bar_radius, Y: f.bar_radius })?;
        }

        let bitmaps: Vec<Option<ID2D1Bitmap1>> = f.icons.iter().map(|i| self.bitmap(i.source, pixels)).collect();

        // Only redraw what changed: this frame's content plus whatever was drawn last frame.
        let bounds = f
            .icons
            .iter()
            .map(|i| i.rect)
            .chain(f.shapes.iter().map(|s| s.rect))
            .chain(f.label.iter().map(|l| l.rect))
            .chain(std::iter::once(f.bar))
            .reduce(|a, b| RectF { l: a.l.min(b.l), t: a.t.min(b.t), r: a.r.max(b.r), b: a.b.max(b.b) })
            .map(|r| r.inflate(2.0));
        let to_rect = |r: RectF| RECT {
            left: (r.l.floor() as i32).clamp(0, self.size.0 as i32),
            top: (r.t.floor() as i32).clamp(0, self.size.1 as i32),
            right: (r.r.ceil() as i32).clamp(0, self.size.0 as i32),
            bottom: (r.b.ceil() as i32).clamp(0, self.size.1 as i32),
        };
        let current = bounds.map(to_rect);
        let update = match (current, self.drawn) {
            (Some(a), Some(b)) => RECT { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) },
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => return Ok(()),
        };
        if update.right <= update.left || update.bottom <= update.top {
            self.drawn = current;
            return Ok(());
        }

        let mut offset = POINT::default();
        let dc: ID2D1DeviceContext = unsafe { self.surface.BeginDraw(Some(&update), &mut offset)? };
        let painted = self.paint(&dc, f, &bitmaps, offset, update);
        unsafe { self.surface.EndDraw()? };
        self.drawn = current;
        painted
    }

    fn fill(&self, dc: &ID2D1DeviceContext, r: RectF, radius: f32, c: Color) {
        unsafe {
            self.brush.SetColor(&color(c));
            let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
            dc.FillRoundedRectangle(&rr, &self.brush);
        }
    }

    fn paint(&self, dc: &ID2D1DeviceContext, f: &Frame, bitmaps: &[Option<ID2D1Bitmap1>], at: POINT, update: RECT) -> Result<()> {
        unsafe {
            dc.SetDpi(96.0, 96.0);
            // `at` is where the update rectangle's top-left lives in the (possibly shared) surface.
            dc.SetTransform(&Matrix3x2::translation((at.x - update.left) as f32, (at.y - update.top) as f32));
            dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let clip = RectF { l: update.left as f32, t: update.top as f32, r: update.right as f32, b: update.bottom as f32 };
            dc.PushAxisAlignedClip(&clip.d2d(), D2D1_ANTIALIAS_MODE_ALIASED);
            dc.Clear(Some(&color(Color::rgba(0, 0, 0, 0))));

            if f.bar.w() > 0.0 {
                self.fill(dc, f.bar, f.bar_radius, f.bar_fill);
                self.brush.SetColor(&color(f.bar_border));
                let edge = f.bar.inflate(-0.5);
                let rr = D2D1_ROUNDED_RECT { rect: edge.d2d(), radiusX: f.bar_radius - 0.5, radiusY: f.bar_radius - 0.5 };
                dc.DrawRoundedRectangle(&rr, &self.brush, 1.0, None);
            }
            for s in &f.shapes {
                self.fill(dc, s.rect, s.radius, s.color);
            }
            for (icon, bmp) in f.icons.iter().zip(bitmaps) {
                match bmp {
                    Some(b) => dc.DrawBitmap(
                        b,
                        Some(&icon.rect.d2d()),
                        icon.opacity,
                        D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                        None,
                        None,
                    ),
                    None => self.fill(dc, icon.rect.inflate(-icon.rect.w() * 0.08), icon.rect.w() * 0.22, f.placeholder.alpha(icon.opacity * 0.7)),
                }
            }
            if let (Some(l), Some((text, layout, _, _))) = (&f.label, &self.label_layout)
                && *text == l.text {
                    self.fill(dc, l.rect, l.rect.h().min(l.rect.w()) / 2.0, l.bg);
                    self.brush.SetColor(&color(l.fg));
                    dc.DrawTextLayout(
                        Vector2 { X: l.rect.l + l.pad.0, Y: l.rect.t + l.pad.1 },
                        layout,
                        &self.brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
            dc.PopAxisAlignedClip();
        }
        Ok(())
    }
}
