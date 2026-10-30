//! GPU rendering: Windows.UI.Composition hosts a host-backdrop (frosted glass) visual
//! plus a Direct2D-drawn surface for each window. One `Gpu` (device, text, icon cache)
//! is shared by the dock and the preview popup; each has its own `Surface`.

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
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::System::WinRT::Composition::*;
use windows::core::{BOOL, HSTRING, Interface, Result, w};
use windows_numerics::{Matrix3x2, Vector2, Vector3};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RectF {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl RectF {
    pub fn new(l: f32, t: f32, w: f32, h: f32) -> RectF {
        RectF { l, t, r: l + w, b: t + h }
    }
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
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
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
    /// Background rectangle; the text starts `pad` inside it.
    pub rect: RectF,
    pub radius: f32,
    pub text: String,
    /// Wider text is trimmed with an ellipsis.
    pub max_width: f32,
    pub bg: Color,
    pub fg: Color,
    pub pad: (f32, f32),
}

/// Text with no background, centred in `rect`.
pub struct TextDraw {
    pub rect: RectF,
    pub text: String,
    pub color: Color,
    pub font: Font,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Font {
    Label,
    Clock,
    /// Windows' icon font, for the status icons.
    Icons,
}

pub struct Frame<'a> {
    /// Offset of the whole window's content (used to slide the dock away when auto-hiding).
    pub offset: (f32, f32),
    /// The frosted, bordered background.
    pub bar: RectF,
    pub bar_radius: f32,
    pub bar_fill: Color,
    pub bar_border: Color,
    pub blur: bool,
    pub placeholder: Color,
    pub shapes: Vec<Shape>,
    pub icons: Vec<IconDraw<'a>>,
    pub labels: Vec<LabelDraw>,
    pub texts: Vec<TextDraw>,
}

/// The composition target and drawing surface for one window.
pub struct Surface {
    _target: DesktopWindowTarget,
    root: ContainerVisual,
    backdrop: SpriteVisual,
    backdrop_shape: CompositionRoundedRectangleGeometry,
    content: SpriteVisual,
    surface: ICompositionDrawingSurfaceInterop,
    size: (u32, u32),
    /// Area drawn last frame; it must be cleared when content shrinks or moves.
    drawn: Option<RECT>,
}

impl Surface {
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
}

/// Device-level state shared by every surface.
/// Font, text, and the layout box's width and height.
type LayoutKey = (Font, String, u32, u32);

/// Per-frame resources, made before drawing starts.
struct Prepared {
    bitmaps: Vec<Option<ID2D1Bitmap1>>,
    labels: Vec<Option<IDWriteTextLayout>>,
    texts: Vec<Option<IDWriteTextLayout>>,
}

pub struct Gpu {
    compositor: Compositor,
    graphics: CompositionGraphicsDevice,
    resources: ID2D1DeviceContext,
    brush: ID2D1SolidColorBrush,
    dwrite: IDWriteFactory,
    label_format: Option<IDWriteTextFormat>,
    clock_format: Option<IDWriteTextFormat>,
    icon_format: Option<IDWriteTextFormat>,
    layouts: HashMap<LayoutKey, (IDWriteTextLayout, f32, f32)>,
    bitmaps: HashMap<String, ID2D1Bitmap1>,
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

impl Gpu {
    /// Needs a DispatcherQueue on the calling thread.
    pub fn new(gpu: bool) -> Result<Self> {
        unsafe {
            let compositor = Compositor::new()?;
            let d3d = d3d_device(gpu)?;
            let dxgi: IDXGIDevice = d3d.cast()?;
            let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d = factory.CreateDevice(&dxgi)?;
            let resources = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let brush = resources.CreateSolidColorBrush(&color(Color::rgba(0, 0, 0, 0)), None)?;
            let graphics = compositor.cast::<ICompositorInterop>()?.CreateGraphicsDevice(&d2d)?;
            Ok(Self {
                compositor,
                graphics,
                resources,
                brush,
                dwrite: DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?,
                label_format: None,
                clock_format: None,
                icon_format: None,
                layouts: HashMap::new(),
                bitmaps: HashMap::new(),
            })
        }
    }

    pub fn create_surface(&self, hwnd: HWND, width: u32, height: u32) -> Result<Surface> {
        let c = &self.compositor;
        unsafe {
            let target = c.cast::<ICompositorDesktopInterop>()?.CreateDesktopWindowTarget(hwnd, true)?;
            let root = c.CreateContainerVisual()?;
            target.SetRoot(&root)?;

            // Frosted glass: DWM's blurred view of whatever is behind the window,
            // clipped to the background's rounded rectangle.
            let backdrop = c.CreateSpriteVisual()?;
            backdrop.SetBrush(&c.CreateHostBackdropBrush()?)?;
            let backdrop_shape = c.CreateRoundedRectangleGeometry()?;
            backdrop.SetClip(&c.CreateGeometricClipWithGeometry(&backdrop_shape)?)?;
            root.Children()?.InsertAtTop(&backdrop)?;

            let (w, h) = (width.max(1), height.max(1));
            let surface = self.graphics.CreateDrawingSurface(
                Size { Width: w as f32, Height: h as f32 },
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                DirectXAlphaMode::Premultiplied,
            )?;
            let content = c.CreateSpriteVisual()?;
            content.SetBrush(&c.CreateSurfaceBrushWithSurface(&surface)?)?;
            content.SetSize(Vector2 { X: w as f32, Y: h as f32 })?;
            root.Children()?.InsertAtTop(&content)?;

            Ok(Surface {
                _target: target,
                root,
                backdrop,
                backdrop_shape,
                content,
                surface: surface.cast()?,
                size: (w, h),
                drawn: Some(RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }),
            })
        }
    }

    pub fn set_label_font(&mut self, family: &str, size_px: f32) {
        self.layouts.clear();
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
                    if let Ok(ellipsis) = self.dwrite.CreateEllipsisTrimmingSign(f) {
                        let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
                        let _ = f.SetTrimming(&trim, &ellipsis);
                    }
                })
                .ok()
        };
    }

    /// Lines centred both ways, so the clock's time and date line up whatever their widths.
    pub fn set_clock_font(&mut self, family: &str, size_px: f32) {
        self.layouts.retain(|k, _| k.0 != Font::Clock);
        self.clock_format = self.centred_format(family, size_px);
    }

    /// Segoe Fluent Icons on Windows 11, Segoe MDL2 Assets (mostly the same glyphs) on 10.
    pub fn set_icon_font(&mut self, size_px: f32) {
        self.layouts.retain(|k, _| k.0 != Font::Icons);
        let installed = |family: &str| unsafe {
            let mut fonts = None;
            let (mut index, mut exists) = (0, BOOL(0));
            self.dwrite.GetSystemFontCollection(&mut fonts, false).is_ok()
                && fonts.is_some_and(|f| f.FindFamilyName(&HSTRING::from(family), &mut index, &mut exists).is_ok() && exists.as_bool())
        };
        let family = if installed("Segoe Fluent Icons") { "Segoe Fluent Icons" } else { "Segoe MDL2 Assets" };
        self.icon_format = self.centred_format(family, size_px);
    }

    fn centred_format(&self, family: &str, size_px: f32) -> Option<IDWriteTextFormat> {
        unsafe {
            self.dwrite
                .CreateTextFormat(&HSTRING::from(family), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size_px, w!(""))
                .inspect(|f| {
                    let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                    let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
                    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
                })
                .ok()
        }
    }

    fn layout(&mut self, font: Font, text: &str, max_width: f32, height: f32) -> Option<&(IDWriteTextLayout, f32, f32)> {
        let key = (font, text.to_string(), max_width.max(1.0) as u32, height.max(1.0) as u32);
        if !self.layouts.contains_key(&key) {
            if self.layouts.len() > 128 {
                self.layouts.clear();
            }
            let format = match font {
                Font::Label => self.label_format.as_ref()?,
                Font::Clock => self.clock_format.as_ref()?,
                Font::Icons => self.icon_format.as_ref()?,
            };
            let utf16: Vec<u16> = text.encode_utf16().collect();
            unsafe {
                let layout = self.dwrite.CreateTextLayout(&utf16, format, key.2 as f32, key.3 as f32).ok()?;
                let mut m = DWRITE_TEXT_METRICS::default();
                layout.GetMetrics(&mut m).ok()?;
                let w = m.widthIncludingTrailingWhitespace.min(key.2 as f32).ceil();
                self.layouts.insert(key.clone(), (layout, w, m.height.ceil()));
            }
        }
        self.layouts.get(&key)
    }

    /// Size of `text` in the label font, in pixels, trimmed to `max_width`.
    pub fn measure(&mut self, text: &str, max_width: f32) -> Option<(f32, f32)> {
        self.layout(Font::Label, text, max_width, 1000.0).map(|(_, w, h)| (*w, *h))
    }

    pub fn measure_clock(&mut self, text: &str) -> Option<(f32, f32)> {
        self.layout(Font::Clock, text, 4000.0, 1000.0).map(|(_, w, h)| (*w, *h))
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

    pub fn draw(&mut self, s: &mut Surface, f: &Frame, pixels: &HashMap<String, Option<IconPixels>>) -> Result<()> {
        s.root.SetOffset(Vector3 { X: f.offset.0, Y: f.offset.1, Z: 0.0 })?;
        let show_backdrop = f.blur && f.bar.w() > 0.0;
        s.backdrop.SetIsVisible(show_backdrop)?;
        if show_backdrop {
            let size = Vector2 { X: f.bar.w(), Y: f.bar.h() };
            s.backdrop.SetOffset(Vector3 { X: f.bar.l, Y: f.bar.t, Z: 0.0 })?;
            s.backdrop.SetSize(size)?;
            s.backdrop_shape.SetSize(size)?;
            s.backdrop_shape.SetCornerRadius(Vector2 { X: f.bar_radius, Y: f.bar_radius })?;
        }

        let prepared = Prepared {
            bitmaps: f.icons.iter().map(|i| self.bitmap(i.source, pixels)).collect(),
            labels: f.labels.iter().map(|l| self.layout(Font::Label, &l.text, l.max_width, 1000.0).map(|(t, _, _)| t.clone())).collect(),
            texts: f.texts.iter().map(|t| self.layout(t.font, &t.text, t.rect.w(), t.rect.h()).map(|(l, _, _)| l.clone())).collect(),
        };

        // Only redraw what changed: this frame's content plus whatever was drawn last frame.
        let bounds = f
            .icons
            .iter()
            .map(|i| i.rect)
            .chain(f.shapes.iter().map(|s| s.rect))
            .chain(f.labels.iter().map(|l| l.rect))
            .chain(f.texts.iter().map(|t| t.rect))
            .chain(std::iter::once(f.bar).filter(|b| b.w() > 0.0))
            .reduce(|a, b| RectF { l: a.l.min(b.l), t: a.t.min(b.t), r: a.r.max(b.r), b: a.b.max(b.b) })
            .map(|r| r.inflate(2.0));
        let to_rect = |r: RectF| RECT {
            left: (r.l.floor() as i32).clamp(0, s.size.0 as i32),
            top: (r.t.floor() as i32).clamp(0, s.size.1 as i32),
            right: (r.r.ceil() as i32).clamp(0, s.size.0 as i32),
            bottom: (r.b.ceil() as i32).clamp(0, s.size.1 as i32),
        };
        let current = bounds.map(to_rect);
        let update = match (current, s.drawn) {
            (Some(a), Some(b)) => RECT { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) },
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => return Ok(()),
        };
        if update.right <= update.left || update.bottom <= update.top {
            s.drawn = current;
            return Ok(());
        }

        let mut offset = POINT::default();
        let dc: ID2D1DeviceContext = unsafe { s.surface.BeginDraw(Some(&update), &mut offset)? };
        self.paint(&dc, f, &prepared, offset, update);
        unsafe { s.surface.EndDraw()? };
        s.drawn = current;
        Ok(())
    }

    fn fill(&self, dc: &ID2D1DeviceContext, r: RectF, radius: f32, c: Color) {
        unsafe {
            self.brush.SetColor(&color(c));
            let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
            dc.FillRoundedRectangle(&rr, &self.brush);
        }
    }

    fn paint(
        &self,
        dc: &ID2D1DeviceContext,
        f: &Frame,
        p: &Prepared,
        at: POINT,
        update: RECT,
    ) {
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
            for (icon, bmp) in f.icons.iter().zip(&p.bitmaps) {
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
            for (l, layout) in f.labels.iter().zip(&p.labels) {
                let Some(layout) = layout else { continue };
                if l.bg.a > 0.0 {
                    self.fill(dc, l.rect, l.radius, l.bg);
                }
                self.brush.SetColor(&color(l.fg));
                dc.DrawTextLayout(Vector2 { X: l.rect.l + l.pad.0, Y: l.rect.t + l.pad.1 }, layout, &self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
            for (t, layout) in f.texts.iter().zip(&p.texts) {
                let Some(layout) = layout else { continue };
                self.brush.SetColor(&color(t.color));
                dc.DrawTextLayout(Vector2 { X: t.rect.l, Y: t.rect.t }, layout, &self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
            dc.PopAxisAlignedClip();
        }
    }
}
