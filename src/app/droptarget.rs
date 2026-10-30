//! Files dragged onto the dock. Dropped on an app, they open with it; dropped between apps
//! (or if they're programs and shortcuts themselves), they're pinned. While dragging, the
//! dock magnifies under the cursor and says which of the two will happen.

use super::*;
use windows::Win32::Foundation::POINTL;
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::core::{Ref, implement};

/// Files currently being dragged over the dock.
pub(super) struct Dragging {
    files: Vec<String>,
    /// Programs and shortcuts get pinned even when dropped on an app.
    apps_only: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum DropAction {
    OpenWith(usize),
    /// Insert into the pinned list at this index.
    Pin(usize),
    Nothing,
}

#[implement(IDropTarget)]
struct DropTarget;

fn set_effect(out: *mut DROPEFFECT, effect: DROPEFFECT) {
    if !out.is_null() {
        unsafe { *out = effect };
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(&self, data: Ref<IDataObject>, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let files = data.as_ref().map(dropped_files).unwrap_or_default();
        set_effect(effect, with_app(|a| a.drag_enter(files, *pt)).unwrap_or(DROPEFFECT_NONE));
        Ok(())
    }

    fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        set_effect(effect, with_app(|a| a.drag_over(*pt)).unwrap_or(DROPEFFECT_NONE));
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        with_app(|a| a.drag_leave());
        Ok(())
    }

    fn Drop(&self, data: Ref<IDataObject>, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let files = data.as_ref().map(dropped_files).unwrap_or_default();
        set_effect(effect, with_app(|a| a.drag_drop(files, *pt)).unwrap_or(DROPEFFECT_NONE));
        Ok(())
    }
}

/// Paths from a drag of files (CF_HDROP), e.g. from Explorer.
fn dropped_files(data: &IDataObject) -> Vec<String> {
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut paths = Vec::new();
    unsafe {
        let Ok(mut medium) = data.GetData(&format) else { return paths };
        let hdrop = HDROP(medium.u.hGlobal.0);
        for i in 0..DragQueryFileW(hdrop, u32::MAX, None) {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            paths.push(from_wide(&buf));
        }
        ReleaseStgMedium(&mut medium);
    }
    paths
}

fn is_app_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|e| ["exe", "lnk", "url", "appref-ms", "bat", "cmd"].iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// Needs OLE initialised on this thread.
pub(super) fn register(hwnd: HWND) {
    let target: IDropTarget = DropTarget.into();
    if let Err(e) = unsafe { RegisterDragDrop(hwnd, &target) } {
        crate::log!("can't accept dropped files: {e}");
    }
}

pub(super) fn unregister(hwnd: HWND) {
    unsafe {
        let _ = RevokeDragDrop(hwnd);
    }
}

impl App {
    fn drop_point(&self, pt: POINTL) -> (f32, f32) {
        let mut p = POINT { x: pt.x, y: pt.y };
        unsafe {
            let _ = ScreenToClient(self.hwnd, &mut p);
        }
        (p.x as f32, p.y as f32)
    }

    fn drag_enter(&mut self, files: Vec<String>, pt: POINTL) -> DROPEFFECT {
        if files.is_empty() {
            return DROPEFFECT_NONE;
        }
        let apps_only = files.iter().all(|f| is_app_file(f));
        self.dragging = Some(Dragging { files, apps_only });
        self.close_preview();
        self.drag_over(pt)
    }

    fn drag_over(&mut self, pt: POINTL) -> DROPEFFECT {
        self.cursor = Some(self.drop_point(pt));
        self.kill_timer(TIMER_HIDE);
        self.hide_target = 0.0;
        self.dirty = true;
        match self.drop_action() {
            DropAction::OpenWith(_) => DROPEFFECT_COPY,
            DropAction::Pin(_) => DROPEFFECT_LINK,
            DropAction::Nothing => DROPEFFECT_NONE,
        }
    }

    fn drag_leave(&mut self) {
        self.dragging = None;
        self.cursor = None;
        self.schedule_hide(self.cfg.dock.autohide_delay_ms);
        self.dirty = true;
    }

    /// Decides what the drop does, but carries it out after returning: opening an app or
    /// writing the config inside the drop would keep the app the files came from waiting.
    fn drag_drop(&mut self, files: Vec<String>, pt: POINTL) -> DROPEFFECT {
        self.cursor = Some(self.drop_point(pt));
        let action = self.drop_action();
        let files = if files.is_empty() { self.dragging.take().map(|d| d.files).unwrap_or_default() } else { files };
        self.dragging = None;
        self.dirty = true;
        if action == DropAction::Nothing || files.is_empty() {
            return DROPEFFECT_NONE;
        }
        self.dropped = Some((action, files));
        post(WM_APP_DROP, 0);
        if matches!(action, DropAction::OpenWith(_)) { DROPEFFECT_COPY } else { DROPEFFECT_LINK }
    }

    /// Runs the drop recorded by `drag_drop`.
    pub(super) fn finish_drop(&mut self) {
        match self.dropped.take() {
            Some((DropAction::OpenWith(i), files)) => self.open_files_with(i, &files),
            Some((DropAction::Pin(at), files)) => self.pin_files(at, files),
            _ => {}
        }
    }

    /// What dropping right now would do.
    pub(super) fn drop_action(&self) -> DropAction {
        let Some(d) = &self.dragging else { return DropAction::Nothing };
        let Some((u, _)) = self.cursor_uv().filter(|_| self.hovering()) else { return DropAction::Nothing };
        if !d.apps_only
            && let Some(i) = self.hovered_index()
            && self.items[i].launch.is_some()
            && (u - self.layout.slots[i].center).abs() <= self.layout.slots[i].size * 0.4
        {
            return DropAction::OpenWith(i);
        }
        let at = self
            .items
            .iter()
            .zip(&self.layout.slots)
            .filter(|(it, s)| matches!(it.kind, Kind::Pinned(_)) && !it.dying && s.center < u)
            .count();
        DropAction::Pin(at)
    }

    /// The label shown while dragging files over the dock: (text, centre along the dock, top).
    pub(super) fn drop_label(&self) -> Option<(String, f32, f32)> {
        match self.drop_action() {
            DropAction::OpenWith(i) => self.icon_uv(i).map(|(u0, u1, _, v1)| (format!("Open with {}", self.items[i].name), (u0 + u1) / 2.0, v1)),
            DropAction::Pin(_) => self.cursor_uv().map(|(u, _)| ("Keep in dock".to_string(), u, self.m.margin + self.m.thick)),
            DropAction::Nothing => None,
        }
    }

    fn open_files_with(&mut self, i: usize, files: &[String]) {
        let Some(l) = self.items[i].launch.clone() else { return };
        let files = files.to_vec();
        self.start_app(i, 1.0, move || shell::open_with(&l.path, l.args.as_deref(), l.cwd.as_deref(), &files));
        self.dirty = true;
    }

    /// Pins paths at `at` in the pinned list, skipping ones already pinned.
    pub(super) fn pin_files(&mut self, mut at: usize, paths: Vec<String>) {
        let mut added = false;
        for path in paths {
            if self.cfg.pinned.iter().any(|p| p.path.eq_ignore_ascii_case(&path)) {
                continue;
            }
            self.cfg.pinned.insert(at.min(self.cfg.pinned.len()), Pinned { path, ..Default::default() });
            at += 1;
            added = true;
        }
        if added {
            self.save_pins();
            self.rebuild_items();
        }
    }
}
