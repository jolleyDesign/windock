//! A minimal egui host for the settings window: a winit window, egui-winit for input, and
//! a CPU rasterizer blitted with GDI. eframe needs OpenGL or wgpu, and either one loads the
//! GPU driver, which costs 150+ MB for what is a small form.

mod raster;

use egui::{ViewportBuilder, ViewportCommand, ViewportId, ViewportInfo};
use raster::Raster;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, ReleaseDC, SetDIBitsToDevice};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

/// Large enough for egui's font atlas at any sensible scale.
const MAX_TEXTURE_SIDE: usize = 8192;

pub trait App {
    fn ui(&mut self, ui: &mut egui::Ui);
    /// The window is closing.
    fn on_exit(&mut self) {}
}

pub fn run<A: App>(viewport: ViewportBuilder, make_app: impl FnOnce(&egui::Context) -> A) -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|e| e.to_string())?;
    let ctx = egui::Context::default();
    let app = make_app(&ctx);
    let mut host = Host {
        ctx,
        app,
        viewport,
        window: None,
        info: ViewportInfo::default(),
        raster: Raster::default(),
        repaint_at: None,
        last_frame: Instant::now(),
        frame_interval: Duration::from_secs(1) / 60,
    };
    event_loop.run_app(&mut host).map_err(|e| e.to_string())
}

struct Host<A> {
    ctx: egui::Context,
    app: A,
    viewport: ViewportBuilder,
    window: Option<(Window, egui_winit::State)>,
    info: ViewportInfo,
    raster: Raster,
    /// When egui next wants a frame (animations, `request_repaint_after`, input).
    repaint_at: Option<Instant>,
    last_frame: Instant,
    /// Frames are spaced at least this far apart: the display's refresh interval.
    frame_interval: Duration,
}

impl<A: App> Host<A> {
    fn next_frame(&self) -> Option<Instant> {
        self.repaint_at.map(|t| t.max(self.last_frame + self.frame_interval))
    }

    fn exit(&mut self, event_loop: &ActiveEventLoop) {
        self.app.on_exit();
        event_loop.exit();
    }

    fn frame(&mut self, event_loop: &ActiveEventLoop) {
        let Some((window, state)) = &mut self.window else { return };
        self.repaint_at = None;
        self.last_frame = Instant::now();
        egui_winit::update_viewport_info(&mut self.info, &self.ctx, window, false);
        let mut input = state.take_egui_input(window);
        input.viewports = std::iter::once((ViewportId::ROOT, self.info.clone())).collect();
        let app = &mut self.app;
        let mut out = self.ctx.run_ui(input, |ui| app.ui(ui));
        state.handle_platform_output(window, out.platform_output);

        // The app still runs while minimized (it saves pending edits), there's just nothing to draw.
        let size = window.inner_size();
        if size.width > 0 && size.height > 0 {
            let primitives = self.ctx.tessellate(out.shapes, out.pixels_per_point);
            let clear = self.ctx.global_style().visuals.panel_fill;
            self.raster.set_textures(&out.textures_delta);
            self.raster.paint(size.width as usize, size.height as usize, out.pixels_per_point, &primitives, clear);
            self.raster.free_textures(&out.textures_delta);
            present(window, &self.raster);
            // Created hidden so the first thing on screen is a finished frame.
            if !window.is_visible().unwrap_or(true) {
                window.set_visible(true);
            }
        }

        let Some(viewport) = out.viewport_output.remove(&ViewportId::ROOT) else { return };
        if viewport.commands.contains(&ViewportCommand::Close) {
            self.exit(event_loop);
            return;
        }
        egui_winit::process_viewport_commands(&self.ctx, &mut self.info, viewport.commands, window, &mut Vec::new());
        self.repaint_at = Instant::now().checked_add(viewport.repaint_delay);
    }
}

impl<A: App> ApplicationHandler for Host<A> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = match egui_winit::create_window(&self.ctx, event_loop, &self.viewport.clone().with_visible(false)) {
            Ok(w) => w,
            Err(e) => {
                crate::log!("settings window: {e}");
                event_loop.exit();
                return;
            }
        };
        let state = egui_winit::State::new(
            self.ctx.clone(),
            ViewportId::ROOT,
            event_loop,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(MAX_TEXTURE_SIDE),
        );
        egui_winit::update_viewport_info(&mut self.info, &self.ctx, &window, true);
        if let Some(mhz) = window.current_monitor().and_then(|m| m.refresh_rate_millihertz()).filter(|&m| m > 0) {
            self.frame_interval = Duration::from_secs(1000) / mhz;
        }
        self.window = Some((window, state));
        self.frame(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => return self.exit(event_loop),
            WindowEvent::RedrawRequested => return self.frame(event_loop),
            _ => {}
        }
        let Some((window, state)) = &mut self.window else { return };
        let response = state.on_window_event(window, &event);
        if matches!(event, WindowEvent::Resized(_)) {
            window.request_redraw();
        } else if response.repaint {
            // Batched: everything that arrives before the loop goes idle makes one frame.
            self.repaint_at = Some(Instant::now());
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.next_frame().is_some_and(|t| t <= Instant::now()) {
            self.frame(event_loop);
        }
        if !event_loop.exiting() {
            event_loop.set_control_flow(self.next_frame().map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
        }
    }
}

fn present(window: &Window, raster: &Raster) {
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
    let hwnd = HWND(handle.hwnd.get() as _);
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: raster.width as i32,
            biHeight: -(raster.height as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe {
        let dc = GetDC(Some(hwnd));
        SetDIBitsToDevice(
            dc,
            0,
            0,
            raster.width as u32,
            raster.height as u32,
            0,
            0,
            0,
            raster.height as u32,
            raster.pixels.as_ptr().cast(),
            &info,
            DIB_RGB_COLORS,
        );
        ReleaseDC(Some(hwnd), dc);
    }
}
