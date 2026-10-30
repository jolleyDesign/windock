//! The Windows taskbar: reading its pinned apps, in taskbar order, and hiding it while the
//! dock runs.
//!
//! Pinned desktop apps are shortcuts in `User Pinned\TaskBar`. Pinned Store apps have no
//! shortcut and only appear (as AppUserModelIDs) in the undocumented `Taskband\Favorites`
//! blob, which also holds the order. If the blob can't be read, shortcuts come back sorted
//! by name.

use std::ffi::c_void;
use std::path::PathBuf;
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RegGetValueW};
use windows::Win32::UI::Shell::{ABM_GETSTATE, ABM_SETSTATE, ABS_AUTOHIDE, APPBARDATA, SHAppBarMessage};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW, GetClassNameW, IsWindowVisible, SW_HIDE, SW_SHOWNA, ShowWindow};
use windows::core::w;

pub enum Pin {
    Shortcut(PathBuf),
    /// A Store app's AppUserModelID.
    App(String),
}

pub fn pinned() -> Vec<Pin> {
    let blob = favorites_blob().unwrap_or_default();
    let mut found: Vec<(Option<usize>, String, Pin)> = Vec::new();

    let dir = PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default())
        .join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar");
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
            continue;
        }
        let file = entry.file_name().to_string_lossy().into_owned();
        let pos = find_utf16(&blob, &file);
        found.push((pos, file.to_lowercase(), Pin::Shortcut(path)));
    }
    for (pos, aumid) in store_app_ids(&blob) {
        found.push((Some(pos), aumid.to_lowercase(), Pin::App(aumid)));
    }
    // Blob order first; anything the blob doesn't mention goes last, by name.
    found.sort_by(|a, b| match (a.0, b.0) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.cmp(&b.1),
    });
    found.into_iter().map(|(_, _, pin)| pin).collect()
}

fn favorites_blob() -> Option<Vec<u8>> {
    let key = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Taskband");
    let value = w!("Favorites");
    unsafe {
        let mut size = 0u32;
        if RegGetValueW(HKEY_CURRENT_USER, key, value, RRF_RT_REG_BINARY, None, None, Some(&mut size)) != ERROR_SUCCESS {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            value,
            RRF_RT_REG_BINARY,
            None,
            Some(data.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        );
        (r == ERROR_SUCCESS).then(|| {
            data.truncate(size as usize);
            data
        })
    }
}

/// ASCII-lowercases UTF-16LE text so searches ignore case.
fn lower_utf16(bytes: &[u8]) -> Vec<u8> {
    bytes
        .chunks_exact(2)
        .flat_map(|c| {
            let unit = u16::from_le_bytes([c[0], c[1]]);
            let lower = if unit < 128 { (unit as u8).to_ascii_lowercase() as u16 } else { unit };
            lower.to_le_bytes()
        })
        .collect()
}

/// Byte offset of `text` (UTF-16LE, ASCII case-insensitive) in the blob.
fn find_utf16(blob: &[u8], text: &str) -> Option<usize> {
    let needle = lower_utf16(&text.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>());
    if needle.is_empty() {
        return None;
    }
    // Strings in shell item lists aren't always 2-byte aligned, so try both alignments.
    (0..2)
        .filter_map(|align| {
            let hay = lower_utf16(blob.get(align..)?);
            hay.windows(needle.len()).step_by(2).position(|w| w == needle).map(|i| align + i * 2)
        })
        .min()
}

/// Store app IDs (`Name_publisherid!App`) embedded in the blob, with their byte offsets.
fn store_app_ids(blob: &[u8]) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    for align in 0..2 {
        let units: Vec<(usize, u16)> = blob
            .get(align..)
            .unwrap_or_default()
            .chunks_exact(2)
            .enumerate()
            .map(|(i, c)| (align + i * 2, u16::from_le_bytes([c[0], c[1]])))
            .collect();
        let mut token = String::new();
        let mut start = 0;
        for (pos, unit) in units.iter().copied().chain(std::iter::once((usize::MAX, 0))) {
            let c = char::from_u32(unit as u32).filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '!'));
            match c {
                Some(c) => {
                    if token.is_empty() {
                        start = pos;
                    }
                    token.push(c);
                }
                None => {
                    if is_store_app_id(&token) && !out.iter().any(|(_, t)| t.eq_ignore_ascii_case(&token)) {
                        out.push((start, std::mem::take(&mut token)));
                    }
                    token.clear();
                }
            }
        }
    }
    out.sort_by_key(|(pos, _)| *pos);
    out
}

/// `PackageName_publisherhash13!AppId`
fn is_store_app_id(s: &str) -> bool {
    let Some((family, app)) = s.split_once('!') else { return false };
    let Some((name, publisher)) = family.rsplit_once('_') else { return false };
    !name.is_empty()
        && publisher.len() == 13
        && publisher.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && app.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && !app.contains('!')
}

// ---------------------------------------------------------------- hiding

/// Holds the taskbar's own auto-hide setting while it's hidden. It's a file so that if the
/// dock is killed, the next start can still put things back.
fn hidden_marker() -> PathBuf {
    crate::util::data_dir().join("taskbar-hidden")
}

/// The main taskbar and the one on each other monitor.
fn taskbar_windows() -> Vec<HWND> {
    let mut found = Vec::new();
    unsafe {
        if let Ok(h) = FindWindowW(w!("Shell_TrayWnd"), None) {
            found.push(h);
        }
        let mut after = None;
        while let Ok(h) = FindWindowExW(None, after, w!("Shell_SecondaryTrayWnd"), None) {
            found.push(h);
            after = Some(h);
        }
    }
    found
}

pub fn is_taskbar(hwnd: HWND) -> bool {
    let mut buf = [0u16; 32];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    matches!(String::from_utf16_lossy(&buf[..n]).as_str(), "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
}

fn appbar_state(set: Option<u32>) -> u32 {
    let Some(tray) = taskbar_windows().first().copied() else { return 0 };
    let mut abd = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, hWnd: tray, ..Default::default() };
    unsafe {
        match set {
            Some(state) => {
                abd.lParam = LPARAM(state as isize);
                SHAppBarMessage(ABM_SETSTATE, &mut abd) as u32
            }
            None => SHAppBarMessage(ABM_GETSTATE, &mut abd) as u32,
        }
    }
}

/// Hides the taskbar on every monitor. It's switched to auto-hide first: a hidden window
/// still reserves its space, an auto-hiding one doesn't. Safe to call repeatedly.
pub fn hide() {
    let marker = hidden_marker();
    if !marker.exists() {
        let state = appbar_state(None);
        if let Err(e) = std::fs::write(&marker, state.to_string()) {
            crate::log!("couldn't save the taskbar's state, leaving it alone: {e}");
            return;
        }
        if state & ABS_AUTOHIDE == 0 {
            appbar_state(Some(state | ABS_AUTOHIDE));
        }
    }
    for h in taskbar_windows() {
        if unsafe { IsWindowVisible(h) }.as_bool() {
            unsafe {
                let _ = ShowWindow(h, SW_HIDE);
            }
        }
    }
}

/// Undoes `hide`, including one from a previous run that didn't exit cleanly.
pub fn restore() {
    let marker = hidden_marker();
    let Ok(text) = std::fs::read_to_string(&marker) else { return };
    for h in taskbar_windows() {
        unsafe {
            let _ = ShowWindow(h, SW_SHOWNA);
        }
    }
    if let Ok(state) = text.trim().parse::<u32>() {
        appbar_state(Some(state));
    }
    let _ = std::fs::remove_file(&marker);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn recognises_store_app_ids() {
        assert!(is_store_app_id("Microsoft.WindowsTerminal_8wekyb3d8bbwe!App"));
        assert!(!is_store_app_id("Microsoft.WindowsTerminal_8wekyb3d8bbwe"));
        assert!(!is_store_app_id("chrome.exe!App"));
        assert!(!is_store_app_id("Name_SHORT!App"));
    }

    #[test]
    fn finds_names_at_either_alignment_ignoring_case() {
        let mut blob = vec![0u8, 1, 2];
        blob.extend(utf16("x\0Steam.lnk"));
        assert_eq!(find_utf16(&blob, "steam.LNK"), Some(3 + 4));
        let mut blob = vec![0u8, 1];
        blob.extend(utf16("Steam.lnk"));
        assert_eq!(find_utf16(&blob, "Steam.lnk"), Some(2));
        assert_eq!(find_utf16(&blob, "Discord.lnk"), None);
    }

    #[test]
    fn extracts_store_app_ids_in_order() {
        let mut blob = vec![9u8];
        blob.extend(utf16("\0Microsoft.WindowsTerminal_8wekyb3d8bbwe!App\0junk\0"));
        blob.push(0);
        blob.extend(utf16("Microsoft.Paint_8wekyb3d8bbwe!Microsoft.Paint\0"));
        let ids: Vec<String> = store_app_ids(&blob).into_iter().map(|(_, id)| id).collect();
        assert_eq!(ids, ["Microsoft.WindowsTerminal_8wekyb3d8bbwe!App", "Microsoft.Paint_8wekyb3d8bbwe!Microsoft.Paint"]);
    }

    /// Prints this machine's taskbar pins: `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn show_this_machines_pins() {
        for pin in pinned() {
            match pin {
                Pin::Shortcut(p) => println!("shortcut  {}", p.display()),
                Pin::App(id) => println!("store app {id}"),
            }
        }
    }
}
