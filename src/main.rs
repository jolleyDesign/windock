#![windows_subsystem = "windows"]

mod app;
mod apps;
mod config;
mod hotkeys;
mod icons;
mod logo;
mod render;
mod settings;
mod shell;
mod softgui;
mod taskbar;
mod util;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MessageBoxW};
use windows::core::{HSTRING, w};

fn main() {
    util::init_log();
    if std::env::args().any(|a| a == "--settings") {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        settings::run();
        return;
    }
    unsafe {
        let _instance = CreateMutexW(None, true, w!("Local\\WindockSingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // OLE (not just COM) so the dock can be a drag-and-drop target.
        let _ = OleInitialize(None);
    }
    crate::log!("starting Windock {}", env!("CARGO_PKG_VERSION"));
    if let Err(e) = app::run() {
        crate::log!("fatal: {e}");
        unsafe {
            MessageBoxW(None, &HSTRING::from(format!("Windock failed to start:\n{e}")), w!("Windock"), MB_ICONERROR);
        }
    }
}
