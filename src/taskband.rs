//! Taskband: empfängt, was Apps über `ITaskbarList3` an die Taskleiste
//! melden – Fortschritt und Zähler-Symbole (z. B. ungelesene Mails).
//!
//! So findet das (in der App laufende) TaskbarList-Objekt die Taskleiste
//! (ermittelt per Ghidra in explorerframe.dll, Windows 11 Build 26300):
//! `FindWindow("Shell_TrayWnd")`, dann `GetProp(tray, "TaskbandHWND")`. An
//! dieses Fenster gehen einfache Nachrichten:
//!
//! | Nachricht | Bedeutung | wParam | lParam |
//! |---|---|---|---|
//! | 0x440 | SetProgressValue | App-Fenster | 0 … 0xFFFE |
//! | 0x441 | SetProgressState | App-Fenster | TBPFLAG |
//! | 0x44F | SetOverlayIcon (synchron) | App-Fenster | HICON oder 0 |
//! | 0x455 | Beschreibung zum Symbol | App-Fenster | Shared Memory |
//!
//! Das Symbol gibt der Absender nach der Nachricht frei – deshalb sofort
//! kopieren. Viele Apps melden sich erst nach `TaskbarButtonCreated`; das
//! schickt die Leiste an neue Fenster (`announce`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::InvalidateRect;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::util::{log, register_class, w16};

const MSG_PROGRESS_VALUE: u32 = 0x440;
const MSG_PROGRESS_STATE: u32 = 0x441;
const MSG_OVERLAY: u32 = 0x44F;
/// Höchster Wert der Fortschrittsskala.
pub const PROGRESS_MAX: u32 = 0xFFFE;

/// TBPFLAG-Werte.
pub const TBPF_INDETERMINATE: u32 = 1;
pub const TBPF_ERROR: u32 = 4;
pub const TBPF_PAUSED: u32 = 8;

/// Was eine App für eines ihrer Fenster gemeldet hat.
#[derive(Default)]
struct Badge {
    /// Eigene Kopie des Zähler-Symbols.
    overlay: HICON,
    state: u32,
    value: u32,
}

/// Fortschritt eines Buttons: Status (TBPF_*) und Wert 0 … PROGRESS_MAX.
#[derive(Clone, Copy)]
pub struct Progress {
    pub state: u32,
    pub value: u32,
}

const TIMER_SPIN: usize = 1;

thread_local! {
    static BADGES: RefCell<HashMap<isize, Badge>> = RefCell::new(HashMap::new());
    /// Fenster, denen schon "TaskbarButtonCreated" geschickt wurde.
    static ANNOUNCED: RefCell<HashSet<isize>> = RefCell::new(HashSet::new());
}

static BUTTON_CREATED: AtomicU32 = AtomicU32::new(0);

unsafe fn redraw() {
    for b in crate::bar::all_bars() {
        InvalidateRect(b, null(), 0);
    }
}

unsafe fn update(target: HWND, f: impl FnOnce(&mut Badge)) {
    BADGES.with(|b| {
        let mut b = b.borrow_mut();
        let e = b.entry(target as isize).or_default();
        f(e);
        if e.overlay.is_null() && e.state == 0 {
            b.remove(&(target as isize));
        }
    });
    redraw();
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let target = wp as HWND;
    match msg {
        MSG_PROGRESS_VALUE => {
            update(target, |e| e.value = (lp as u32).min(PROGRESS_MAX));
            0
        }
        MSG_PROGRESS_STATE => {
            update(target, |e| e.state = lp as u32);
            // Unbestimmter Fortschritt läuft als Animation.
            let spinning = BADGES.with(|b| b.borrow().values().any(|e| e.state & TBPF_INDETERMINATE != 0));
            if spinning {
                SetTimer(hwnd, TIMER_SPIN, 50, None);
            } else {
                KillTimer(hwnd, TIMER_SPIN);
            }
            0
        }
        MSG_OVERLAY => {
            // Der Absender gibt sein Symbol gleich nach der Nachricht frei.
            let copy = if lp == 0 { null_mut() } else { CopyIcon(lp as HICON) };
            update(target, |e| {
                if !e.overlay.is_null() {
                    DestroyIcon(e.overlay);
                }
                e.overlay = copy;
            });
            0
        }
        WM_TIMER if wp == TIMER_SPIN => {
            redraw();
            0
        }
        // Übrige TaskbarList-Nachrichten (Vorschau-Buttons, Tooltips, Tabs …)
        // annehmen, aber noch nicht umsetzen.
        0x432..=0x455 => 0,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Taskband-Fenster unter der Hauptleiste anlegen und als `TaskbandHWND`
/// bekannt machen.
pub unsafe fn create(inst: HINSTANCE, bar: HWND) {
    BUTTON_CREATED.store(RegisterWindowMessageW(w16("TaskbarButtonCreated").as_ptr()), Ordering::Relaxed);
    let class = w16("MSTaskSwWClass");
    register_class(inst, &class, Some(wndproc), 0);
    let band = CreateWindowExW(0, class.as_ptr(), null(), WS_CHILD, 0, 0, 0, 0, bar, null_mut(), inst, null());
    if band.is_null() {
        log("Taskband konnte nicht angelegt werden");
        return;
    }
    SetPropW(bar, w16("TaskbandHWND").as_ptr(), band as HANDLE);
}

/// Neuen Fenstern einmal "TaskbarButtonCreated" schicken – erst danach melden
/// viele Apps Fortschritt und Zähler. Fenster, die es nicht mehr gibt, samt
/// ihren Meldungen vergessen.
pub unsafe fn announce(windows: &[HWND]) {
    let msg = BUTTON_CREATED.load(Ordering::Relaxed);
    ANNOUNCED.with(|a| {
        let mut a = a.borrow_mut();
        a.retain(|&h| IsWindow(h as HWND) != 0);
        for &w in windows {
            if msg != 0 && a.insert(w as isize) {
                PostMessageW(w, msg, 0, 0);
            }
        }
    });
    BADGES.with(|b| {
        b.borrow_mut().retain(|&h, e| {
            let alive = IsWindow(h as HWND) != 0;
            if !alive && !e.overlay.is_null() {
                DestroyIcon(e.overlay);
            }
            alive
        })
    });
}

/// Zähler-Symbol und Fortschritt für einen Button (seine Fenster): das zuletzt
/// gemeldete Symbol eines der Fenster, beim Fortschritt Fehler vor Pause vor
/// normal.
pub fn badge(windows: &[HWND]) -> (HICON, Option<Progress>) {
    BADGES.with(|b| {
        let b = b.borrow();
        let mut overlay = null_mut();
        let mut progress: Option<Progress> = None;
        let rank = |s: u32| if s & TBPF_ERROR != 0 { 3 } else if s & TBPF_PAUSED != 0 { 2 } else { 1 };
        for w in windows {
            let Some(e) = b.get(&(*w as isize)) else { continue };
            if !e.overlay.is_null() {
                overlay = e.overlay;
            }
            if e.state != 0 && progress.map_or(true, |p| rank(e.state) > rank(p.state)) {
                progress = Some(Progress { state: e.state, value: e.value });
            }
        }
        (overlay, progress)
    })
}

pub unsafe fn self_test() {
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList, TBPF_NOPROGRESS, TBPF_NORMAL};
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let inst = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null());
    let class = w16("LightbarTaskbarListTest");
    unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        DefWindowProcW(h, m, w, l)
    }
    register_class(inst, &class, Some(proc_), 0);
    let hwnd = CreateWindowExW(0, class.as_ptr(), w16("Lightbar-Test").as_ptr(), WS_OVERLAPPEDWINDOW, 100, 100, 300, 200, null_mut(), null_mut(), inst, null());
    ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
    let pump = |ms: u64| {
        let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        let mut msg: MSG = std::mem::zeroed();
        while std::time::Instant::now() < end {
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == TASKBAR_BUTTON_CREATED.load(Ordering::Relaxed) {
                    log("Test: TaskbarButtonCreated empfangen");
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    TASKBAR_BUTTON_CREATED.store(RegisterWindowMessageW(w16("TaskbarButtonCreated").as_ptr()), Ordering::Relaxed);
    pump(800);
    let h = windows::Win32::Foundation::HWND(hwnd);
    match CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER) {
        Ok(t) => {
            log(&format!("Test: HrInit {:?}", t.HrInit()));
            log(&format!("Test: SetProgressState {:?}", t.SetProgressState(h, TBPF_NORMAL)));
            log(&format!("Test: SetProgressValue {:?}", t.SetProgressValue(h, 30, 100)));
            let icon = windows::Win32::UI::WindowsAndMessaging::HICON(LoadIconW(null_mut(), IDI_INFORMATION) as _);
            log(&format!("Test: SetOverlayIcon {:?}", t.SetOverlayIcon(h, icon, windows::core::w!("Test"))));
            pump(800);
            let _ = t.SetProgressState(h, TBPF_NOPROGRESS);
            let _ = t.SetOverlayIcon(h, windows::Win32::UI::WindowsAndMessaging::HICON::default(), None);
        }
        Err(e) => log(&format!("Test: TaskbarList nicht erzeugt: {e:?}")),
    }
    pump(300);
    DestroyWindow(hwnd);
}

static TASKBAR_BUTTON_CREATED: AtomicU32 = AtomicU32::new(0);
