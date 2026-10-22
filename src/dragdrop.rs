//! Dateien auf App-Buttons ziehen – wie bei der Windows-Leiste:
//!
//! - Verweilt der Zeiger mit gezogenen Dateien etwa 500 ms über dem Button
//!   einer laufenden App, kommt deren Fenster nach vorne (dort lässt man dann
//!   ab).
//! - Auf eine angeheftete, nicht laufende App abgelegt, öffnet sie die Dateien.
//! - Sonst nimmt die Leiste nichts an (DROPEFFECT_NONE).
//!
//! Das Ablageziel (IDropTarget) hängt per RegisterDragDrop an jeder Leiste;
//! OLE muss dafür auf dem Leisten-Thread initialisiert sein.

use std::cell::{Cell, RefCell};
use std::time::Instant;

use windows::core::{implement, Result};
use windows::Win32::Foundation::{HWND as WHWND, POINTL};
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::Ole::*;
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

use super::*;

/// Zeitgeber der Leiste für "Verweilen über einem Button".
const TIMER_DRAG_HOVER: usize = 21;
const HOVER_MS: u32 = 500;

thread_local! {
    /// Button (Platz-Schlüssel) unter dem Zeiger beim Ziehen, mit Leiste und Beginn.
    static HOVER: RefCell<Option<(isize, String, Instant)>> = const { RefCell::new(None) };
}

#[implement(IDropTarget)]
struct Target {
    bar: isize,
    /// Bringt das Gezogene Dateien mit (CF_HDROP)?
    files: Cell<bool>,
}

fn hdrop_format() -> FORMATETC {
    FORMATETC { cfFormat: CF_HDROP.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 }
}

/// Gruppe unter einem Bildschirmpunkt.
unsafe fn group_at(bar: HWND, pt: &POINTL) -> Option<Group> {
    let mut p = POINT { x: pt.x, y: pt.y };
    ScreenToClient(bar, &mut p);
    let Hit::Task(i) = hit_at(bar, p.x, p.y) else { return None };
    with(|s| s.groups.get(i).cloned()).flatten()
}

/// Ablegen erlaubt: Dateien auf eine angeheftete App, die nicht läuft.
fn accepts(g: &Group, files: bool) -> bool {
    files && g.windows.is_empty() && with(|s| s.pins.contains(&g.slot)).unwrap_or(false)
}

impl Target_Impl {
    unsafe fn over(&self, pt: &POINTL, effect: *mut DROPEFFECT) {
        let bar = self.bar as HWND;
        let g = group_at(bar, pt);
        // Verweilen über einer laufenden App: nach HOVER_MS deren Fenster holen.
        match g.as_ref().filter(|g| !g.windows.is_empty()) {
            Some(g) => {
                let same = HOVER.with(|h| h.borrow().as_ref().is_some_and(|(b, slot, _)| *b == self.bar && *slot == g.slot));
                if !same {
                    HOVER.with(|h| *h.borrow_mut() = Some((self.bar, g.slot.clone(), Instant::now())));
                    SetTimer(bar, TIMER_DRAG_HOVER, HOVER_MS, None);
                }
            }
            None => end_hover(),
        }
        if !effect.is_null() {
            *effect = if g.is_some_and(|g| accepts(&g, self.files.get())) { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
        }
    }
}

impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(&self, data: Option<&IDataObject>, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        unsafe {
            self.files.set(data.is_some_and(|d| d.QueryGetData(&hdrop_format()).is_ok()));
            self.over(pt, effect);
        }
        Ok(())
    }

    fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        unsafe { self.over(pt, effect) };
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        unsafe { end_hover() };
        Ok(())
    }

    fn Drop(&self, data: Option<&IDataObject>, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        unsafe {
            end_hover();
            let target = group_at(self.bar as HWND, pt).filter(|g| accepts(g, self.files.get()));
            let files = match (&target, data) {
                (Some(_), Some(d)) => dropped_files(d),
                _ => Vec::new(),
            };
            let ok = match target {
                Some(g) if !files.is_empty() => {
                    launch_with(&g, false, &files);
                    true
                }
                _ => false,
            };
            if !effect.is_null() {
                *effect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
            }
        }
        Ok(())
    }
}

/// Pfade aus CF_HDROP.
unsafe fn dropped_files(data: &IDataObject) -> Vec<String> {
    let Ok(mut medium) = data.GetData(&hdrop_format()) else { return Vec::new() };
    let drop = HDROP(medium.u.hGlobal.0);
    let n = DragQueryFileW(drop, u32::MAX, None);
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let len = DragQueryFileW(drop, i, None) as usize;
        let mut buf = vec![0u16; len + 1];
        let got = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
        if got > 0 {
            out.push(String::from_utf16_lossy(&buf[..got]));
        }
    }
    ReleaseStgMedium(&mut medium);
    out
}

unsafe fn end_hover() {
    if let Some((bar, _, _)) = HOVER.with(|h| h.borrow_mut().take()) {
        KillTimer(bar as HWND, TIMER_DRAG_HOVER);
    }
}

/// TIMER_DRAG_HOVER der Leiste: Liegt der Zeiger noch über demselben Button,
/// dessen oberstes Fenster nach vorne holen. `true`, wenn der Zeitgeber uns gehört.
pub(super) unsafe fn on_timer(bar: HWND, id: usize) -> bool {
    if id != TIMER_DRAG_HOVER {
        return false;
    }
    KillTimer(bar, TIMER_DRAG_HOVER);
    let Some((b, slot, _)) = HOVER.with(|h| h.borrow().clone()) else { return true };
    if b != bar as isize {
        return true;
    }
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    let still = group_at(bar, &POINTL { x: pt.x, y: pt.y }).filter(|g| g.slot == slot);
    if let Some(g) = still {
        if let Some(&h) = by_z_order(&g.windows).first() {
            activate(h);
        }
    }
    true
}

/// Leiste als Ablageziel anmelden (OLE auf diesem Thread bei Bedarf starten).
pub(super) unsafe fn register(bar: HWND) {
    thread_local! {
        static OLE: Cell<bool> = const { Cell::new(false) };
    }
    if !OLE.with(|o| o.replace(true)) {
        if let Err(e) = OleInitialize(None) {
            log(&format!("OLE nicht initialisiert, kein Ablegen auf App-Buttons: {e:?}"));
            return;
        }
    }
    let target: IDropTarget = Target { bar: bar as isize, files: Cell::new(false) }.into();
    if let Err(e) = RegisterDragDrop(WHWND(bar as _), &target) {
        log(&format!("RegisterDragDrop fehlgeschlagen: {e:?}"));
    }
}

pub(super) unsafe fn revoke(bar: HWND) {
    end_hover();
    let _ = RevokeDragDrop(WHWND(bar as _));
}
