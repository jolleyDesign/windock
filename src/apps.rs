//! Finding taskbar-worthy windows and working out which app each belongs to.

use crate::util::from_wide;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTONULL, MonitorFromWindow};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::Shell::{QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PWSTR};

/// Identifies an app. Windows match pinned items by AppUserModelID first, then by executable.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AppKey {
    pub aumid: Option<String>,
    pub exe: Option<String>,
    /// The process is a packaged (Store/MSIX) app, so it should be launched via its AUMID.
    pub packaged: bool,
}

impl AppKey {
    pub fn matches(&self, other: &AppKey) -> bool {
        if let (Some(a), Some(b)) = (&self.aumid, &other.aumid)
            && a.eq_ignore_ascii_case(b) {
                return true;
            }
        if let (Some(a), Some(b)) = (&self.exe, &other.exe) {
            // Two different explicit IDs on the same exe (e.g. browser profiles) are different apps.
            return a.eq_ignore_ascii_case(b) && (self.aumid.is_none() || other.aumid.is_none());
        }
        false
    }

    pub fn group_id(&self) -> String {
        let k = self.aumid.as_ref().or(self.exe.as_ref()).cloned().unwrap_or_default();
        format!("r:{}", k.to_lowercase())
    }
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    from_wide(&buf[..n])
}

pub fn title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
    from_wide(&buf[..n])
}

fn pid_of(hwnd: HWND) -> u32 {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as _, 4).is_ok() && cloaked != 0
    }
}

/// Roughly the same rules the taskbar and Alt+Tab use.
pub fn is_app_window(hwnd: HWND, own_pid: u32) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        let ex = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
        let app_window = ex.contains(WS_EX_APPWINDOW);
        if !app_window {
            if ex.contains(WS_EX_TOOLWINDOW) || ex.contains(WS_EX_NOACTIVATE) {
                return false;
            }
            if GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
                return false;
            }
        }
        if GetWindowTextLengthW(hwnd) == 0 || is_cloaked(hwnd) || pid_of(hwnd) == own_pid {
            return false;
        }
        !matches!(
            class_name(hwnd).as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Windows.UI.Core.CoreWindow"
        )
    }
}

/// Top-level app windows in z-order (topmost first).
pub fn app_windows(own_pid: u32) -> Vec<HWND> {
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lp.0 as *mut Vec<HWND>) };
        out.push(hwnd);
        true.into()
    }
    let mut all: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut all as *mut _ as isize));
    }
    all.retain(|&h| is_app_window(h, own_pid));
    all
}

fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok().map(|_| from_wide(&buf[..len as usize]))
    }
}

fn package_aumid(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let r = GetApplicationUserModelId(h, &mut len, Some(PWSTR(buf.as_mut_ptr())));
        let _ = CloseHandle(h);
        (r == ERROR_SUCCESS).then(|| from_wide(&buf)).filter(|s| !s.is_empty())
    }
}

fn window_aumid(hwnd: HWND) -> Option<String> {
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
        let v = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        Some(v.to_string()).filter(|s| !s.is_empty())
    }
}

/// UWP apps live inside an ApplicationFrameHost window; the real app is a child from another process.
fn hosted_app_pid(frame: HWND) -> Option<u32> {
    unsafe extern "system" fn cb(child: HWND, lp: LPARAM) -> BOOL {
        let state = unsafe { &mut *(lp.0 as *mut (u32, u32)) };
        let pid = pid_of(child);
        if pid != state.0 {
            state.1 = pid;
            return false.into();
        }
        true.into()
    }
    let mut state = (pid_of(frame), 0u32);
    unsafe {
        let _ = EnumChildWindows(Some(frame), Some(cb), LPARAM(&mut state as *mut _ as isize));
    }
    (state.1 != 0).then_some(state.1)
}

pub fn window_key(hwnd: HWND) -> AppKey {
    let mut pid = pid_of(hwnd);
    let explicit = window_aumid(hwnd);
    if class_name(hwnd) == "ApplicationFrameWindow"
        && let Some(p) = hosted_app_pid(hwnd) {
            pid = p;
        }
    let pkg = package_aumid(pid);
    AppKey {
        packaged: pkg.is_some(),
        aumid: explicit.or(pkg),
        exe: process_path(pid),
    }
}

pub fn activate(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindowAsync(hwnd, SW_RESTORE);
        }
        if !SetForegroundWindow(hwnd).as_bool() {
            // Windows only lets the process that received the last input steal focus;
            // a synthetic Alt tap satisfies that rule.
            let alt = |flags| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT { wVk: VK_MENU, dwFlags: flags, ..Default::default() },
                },
            };
            SendInput(&[alt(KEYBD_EVENT_FLAGS(0)), alt(KEYEVENTF_KEYUP)], size_of::<INPUT>() as i32);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

pub fn minimize(hwnd: HWND) {
    unsafe {
        let _ = ShowWindowAsync(hwnd, SW_MINIMIZE);
    }
}

pub fn close(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

pub fn foreground() -> HWND {
    unsafe { GetForegroundWindow() }
}

/// True when a game, video or presentation is running full screen on `monitor`.
pub fn fullscreen_app_on(monitor: HMONITOR) -> bool {
    unsafe {
        let busy = matches!(
            SHQueryUserNotificationState(),
            Ok(s) if s == QUNS_BUSY || s == QUNS_RUNNING_D3D_FULL_SCREEN || s == QUNS_PRESENTATION_MODE
        );
        if !busy {
            return false;
        }
        let fg = GetForegroundWindow();
        !fg.is_invalid() && MonitorFromWindow(fg, MONITOR_DEFAULTTONULL) == monitor
    }
}
