//! Other apps' notification-area icons: OneDrive, Slack, VPN clients and so on.
//!
//! Apps add them with `Shell_NotifyIcon`, which finds the top-level `Shell_TrayWnd` window and
//! sends it a WM_COPYDATA. While the dock shows these icons it opens a hidden window of that
//! class above Explorer's, keeps a copy of every icon, and passes every message on to Explorer.
//! Explorer's tray stays exactly as it would have been, so nothing is lost when the dock quits or
//! crashes: the window goes away and `Shell_NotifyIcon` finds Explorer's again.
//!
//! Icons added before the window existed are collected by broadcasting TaskbarCreated, which
//! Explorer sends when it restarts and which makes every app add its icons again.
//!
//! Other things are sent to `Shell_TrayWnd` too (app bar messages from `SHAppBarMessage`, and
//! WM_COMMANDs like "minimize all"); they are forwarded untouched. The window also mirrors the
//! real taskbar's position for apps that look it up.

use crate::icons::IconPixels;
use crate::util::from_wide;
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE};
use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, FOLDERID_System, FOLDERID_SystemX86, FOLDERID_Windows, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{GUID, PCWSTR, w};

const SIGNATURE: u32 = 0x3475_3423;
/// WM_COPYDATA kinds sent to the tray.
const COPYDATA_NOTIFY: usize = 1;
const COPYDATA_ICON_RECT: usize = 3;

const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIM_SETFOCUS: u32 = 3;
const NIM_SETVERSION: u32 = 4;

const NIF_MESSAGE: u32 = 0x01;
const NIF_ICON: u32 = 0x02;
const NIF_TIP: u32 = 0x04;
const NIF_STATE: u32 = 0x08;
const NIF_GUID: u32 = 0x20;
const NIS_HIDDEN: u32 = 0x01;

const NIN_SELECT: u32 = WM_USER;
const NOTIFYICON_VERSION_4: u32 = 4;

/// Asks the tray thread to check it's still the window `Shell_NotifyIcon` finds.
const WM_APP_CHECK: u32 = WM_APP + 1;

/// `NOTIFYICONDATAW` as the shell sends it: handles are 32 bits so 32- and 64-bit apps agree.
/// Apps built for older Windows send a shorter one; missing fields read as zero.
#[repr(C)]
#[derive(Clone, Copy)]
struct NotifyIconData {
    cb_size: u32,
    hwnd: u32,
    uid: u32,
    flags: u32,
    callback: u32,
    icon: u32,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    /// `uTimeout` or `uVersion`.
    version: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: GUID,
    balloon_icon: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TrayNotifyData {
    signature: u32,
    message: u32,
    nid: NotifyIconData,
}

/// What `Shell_NotifyIconGetRect` sends.
#[repr(C)]
#[derive(Clone, Copy)]
struct IconIdentifier {
    signature: u32,
    /// 1 asks for the top-left corner, 2 for the bottom-right.
    message: u32,
    cb_size: u32,
    padding: u32,
    hwnd: u32,
    uid: u32,
    guid: GUID,
}

/// One app's icon.
#[derive(Clone, Debug, PartialEq)]
pub struct Icon {
    /// The window that gets the icon's mouse messages.
    pub owner: isize,
    pub uid: u32,
    pub guid: Option<GUID>,
    pub callback: u32,
    pub version: u32,
    pub tip: String,
    /// The app asked for it to be hidden for now.
    pub hidden: bool,
    /// Shown on the taskbar rather than in the overflow, as set in Windows' Settings.
    pub promoted: bool,
    /// Changes whenever the picture does; it names the picture in the dock's icon cache.
    pub image: u64,
}

impl Icon {
    pub fn id(&self) -> String {
        match &self.guid {
            Some(g) => format!("tray:{g:?}"),
            None => format!("tray:{}:{}", self.owner, self.uid),
        }
    }

    fn is(&self, owner: isize, uid: u32, guid: Option<GUID>) -> bool {
        match (guid, self.guid) {
            (Some(a), Some(b)) => a == b,
            _ => self.owner == owner && self.uid == uid,
        }
    }
}

#[derive(Default)]
struct Shared {
    icons: Vec<Icon>,
    /// Pictures the dock hasn't taken yet, by `Icon::image`.
    images: Vec<(u64, Option<IconPixels>)>,
    /// Where the dock drew each icon (by `Icon::id`), for `Shell_NotifyIconGetRect`.
    rects: Vec<(String, RECT)>,
}

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);
static NEXT_IMAGE: AtomicU64 = AtomicU64::new(1);
/// The dock window and the message it's posted when the icons change.
static NOTIFY_HWND: AtomicIsize = AtomicIsize::new(0);
static NOTIFY_MSG: AtomicU32 = AtomicU32::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

fn with_shared<R>(f: impl FnOnce(&mut Shared) -> R) -> R {
    let mut guard = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Shared::default))
}

fn changed() {
    unsafe {
        let _ = PostMessageW(Some(HWND(NOTIFY_HWND.load(Ordering::Relaxed) as _)), NOTIFY_MSG.load(Ordering::Relaxed), WPARAM(0), LPARAM(0));
    }
}

/// Runs the tray window on its own thread while it exists.
pub struct Host {
    hwnd: isize,
    /// Signalled when the thread has closed its window.
    done: std::sync::mpsc::Receiver<()>,
}

impl Host {
    /// Posts `msg` to `notify` whenever the icons change.
    pub fn start(notify: HWND, msg: u32) -> Option<Host> {
        NOTIFY_HWND.store(notify.0 as isize, Ordering::Relaxed);
        NOTIFY_MSG.store(msg, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            run(tx);
            let _ = done_tx.send(());
        });
        match rx.recv() {
            Ok(hwnd) if hwnd != 0 => Some(Host { hwnd, done }),
            _ => None,
        }
    }

    pub fn icons(&self) -> Vec<Icon> {
        with_shared(|s| s.icons.clone())
    }

    pub fn take_images(&self) -> Vec<(u64, Option<IconPixels>)> {
        with_shared(|s| std::mem::take(&mut s.images))
    }

    pub fn set_rects(&self, rects: Vec<(String, RECT)>) {
        with_shared(|s| s.rects = rects);
    }

    /// Forgets icons whose app went away without removing them, as Explorer does.
    pub fn prune(&self) -> bool {
        with_shared(|s| {
            let before = s.icons.len();
            s.icons.retain(|i| unsafe { IsWindow(Some(HWND(i.owner as _))) }.as_bool());
            s.icons.len() != before
        })
    }

    /// Makes sure apps still find this window rather than Explorer's, which can move above it
    /// when Explorer shows the taskbar again.
    pub fn check(&self) {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as _)), WM_APP_CHECK, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        // Don't hang the dock's exit on it: the process ending takes the window with it.
        if self.done.recv_timeout(std::time::Duration::from_secs(2)).is_err() {
            crate::log!("tray: window didn't close in time");
        }
        with_shared(|s| *s = Shared::default());
    }
}

fn run(ready: std::sync::mpsc::Sender<isize>) {
    unsafe {
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        let instance: HINSTANCE = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
        let class = w!("Shell_TrayWnd");
        let wc = WNDCLASSEXW { cbSize: size_of::<WNDCLASSEXW>() as u32, lpfnWndProc: Some(wndproc), hInstance: instance, lpszClassName: class, ..Default::default() };
        RegisterClassExW(&wc);
        let hwnd = match CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW, class, None, WS_POPUP, 0, 0, 0, 0, None, None, Some(instance), None) {
            Ok(h) => h,
            Err(e) => {
                crate::log!("tray window failed: {e}");
                let _ = ready.send(0);
                return;
            }
        };
        // Apps at a lower integrity level (sandboxed ones) can still add icons, as with Explorer.
        let _ = ChangeWindowMessageFilterEx(hwnd, WM_COPYDATA, MSGFLT_ALLOW, None);
        keep_in_front(hwnd);
        let _ = ready.send(hwnd.0 as isize);
        // Every app adds its icons again, now to this window.
        let _ = SendNotifyMessageW(HWND_BROADCAST, TASKBAR_CREATED.load(Ordering::Relaxed), WPARAM(0), LPARAM(0));

        // Re-read which icons are promoted whenever that's changed in Settings.
        let settings = Settings::watch();
        let mut msg = MSG::default();
        let handles: Vec<HANDLE> = settings.as_ref().map(|s| s.event).into_iter().collect();
        loop {
            // MWMO_INPUTAVAILABLE: also wake for messages already queued, which a plain wait
            // misses once the queue has been looked at.
            let r = MsgWaitForMultipleObjectsEx(Some(&handles), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            if !handles.is_empty() && r == WAIT_OBJECT_0 {
                if let Some(s) = &settings {
                    s.rearm();
                }
                if refresh_promoted() {
                    changed();
                }
            }
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

/// Puts the window above Explorer's tray so `FindWindow` returns it first, and gives it the
/// taskbar's position for apps that ask where the taskbar is.
fn keep_in_front(hwnd: HWND) {
    unsafe {
        let mut r = RECT::default();
        if let Some(t) = crate::taskbar::explorer_tray() {
            let _ = GetWindowRect(t, &mut r);
        }
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOACTIVATE);
        if FindWindowW(w!("Shell_TrayWnd"), None).ok() != Some(hwnd) {
            crate::log!("tray: another Shell_TrayWnd is in front");
        }
    }
}

/// Sends a message on to Explorer's tray and returns its answer.
fn forward(msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(tray) = crate::taskbar::explorer_tray() else { return LRESULT(0) };
    let mut result = 0usize;
    unsafe {
        SendMessageTimeoutW(tray, msg, wp, lp, SMTO_ABORTIFHUNG, 4000, Some(&mut result));
    }
    LRESULT(result as isize)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COPYDATA => {
            let cds = unsafe { &*(lp.0 as *const COPYDATASTRUCT) };
            match cds.dwData {
                COPYDATA_NOTIFY => {
                    let ours = unsafe { read::<TrayNotifyData>(cds) }.filter(|d| d.signature == SIGNATURE).map(|d| on_notify(d.message, &d.nid));
                    let theirs = forward(msg, wp, lp);
                    // Explorer refuses an icon it already has (one added before this window existed).
                    if ours == Some(true) { LRESULT(1) } else { theirs }
                }
                COPYDATA_ICON_RECT => match unsafe { read::<IconIdentifier>(cds) }.and_then(icon_rect) {
                    Some(r) => r,
                    None => forward(msg, wp, lp),
                },
                _ => forward(msg, wp, lp),
            }
        }
        WM_COMMAND => forward(msg, wp, lp),
        WM_SYSCOMMAND if wp.0 as u32 == SC_TASKLIST => forward(msg, wp, lp),
        m if (WM_USER..WM_APP).contains(&m) => forward(msg, wp, lp),
        WM_APP_CHECK => {
            keep_in_front(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Copies a WM_COPYDATA payload into `T`, zero-filling whatever an older sender left out.
unsafe fn read<T: Copy>(cds: &COPYDATASTRUCT) -> Option<T> {
    if cds.lpData.is_null() || (cds.cbData as usize) < 8 {
        return None;
    }
    unsafe {
        let mut out: T = std::mem::zeroed();
        let n = (cds.cbData as usize).min(size_of::<T>());
        std::ptr::copy_nonoverlapping(cds.lpData as *const u8, &mut out as *mut T as *mut u8, n);
        Some(out)
    }
}

/// Handles are sign-extended from their 32 bits.
fn handle(v: u32) -> isize {
    v as i32 as isize
}

/// Updates the icon list; returns whether the request made sense.
fn on_notify(message: u32, nid: &NotifyIconData) -> bool {
    let owner = handle(nid.hwnd);
    let guid = (nid.flags & NIF_GUID != 0 && nid.guid != GUID::zeroed()).then_some(nid.guid);
    let ok = with_shared(|s| {
        let at = s.icons.iter().position(|i| i.is(owner, nid.uid, guid));
        match message {
            // A modify for an icon added before this window existed (by an app that ignores
            // TaskbarCreated) works like an add, if it carries enough to show and click it.
            NIM_ADD | NIM_MODIFY => {
                let complete = nid.flags & (NIF_ICON | NIF_MESSAGE) == NIF_ICON | NIF_MESSAGE;
                let at = match at {
                    Some(at) => at,
                    None if message == NIM_ADD || complete => {
                        s.icons.push(Icon { owner, uid: nid.uid, guid, callback: 0, version: 0, tip: String::new(), hidden: false, promoted: false, image: 0 });
                        let i = s.icons.len() - 1;
                        s.icons[i].promoted = is_promoted(&s.icons[i]);
                        i
                    }
                    None => return false,
                };
                let icon = &mut s.icons[at];
                icon.owner = owner;
                if nid.flags & NIF_MESSAGE != 0 {
                    icon.callback = nid.callback;
                }
                if nid.flags & NIF_TIP != 0 {
                    icon.tip = from_wide(&nid.tip);
                }
                if nid.flags & NIF_STATE != 0 && nid.state_mask & NIS_HIDDEN != 0 {
                    icon.hidden = nid.state & NIS_HIDDEN != 0;
                }
                if nid.flags & NIF_ICON != 0 {
                    let image = NEXT_IMAGE.fetch_add(1, Ordering::Relaxed);
                    icon.image = image;
                    let pixels = crate::icons::icon_pixels(HICON(handle(nid.icon) as _));
                    s.images.retain(|(old, _)| *old != image);
                    s.images.push((image, pixels));
                }
                true
            }
            NIM_DELETE => {
                if let Some(at) = at {
                    s.icons.remove(at);
                }
                at.is_some()
            }
            NIM_SETVERSION => {
                if let Some(at) = at {
                    s.icons[at].version = nid.version;
                }
                at.is_some()
            }
            NIM_SETFOCUS => true,
            _ => false,
        }
    });
    if ok {
        changed();
    }
    ok
}

/// `Shell_NotifyIconGetRect`: apps ask where their icon is to place a flyout beside it.
fn icon_rect(q: IconIdentifier) -> Option<LRESULT> {
    if q.signature != SIGNATURE {
        return None;
    }
    let owner = handle(q.hwnd);
    let guid = (q.guid != GUID::zeroed()).then_some(q.guid);
    let rect = with_shared(|s| {
        let icon = s.icons.iter().find(|i| i.is(owner, q.uid, guid))?;
        let id = icon.id();
        s.rects.iter().find(|(r, _)| *r == id).map(|(_, rc)| *rc)
    })?;
    let pack = |x: i32, y: i32| LRESULT(((x as u16 as u32) | ((y as u16 as u32) << 16)) as i32 as isize);
    match q.message {
        1 => Some(pack(rect.left, rect.top)),
        2 => Some(pack(rect.right, rect.bottom)),
        _ => None,
    }
}

// ---------------------------------------------------------------- clicks

#[derive(Clone, Copy, PartialEq)]
pub enum Click {
    Left,
    Double,
    Right,
    /// The pointer moved onto the icon; some apps refresh their tooltip then.
    Hover,
}

/// Sends an icon's owner the mouse messages Explorer would for a click at screen point `at`.
pub fn click(icon: &Icon, click: Click, at: (i32, i32)) {
    let owner = HWND(icon.owner as _);
    unsafe {
        if click != Click::Hover {
            // The app usually brings a window or menu forward; let it.
            let _ = AllowSetForegroundWindow(crate::apps::pid_of(owner));
        }
    }
    let events: &[u32] = match click {
        Click::Left => &[WM_LBUTTONDOWN, WM_LBUTTONUP, NIN_SELECT],
        Click::Double => &[WM_LBUTTONDBLCLK, WM_LBUTTONUP],
        Click::Right => &[WM_RBUTTONDOWN, WM_RBUTTONUP, WM_CONTEXTMENU],
        Click::Hover => &[WM_MOUSEMOVE],
    };
    for &event in events {
        let (wp, lp) = if icon.version >= NOTIFYICON_VERSION_4 {
            let xy = (at.0 as u16 as usize) | ((at.1 as u16 as usize) << 16);
            (xy, (event as usize & 0xffff) | ((icon.uid as usize & 0xffff) << 16))
        } else {
            // Before version 4 there's no NIN_SELECT or WM_CONTEXTMENU.
            if matches!(event, NIN_SELECT | WM_CONTEXTMENU) {
                continue;
            }
            (icon.uid as usize, event as usize)
        };
        unsafe {
            let _ = SendNotifyMessageW(owner, icon.callback, WPARAM(wp), LPARAM(lp as isize));
        }
    }
}

// ---------------------------------------------------------------- promoted icons

/// Windows 11 keeps "show in the taskbar" per icon under this key: one subkey per icon with its
/// app's path (folders written as known-folder GUIDs) and either its uID or its GUID.
const SETTINGS_KEY: PCWSTR = w!("Control Panel\\NotifyIconSettings");

struct Settings {
    key: HKEY,
    event: HANDLE,
}

impl Settings {
    fn watch() -> Option<Settings> {
        unsafe {
            let mut key = HKEY::default();
            if RegOpenKeyExW(HKEY_CURRENT_USER, SETTINGS_KEY, None, KEY_NOTIFY | KEY_READ, &mut key) != ERROR_SUCCESS {
                return None;
            }
            let event = CreateEventW(None, false, false, None).ok()?;
            let s = Settings { key, event };
            s.rearm();
            Some(s)
        }
    }

    fn rearm(&self) {
        unsafe {
            let _ = RegNotifyChangeKeyValue(self.key, true, REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET, Some(self.event), true);
        }
    }
}

impl Drop for Settings {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.key);
            let _ = CloseHandle(self.event);
        }
    }
}

struct Entry {
    path: String,
    uid: Option<u32>,
    guid: Option<GUID>,
    promoted: bool,
}

/// Re-reads every icon's promoted state; returns whether any changed.
fn refresh_promoted() -> bool {
    let entries = read_entries();
    with_shared(|s| {
        let mut any = false;
        for icon in &mut s.icons {
            let p = promoted_in(&entries, icon);
            any |= p != icon.promoted;
            icon.promoted = p;
        }
        any
    })
}

fn is_promoted(icon: &Icon) -> bool {
    promoted_in(&read_entries(), icon)
}

fn promoted_in(entries: &[Entry], icon: &Icon) -> bool {
    let exe = crate::apps::process_path(crate::apps::pid_of(HWND(icon.owner as _))).unwrap_or_default();
    entries.iter().any(|e| {
        let same = match (icon.guid, e.guid) {
            (Some(a), Some(b)) => a == b,
            _ => e.uid == Some(icon.uid) && e.path.eq_ignore_ascii_case(&exe),
        };
        same && e.promoted
    })
}

fn read_entries() -> Vec<Entry> {
    let mut out = Vec::new();
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, SETTINGS_KEY, None, KEY_READ, &mut key) != ERROR_SUCCESS {
            return out;
        }
        let mut index = 0;
        loop {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(key, index, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) != ERROR_SUCCESS {
                break;
            }
            index += 1;
            let sub = PCWSTR(name.as_ptr());
            let path = reg_string(key, sub, w!("ExecutablePath")).map(|p| expand_known_folder(&p)).unwrap_or_default();
            let guid = reg_string(key, sub, w!("IconGuid")).and_then(|g| parse_guid(&g));
            out.push(Entry { path, uid: reg_dword(key, sub, w!("UID")), guid, promoted: reg_dword(key, sub, w!("IsPromoted")) == Some(1) });
        }
        let _ = RegCloseKey(key);
    }
    out
}

fn reg_dword(key: HKEY, sub: PCWSTR, value: PCWSTR) -> Option<u32> {
    let mut data = 0u32;
    let mut size = 4u32;
    let r = unsafe { RegGetValueW(key, sub, value, RRF_RT_REG_DWORD, None, Some(&mut data as *mut u32 as _), Some(&mut size)) };
    (r == ERROR_SUCCESS).then_some(data)
}

fn reg_string(key: HKEY, sub: PCWSTR, value: PCWSTR) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe { RegGetValueW(key, sub, value, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as _), Some(&mut size)) };
    (r == ERROR_SUCCESS).then(|| from_wide(&buf))
}

/// `{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\cmd.exe` → `C:\Windows\System32\cmd.exe`.
fn expand_known_folder(path: &str) -> String {
    let Some((head, rest)) = path.split_once('\\').filter(|(h, _)| h.starts_with('{')) else { return path.to_string() };
    let Some(id) = parse_guid(head) else { return path.to_string() };
    let known = [FOLDERID_System, FOLDERID_SystemX86, FOLDERID_Windows, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86];
    if !known.contains(&id) {
        return path.to_string();
    }
    unsafe {
        match SHGetKnownFolderPath(&id, KF_FLAG_DEFAULT, None) {
            Ok(p) => {
                let base = p.to_string().unwrap_or_default();
                windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _));
                format!("{base}\\{rest}")
            }
            Err(_) => path.to_string(),
        }
    }
}

/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`, braces optional.
fn parse_guid(s: &str) -> Option<GUID> {
    let hex: String = s.trim().trim_start_matches('{').trim_end_matches('}').chars().filter(|&c| c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok().map(GUID::from_u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_guids_with_or_without_braces() {
        let g = GUID::from_u128(0x1ac14e77_02e7_4e5d_b744_2eb1ae5198b7);
        assert_eq!(parse_guid("{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}"), Some(g));
        assert_eq!(parse_guid("1ac14e77-02e7-4e5d-b744-2eb1ae5198b7"), Some(g));
        assert_eq!(parse_guid("{not-a-guid}"), None);
    }

    #[test]
    fn leaves_plain_paths_alone() {
        assert_eq!(expand_known_folder(r"C:\Program Files\App\app.exe"), r"C:\Program Files\App\app.exe");
    }

    #[test]
    fn notify_data_matches_the_shells_layout() {
        // dwSignature + dwMessage + NOTIFYICONDATAW with 32-bit handles.
        assert_eq!(size_of::<NotifyIconData>(), 956);
        assert_eq!(size_of::<TrayNotifyData>(), 964);
    }
}
