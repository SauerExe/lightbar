//! Einblend-Animation für Popups (Startmenü, Schnelleinstellungen, Vorschau,
//! Überlauf, Tooltips): von der Leiste weg ein Stück gleiten (unten: von
//! leicht unterhalb hoch, seitlich: von der Seite herein) und dabei einblenden.
//!
//! Die Popups sind Layered-Fenster; animiert wird nur deren Gesamt-Alpha und
//! Position – der Inhalt wird nicht neu gezeichnet. Ein Thread-Timer ohne
//! Fenster treibt alle laufenden Animationen und stoppt, sobald keine mehr läuft.

use std::cell::{Cell, RefCell};
use std::ptr::null_mut;
use std::time::Instant;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const DURATION: f32 = 0.18;

struct Pop {
    hwnd: HWND,
    start: Instant,
    x: i32,
    y: i32,
    /// Startversatz (dx, dy) zur Zielposition.
    offset: (i32, i32),
}

thread_local! {
    static POPS: RefCell<Vec<Pop>> = const { RefCell::new(Vec::new()) };
    static TIMER: Cell<usize> = const { Cell::new(0) };
}

/// Weiches Abbremsen am Ende (kubisch).
pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// Fenster muss mit WS_EX_LAYERED erzeugt worden sein.
pub unsafe fn make_opaque(hwnd: HWND) {
    SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
}

/// Popup an (x, y) mit Größe (w, h) zeigen und vom Versatz `offset` (dx, dy,
/// siehe `display::pop_offset`) an seinen Platz gleiten lassen. Aktiviert
/// nicht – Fokus setzt der Aufrufer.
pub unsafe fn pop_in(hwnd: HWND, x: i32, y: i32, w: i32, h: i32, offset: (i32, i32)) {
    SetLayeredWindowAttributes(hwnd, 0, 0, LWA_ALPHA);
    SetWindowPos(hwnd, HWND_TOPMOST, x + offset.0, y + offset.1, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    POPS.with(|p| {
        let mut p = p.borrow_mut();
        p.retain(|q| q.hwnd != hwnd);
        p.push(Pop { hwnd, start: Instant::now(), x, y, offset });
    });
    if TIMER.with(Cell::get) == 0 {
        TIMER.with(|t| t.set(SetTimer(null_mut(), 0, 15, Some(tick))));
    }
}

unsafe extern "system" fn tick(_: HWND, _: u32, _: usize, _: u32) {
    // Erst berechnen, dann außerhalb des Borrows anwenden: SetWindowPos
    // schickt Nachrichten an die Popups.
    let frames: Vec<(HWND, i32, i32, u8)> = POPS.with(|p| {
        let mut p = p.borrow_mut();
        let frames = p
            .iter()
            .map(|q| {
                let e = ease_out(q.start.elapsed().as_secs_f32() / DURATION);
                let rest = |o: i32| ((1.0 - e) * o as f32).round() as i32;
                (q.hwnd, q.x + rest(q.offset.0), q.y + rest(q.offset.1), (e * 255.0) as u8)
            })
            .collect();
        p.retain(|q| q.start.elapsed().as_secs_f32() < DURATION);
        frames
    });
    for (hwnd, x, y, alpha) in frames {
        SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA);
        SetWindowPos(hwnd, null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
    if POPS.with(|p| p.borrow().is_empty()) {
        let id = TIMER.with(|t| t.replace(0));
        if id != 0 {
            KillTimer(null_mut(), id);
        }
    }
}
