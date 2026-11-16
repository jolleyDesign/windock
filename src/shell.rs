//! Thin wrappers over Win32 shell, registry and theme APIs.

use crate::util::{from_wide, wide};
use std::ffi::c_void;
use std::path::Path;
use windows::Win32::Foundation::*;
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, CoCreateInstance, CoTaskMemFree, IPersistFile, STGM_READ};
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_A, VK_LWIN, VK_N, VK_X};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, SW_SHOWNORMAL};
use windows::core::{HSTRING, PCWSTR, w};

pub const APPS_FOLDER: &str = r"shell:AppsFolder\";

pub fn apps_folder_aumid(path: &str) -> Option<&str> {
    let n = APPS_FOLDER.len();
    (path.len() > n && path[..n].eq_ignore_ascii_case(APPS_FOLDER)).then(|| &path[n..])
}

/// Expands `%VARIABLES%` in a path.
pub fn expand_env(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let src = wide(s);
    let mut buf = vec![0u16; 2048];
    let n = unsafe { windows::Win32::System::Environment::ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf)) };
    if n == 0 || n as usize > buf.len() {
        return s.to_string();
    }
    from_wide(&buf)
}

pub fn launch(path: &str, args: Option<&str>, cwd: Option<&str>) -> bool {
    let file = wide(path);
    let params = args.filter(|a| !a.is_empty()).map(wide);
    // Default the working directory to the program's folder, like Explorer does.
    let dir = cwd.map(str::to_owned).or_else(|| {
        let p = Path::new(path);
        (apps_folder_aumid(path).is_none() && p.is_file()).then(|| p.parent().map(|d| d.to_string_lossy().into_owned())).flatten()
    });
    let dir = dir.map(|d| wide(&d));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("open"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: params.as_ref().map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
        lpDirectory: dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    let ok = unsafe { ShellExecuteExW(&mut info) }.is_ok();
    if !ok {
        crate::log!("launch failed: {path}");
    }
    ok
}

/// Opens a file for editing, falling back to Notepad when nothing is associated.
pub fn edit_file(path: &Path) {
    let p = path.to_string_lossy();
    let file = wide(&p);
    let r = unsafe { ShellExecuteW(None, w!("open"), PCWSTR(file.as_ptr()), None, None, SW_SHOWNORMAL) };
    if r.0 as isize <= 32 {
        launch("notepad.exe", Some(&format!("\"{p}\"")), None);
    }
}

/// Target path and AppUserModelID of a .lnk shortcut.
pub fn resolve_lnk(path: &str) -> (Option<String>, Option<String>) {
    unsafe {
        let Ok(link) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER) else {
            return (None, None);
        };
        let Ok(file) = windows::core::Interface::cast::<IPersistFile>(&link) else {
            return (None, None);
        };
        if file.Load(&HSTRING::from(path), STGM_READ).is_err() {
            return (None, None);
        }
        let mut buf = [0u16; 1024];
        let target = link.GetPath(&mut buf, std::ptr::null_mut(), 0).ok().map(|_| from_wide(&buf)).filter(|s| !s.is_empty());
        let aumid = windows::core::Interface::cast::<IPropertyStore>(&link)
            .ok()
            .and_then(|store| store.GetValue(&PKEY_AppUserModel_ID).ok())
            .map(|v| v.to_string())
            .filter(|s| !s.is_empty());
        (target, aumid)
    }
}

/// What a shortcut runs: target, arguments and working folder.
fn lnk_command(path: &str) -> Option<(String, String, Option<String>)> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        windows::core::Interface::cast::<IPersistFile>(&link).ok()?.Load(&HSTRING::from(path), STGM_READ).ok()?;
        let mut buf = [0u16; 1024];
        link.GetPath(&mut buf, std::ptr::null_mut(), 0).ok()?;
        let target = from_wide(&buf);
        if target.is_empty() {
            return None;
        }
        let mut args = [0u16; 4096];
        let _ = link.GetArguments(&mut args);
        let mut dir = [0u16; 1024];
        let _ = link.GetWorkingDirectory(&mut dir);
        Some((target, from_wide(&args), Some(from_wide(&dir)).filter(|d| !d.is_empty())))
    }
}

/// Opens files with an app: an .exe, a shortcut (its target gets the files after its own
/// arguments) or a Store app ('shell:AppsFolder\<AUMID>').
pub fn open_with(app: &str, app_args: Option<&str>, cwd: Option<&str>, files: &[String]) -> bool {
    if let Some(aumid) = apps_folder_aumid(app) {
        return activate_with_files(aumid, files);
    }
    let quoted: Vec<String> = files.iter().map(|f| format!("\"{f}\"")).collect();
    let (program, base_args, dir) = if app.to_lowercase().ends_with(".lnk") {
        match lnk_command(app) {
            Some((target, args, dir)) => (target, args, dir.or(cwd.map(str::to_owned))),
            // Advertised (installer) shortcuts have no plain target; let the shell run them.
            None => (app.to_string(), String::new(), cwd.map(str::to_owned)),
        }
    } else {
        (app.to_string(), app_args.unwrap_or_default().to_string(), cwd.map(str::to_owned))
    };
    let args = [base_args.trim().to_string(), quoted.join(" ")].join(" ").trim().to_string();
    launch(&program, Some(&args), dir.as_deref())
}

/// Starts a Store app with the files as its command line. Packaged desktop apps (Notepad,
/// Terminal, Paint...) open them; classic UWP apps receive them as launch arguments.
/// (ActivateForFile only works for UWP apps that declare the file type, and is slow to fail.)
fn activate_with_files(aumid: &str, files: &[String]) -> bool {
    let args: Vec<String> = files.iter().map(|f| format!("\"{f}\"")).collect();
    let result = unsafe {
        CoCreateInstance::<_, IApplicationActivationManager>(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)
            .and_then(|m| m.ActivateApplication(&HSTRING::from(aumid), &HSTRING::from(args.join(" ")), AO_NONE))
    };
    if let Err(e) = &result {
        crate::log!("couldn't open files with {aumid}: {e}");
    }
    result.is_ok()
}

/// "FileDescription" from an executable's version resource, e.g. "Microsoft Edge".
pub fn file_description(path: &str) -> Option<String> {
    unsafe {
        let p = HSTRING::from(path);
        let size = GetFileVersionInfoSizeW(&p, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(&p, None, size, data.as_mut_ptr() as *mut c_void).ok()?;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let mut langs = vec![(0x0409u16, 0x04b0u16)];
        if VerQueryValueW(data.as_ptr() as _, w!("\\VarFileInfo\\Translation"), &mut ptr, &mut len).as_bool() && len >= 4 {
            let pairs = std::slice::from_raw_parts(ptr as *const u16, (len / 2) as usize);
            langs = pairs.chunks_exact(2).map(|c| (c[0], c[1])).chain(langs).collect();
        }
        for (lang, cp) in langs {
            let key = wide(&format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\FileDescription"));
            if VerQueryValueW(data.as_ptr() as _, PCWSTR(key.as_ptr()), &mut ptr, &mut len).as_bool() && len > 1 {
                let s = from_wide(std::slice::from_raw_parts(ptr as *const u16, len as usize));
                let s = s.trim().to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
        None
    }
}

/// Display name the shell shows for a path or parsing name (e.g. a Store app in AppsFolder).
pub fn display_name(parsing_name: &str) -> Option<String> {
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(parsing_name), None).ok()?;
        let name = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
        let s = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as _));
        s.filter(|s| !s.is_empty())
    }
}

/// Friendly name for something the user pinned or that is running.
pub fn friendly_name(path: &str) -> String {
    let p = Path::new(path);
    let is_exe = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    if is_exe && let Some(d) = file_description(path) {
        return d;
    }
    if let Some(d) = display_name(path) {
        return d;
    }
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

fn reg_dword(subkey: PCWSTR, value: PCWSTR) -> Option<u32> {
    let mut data = 0u32;
    let mut size = 4u32;
    let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, subkey, value, RRF_RT_REG_DWORD, None, Some(&mut data as *mut u32 as *mut c_void), Some(&mut size)) };
    (r == ERROR_SUCCESS).then_some(data)
}

const PERSONALIZE: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");

pub fn system_dark_mode() -> bool {
    reg_dword(PERSONALIZE, w!("AppsUseLightTheme")) == Some(0)
}

pub fn transparency_enabled() -> bool {
    reg_dword(PERSONALIZE, w!("EnableTransparency")) != Some(0)
}

pub fn set_autostart(enabled: bool) {
    let run = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    unsafe {
        if enabled {
            let Ok(exe) = std::env::current_exe() else { return };
            let cmd = wide(&format!("\"{}\"", exe.display()));
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, run, w!("Windock"), REG_SZ.0, Some(cmd.as_ptr() as *const c_void), (cmd.len() * 2) as u32);
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, run, w!("Windock"));
        }
    }
}

/// Whether `hwnd` is Quick Settings' window.
pub fn is_quick_settings(hwnd: HWND) -> bool {
    let mut buf = [0u16; 32];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n]) == "ControlCenterWindow"
}

/// Opens Quick Settings (Wi-Fi, volume, battery) by pressing Win+A: unlike the notification
/// centre, it has no URI that works on current Windows 11.
pub fn open_quick_settings() {
    press_win(VK_A);
}

/// Closes the notification centre: Win+N toggles it, while its URI only ever opens it.
pub fn close_notification_center() {
    press_win(VK_N);
}

/// Opens or closes the Start menu, as the Windows key does.
pub fn toggle_start() {
    press(&[VK_LWIN]);
}

/// The Win+X menu: Device Manager, Terminal, Shut down and so on.
pub fn open_power_menu() {
    press_win(VK_X);
}

/// Minimizes every window, or brings them back, like Win+D. Windows keeps the list of what
/// to restore, so this agrees with the taskbar's own button and the keyboard shortcut.
pub fn toggle_desktop() {
    unsafe {
        match CoCreateInstance::<_, IShellDispatch4>(&Shell, None, CLSCTX_INPROC_SERVER) {
            Ok(shell) => {
                let _ = shell.ToggleDesktop();
            }
            Err(e) => crate::log!("couldn't toggle the desktop: {e}"),
        }
    }
}

fn press_win(vk: VIRTUAL_KEY) {
    press(&[VK_LWIN, vk]);
}

/// Presses the keys in order, then releases them in reverse.
fn press(vks: &[VIRTUAL_KEY]) {
    let key = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() } },
    };
    let keys: Vec<INPUT> = vks.iter().map(|&vk| key(vk, false)).chain(vks.iter().rev().map(|&vk| key(vk, true))).collect();
    unsafe {
        SendInput(&keys, size_of::<INPUT>() as i32);
    }
}
