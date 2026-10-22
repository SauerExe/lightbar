//! Bildschirme und die Taskleisten-Einstellungen von Windows.
//!
//! Lightbar übernimmt, was in Windows eingestellt ist: Ausrichtung
//! (links/mittig, `TaskbarAl`) und "Taskleiste auf allen Bildschirmen anzeigen"
//! (`MMTaskbarEnabled`). Welche Zweitbildschirme eine Leiste bekommen, lässt
//! sich zusätzlich pro Bildschirm festlegen; das merkt sich `monitors.txt`
//! über die Geräte-ID des Monitors (bleibt bei Umstecken und neuer
//! Nummerierung gleich).

use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{EDD_GET_DEVICE_INTERFACE_NAME, MONITORINFOF_PRIMARY};

use crate::util::w16;

const ADVANCED: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced";

fn reg_dword(name: &str) -> Option<u32> {
    unsafe {
        let mut value = 0u32;
        let mut size = size_of::<u32>() as u32;
        let ok = RegGetValueW(
            HKEY_CURRENT_USER,
            w16(ADVANCED).as_ptr(),
            w16(name).as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut value as *mut u32 as *mut _,
            &mut size,
        );
        (ok == ERROR_SUCCESS).then_some(value)
    }
}

/// 0 = noch nicht gelesen, 1 = mittig, 2 = links.
static ALIGN: AtomicU8 = AtomicU8::new(0);

/// Windows-Einstellung "Taskleistenausrichtung": links statt mittig.
/// Fehlt der Wert, gilt der Windows-11-Standard (mittig). Zwischengespeichert,
/// weil das Layout bei jeder Mausbewegung berechnet wird; `reload` liest neu.
pub fn align_left() -> bool {
    if ALIGN.load(Ordering::Relaxed) == 0 {
        reload();
    }
    ALIGN.load(Ordering::Relaxed) == 2
}

/// Bildschirmkante der Taskleiste.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Left,
    Top,
    Right,
    Bottom,
}

impl Edge {
    /// Links oder rechts: Die Leiste ist eine Spalte.
    pub fn vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }
    /// Wert für APPBARDATA::uEdge (ABE_LEFT … ABE_BOTTOM).
    pub fn abe(self) -> u32 {
        self as u32
    }
    fn from(v: u32) -> Option<Edge> {
        [Edge::Left, Edge::Top, Edge::Right, Edge::Bottom].get(v as usize).copied()
    }
}

/// 0 = noch nicht gelesen, sonst Kante + 1.
static EDGE: AtomicU8 = AtomicU8::new(0);

/// Position der Taskleiste aus den Windows-Einstellungen ("Taskleistenposition
/// auf dem Bildschirm", `TaskbarLocation` – 0 links, 1 oben, 2 rechts,
/// 3 unten). Ältere Windows-Versionen speichern sie nur in `StuckRects3`
/// (Byte 12, gleiche Werte); fehlt beides: unten.
pub fn edge() -> Edge {
    if EDGE.load(Ordering::Relaxed) == 0 {
        reload();
    }
    Edge::from(EDGE.load(Ordering::Relaxed) as u32 - 1).unwrap_or(Edge::Bottom)
}

/// Byte `i` aus `StuckRects3\Settings` (Explorers Taskleisten-Zustand).
fn stuck_rects_byte(i: usize) -> Option<u8> {
    unsafe {
        let mut buf = [0u8; 64];
        let mut size = buf.len() as u32;
        let key = w16("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StuckRects3");
        let ok = RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), w16("Settings").as_ptr(), RRF_RT_REG_BINARY, null_mut(), buf.as_mut_ptr() as *mut _, &mut size);
        (ok == ERROR_SUCCESS && size as usize > i).then_some(buf[i])
    }
}

fn read_edge() -> Edge {
    if let Some(e) = reg_dword("TaskbarLocation").and_then(Edge::from) {
        return e;
    }
    stuck_rects_byte(12).and_then(|b| Edge::from(b as u32)).unwrap_or(Edge::Bottom)
}

/// Nach WM_SETTINGCHANGE: Ausrichtung, Position und die übrigen
/// Taskleisten-Einstellungen neu lesen. `true`, wenn sich die Position oder die
/// Schaltflächengröße geändert hat (dann müssen die Leisten neu angelegt werden).
pub fn reload() -> bool {
    let left = reg_dword("TaskbarAl") == Some(0);
    ALIGN.store(if left { 2 } else { 1 }, Ordering::Relaxed);
    let edge = read_edge() as u8 + 1;
    let moved = EDGE.swap(edge, Ordering::Relaxed) != edge;
    let flags = read_flags();
    let old = FLAGS.swap(flags, Ordering::Relaxed);
    moved || (old & SMALL) != (flags & SMALL)
}

// ------------------------------------- Weitere Taskleisten-Einstellungen

/// Gelesen bei `reload`; Bits wie unten, 0 = noch nicht gelesen.
static FLAGS: AtomicU32 = AtomicU32::new(0);
const READ: u32 = 1;
const SMALL: u32 = 1 << 1;
const SECONDS_SET: u32 = 1 << 2;
const SECONDS: u32 = 1 << 3;
const DESKTOP_CORNER: u32 = 1 << 4;
const FLASHING: u32 = 1 << 5;
const TASK_VIEW: u32 = 1 << 6;
const AUTOHIDE: u32 = 1 << 7;
const SEARCH_SHIFT: u32 = 8;

fn read_flags() -> u32 {
    let mut f = READ;
    // "Kleinere Taskleistenschaltflächen anzeigen".
    if reg_dword("TaskbarSmallIcons") == Some(1) {
        f |= SMALL;
    }
    // "Sekunden in der Uhr im Infobereich anzeigen".
    if let Some(v) = reg_dword("ShowSecondsInSystemClock") {
        f |= SECONDS_SET | if v != 0 { SECONDS } else { 0 };
    }
    // "Ecke ganz rechts auswählen, um den Desktop anzuzeigen" (Standard an).
    if reg_dword("TaskbarSd") != Some(0) {
        f |= DESKTOP_CORNER;
    }
    // "Blinken von Apps in der Taskleiste anzeigen" (Standard an).
    if reg_dword("TaskbarFlashing") != Some(0) {
        f |= FLASHING;
    }
    // Taskleistenelemente: Aufgabenansicht (Standard an) und Suche
    // (0 aus, 1 Symbol, 2 Suchfeld, 3 Symbol mit Beschriftung; Standard Symbol).
    if reg_dword("ShowTaskViewButton") != Some(0) {
        f |= TASK_VIEW;
    }
    // "Taskleiste automatisch ausblenden": StuckRects3, Byte 8, Bit 0x01.
    if stuck_rects_byte(8).is_some_and(|b| b & 0x01 != 0) {
        f |= AUTOHIDE;
    }
    let search = reg_dword_at("Software\\Microsoft\\Windows\\CurrentVersion\\Search", "SearchboxTaskbarMode").unwrap_or(1).min(3);
    f | (search << SEARCH_SHIFT)
}

fn flags() -> u32 {
    if FLAGS.load(Ordering::Relaxed) == 0 {
        reload();
    }
    FLAGS.load(Ordering::Relaxed)
}

/// Kleinere Taskleistenschaltflächen.
pub fn small_buttons() -> bool {
    flags() & SMALL != 0
}

/// Sekunden in der Uhr – wenn in Windows eingestellt, sonst `None`.
pub fn clock_seconds() -> Option<bool> {
    let f = flags();
    (f & SECONDS_SET != 0).then_some(f & SECONDS != 0)
}

/// Sekunden in der Uhr in Windows einstellen (Lightbars Menüpunkt schreibt
/// dieselbe Einstellung, damit beide nicht auseinanderlaufen).
pub fn set_clock_seconds(on: bool) {
    unsafe {
        let v: u32 = on as u32;
        RegSetKeyValueW(HKEY_CURRENT_USER, w16(ADVANCED).as_ptr(), w16("ShowSecondsInSystemClock").as_ptr(), REG_DWORD, &v as *const u32 as *const _, 4);
    }
    reload();
}

/// Suche in der Taskleiste ein- (als Suchfeld) oder ausschalten – dieselbe
/// Windows-Einstellung wie unter Personalisierung → Taskleiste.
pub fn set_search(on: bool) {
    let v: u32 = if on { 2 } else { 0 };
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, w16("Software\\Microsoft\\Windows\\CurrentVersion\\Search").as_ptr(), w16("SearchboxTaskbarMode").as_ptr(), REG_DWORD, &v as *const u32 as *const _, 4);
    }
    reload();
}

/// Schaltfläche "Aufgabenansicht" ein- oder ausschalten (Windows-Einstellung).
pub fn set_task_view(on: bool) {
    let v: u32 = on as u32;
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, w16(ADVANCED).as_ptr(), w16("ShowTaskViewButton").as_ptr(), REG_DWORD, &v as *const u32 as *const _, 4);
    }
    reload();
}

/// Ecke ganz rechts zeigt den Desktop.
pub fn desktop_corner() -> bool {
    flags() & DESKTOP_CORNER != 0
}

/// Apps dürfen blinken (Aufmerksamkeit).
pub fn flashing() -> bool {
    flags() & FLASHING != 0
}

/// "Taskleiste automatisch ausblenden", wie es gerade in der Registry steht.
/// Achtung: Nach der Übernahme stellt Lightbar Explorer selbst auf Ausblenden
/// (`shell::takeover`) – was der Nutzer will, ermittelt `bar::autohide_wanted`.
pub fn autohide() -> bool {
    flags() & AUTOHIDE != 0
}

/// Schaltfläche "Aufgabenansicht" anzeigen.
pub fn task_view_button() -> bool {
    flags() & TASK_VIEW != 0
}

/// Suche in der Taskleiste: 0 aus, 1 Symbol, 2 Suchfeld, 3 Symbol mit Beschriftung.
pub fn search_mode() -> u32 {
    (flags() >> SEARCH_SHIFT) & 3
}

fn reg_dword_at(key: &str, name: &str) -> Option<u32> {
    unsafe {
        let mut value = 0u32;
        let mut size = size_of::<u32>() as u32;
        let ok = RegGetValueW(HKEY_CURRENT_USER, w16(key).as_ptr(), w16(name).as_ptr(), RRF_RT_REG_DWORD, null_mut(), &mut value as *mut u32 as *mut _, &mut size);
        (ok == ERROR_SUCCESS).then_some(value)
    }
}

/// Wo ein Popup entlang der Leiste sitzt: am Anfang (links bzw. oben), in der
/// Mitte oder am Ende (rechts bzw. unten) des Ankers.
#[derive(Clone, Copy)]
pub enum Along {
    Start,
    Center,
    End,
}

/// Popup der Größe `w` × `h` neben `anchor` (Leiste oder ein Element darauf,
/// Bildschirmkoordinaten) platzieren – auf der Seite der Leiste, die zum
/// Bildschirm zeigt: über einer unteren Leiste, unter einer oberen, neben einer
/// seitlichen. Bleibt auf dem Bildschirm des Ankers.
pub unsafe fn place_near(anchor: RECT, w: i32, h: i32, gap: i32, along: Along) -> (i32, i32) {
    let m = crate::util::work_area_at((anchor.left + anchor.right) / 2, (anchor.top + anchor.bottom) / 2);
    let pos = |lo: i32, hi: i32, size: i32| match along {
        Along::Start => lo + gap,
        Along::Center => (lo + hi - size) / 2,
        Along::End => hi - gap - size,
    };
    let clamp = |v: i32, lo: i32, hi: i32, size: i32| v.clamp(lo + gap.min(8), (hi - gap.min(8) - size).max(lo));
    let (x, y) = match edge() {
        Edge::Bottom => (pos(anchor.left, anchor.right, w), anchor.top - gap - h),
        Edge::Top => (pos(anchor.left, anchor.right, w), anchor.bottom + gap),
        Edge::Left => (anchor.right + gap, pos(anchor.top, anchor.bottom, h)),
        Edge::Right => (anchor.left - gap - w, pos(anchor.top, anchor.bottom, h)),
    };
    (clamp(x, m.left, m.right, w), clamp(y, m.top, m.bottom, h))
}

/// Platz für ein Popup neben `anchor` auf dessen Bildschirm: (Breite, Höhe).
pub unsafe fn room_near(anchor: RECT, gap: i32) -> (i32, i32) {
    let m = crate::util::work_area_at((anchor.left + anchor.right) / 2, (anchor.top + anchor.bottom) / 2);
    let (w, h) = (m.right - m.left - 2 * gap, m.bottom - m.top - 2 * gap);
    match edge() {
        Edge::Bottom => (w, anchor.top - m.top - 2 * gap),
        Edge::Top => (w, m.bottom - anchor.bottom - 2 * gap),
        Edge::Left => (m.right - anchor.right - 2 * gap, h),
        Edge::Right => (anchor.left - m.left - 2 * gap, h),
    }
}

/// Einblend-Versatz (dx, dy) für Popups: von der Leiste weg gleiten (unten: von
/// unten hoch, oben: von oben herab, links: von links herein, rechts: von
/// rechts herein).
pub fn pop_offset(base: i32) -> (i32, i32) {
    offset_from(edge(), base)
}

fn offset_from(edge: Edge, base: i32) -> (i32, i32) {
    match edge {
        Edge::Bottom => (0, base),
        Edge::Top => (0, -base),
        Edge::Left => (-base, 0),
        Edge::Right => (base, 0),
    }
}

/// Windows-Einstellung "Taskleiste auf allen Bildschirmen anzeigen".
/// Fehlt der Wert, gilt der Windows-Standard (an).
pub fn windows_all_displays() -> bool {
    reg_dword("MMTaskbarEnabled") != Some(0)
}

/// Ein angeschlossener Bildschirm.
pub struct Monitor {
    pub handle: HMONITOR,
    /// Stabile Geräte-ID (Monitor-Schnittstelle), sonst der GDI-Gerätename.
    pub id: String,
    /// Anzeigename für Menüs, z. B. "Bildschirm 2 · 1080 × 1920".
    pub label: String,
    pub rect: RECT,
    pub primary: bool,
}

unsafe extern "system" fn collect(mon: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
    (*(lp as *mut Vec<HMONITOR>)).push(mon);
    1
}

fn from_wide(s: &[u16]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n])
}

/// Alle Bildschirme, der Hauptbildschirm zuerst, dann von links nach rechts.
pub fn list() -> Vec<Monitor> {
    unsafe {
        let mut handles: Vec<HMONITOR> = Vec::new();
        EnumDisplayMonitors(null_mut(), null(), Some(collect), &mut handles as *mut _ as LPARAM);
        let mut out: Vec<Monitor> = handles
            .into_iter()
            .filter_map(|handle| {
                let mut mi: MONITORINFOEXW = zeroed();
                mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
                if GetMonitorInfoW(handle, &mut mi as *mut _ as *mut MONITORINFO) == 0 {
                    return None;
                }
                let device = from_wide(&mi.szDevice);
                let mut dd: DISPLAY_DEVICEW = zeroed();
                dd.cb = size_of::<DISPLAY_DEVICEW>() as u32;
                let id = if EnumDisplayDevicesW(w16(&device).as_ptr(), 0, &mut dd, EDD_GET_DEVICE_INTERFACE_NAME) != 0 {
                    let id = from_wide(&dd.DeviceID);
                    if id.is_empty() { device.clone() } else { id }
                } else {
                    device.clone()
                };
                let r = mi.monitorInfo.rcMonitor;
                Some(Monitor {
                    handle,
                    id,
                    label: format!("{} × {}", r.right - r.left, r.bottom - r.top),
                    rect: r,
                    primary: mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                })
            })
            .collect();
        out.sort_by_key(|m| (!m.primary, m.rect.left, m.rect.top));
        for (i, m) in out.iter_mut().enumerate() {
            let main = if m.primary { t!(" (Hauptbildschirm)", " (main display)") } else { "" };
            m.label = tf!("Bildschirm {} · {}{main}", "Display {} · {}{main}", i + 1, m.label);
        }
        out
    }
}

// ------------------------------------------------- Auswahl pro Bildschirm

/// Eigene Auswahl. Bildschirme ohne eigene Auswahl folgen der Regel für
/// weitere Bildschirme – und die ohne eigene Wahl der Windows-Einstellung.
#[derive(Default)]
struct Prefs {
    /// Leiste auf weiteren Bildschirmen; None = wie Windows.
    new_monitors: Option<bool>,
    on: Vec<String>,
    off: Vec<String>,
}

static PREFS: Mutex<Option<Prefs>> = Mutex::new(None);

fn path() -> Option<std::path::PathBuf> {
    Some(crate::util::data_dir()?.join("monitors.txt"))
}

fn load() -> Prefs {
    let mut p = Prefs::default();
    let Some(text) = path().and_then(|f| std::fs::read_to_string(f).ok()) else { return p };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim().to_string();
        match k.trim() {
            "new" => p.new_monitors = Some(v == "1"),
            "on" => p.on.push(v),
            "off" => p.off.push(v),
            _ => {}
        }
    }
    p
}

fn save(p: &Prefs) {
    let Some(f) = path() else { return };
    let mut text = String::new();
    if let Some(n) = p.new_monitors {
        text.push_str(&format!("new={}\n", n as u8));
    }
    for k in &p.on {
        text.push_str(&format!("on={k}\n"));
    }
    for k in &p.off {
        text.push_str(&format!("off={k}\n"));
    }
    if let Err(e) = crate::util::write_atomic(&f, text.as_bytes()) {
        crate::util::log(&format!("monitors.txt nicht speicherbar ({e})"));
    }
}

fn with<R>(f: impl FnOnce(&mut Prefs) -> R) -> R {
    let mut g = PREFS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(load))
}

/// Leiste auf weiteren Bildschirmen: eigene Wahl, sonst wie Windows.
pub fn new_monitors() -> bool {
    with(|p| p.new_monitors).unwrap_or_else(windows_all_displays)
}

pub fn set_new_monitors(on: bool) {
    with(|p| {
        p.new_monitors = Some(on);
        save(p);
    });
}

/// Soll dieser Bildschirm eine Leiste bekommen? Der Hauptbildschirm immer.
pub fn wanted(m: &Monitor) -> bool {
    if m.primary {
        return true;
    }
    let own = with(|p| {
        if p.off.contains(&m.id) {
            Some(false)
        } else if p.on.contains(&m.id) {
            Some(true)
        } else {
            None
        }
    });
    own.unwrap_or_else(new_monitors)
}

pub fn set_wanted(id: &str, on: bool) {
    with(|p| {
        p.on.retain(|k| k != id);
        p.off.retain(|k| k != id);
        if on { &mut p.on } else { &mut p.off }.push(id.to_string());
        save(p);
    });
}

// ------------------------------------------------ Änderungen sofort merken

/// Registry-Schlüssel der Taskleisten-Einstellungen überwachen und bei jeder
/// Änderung `notify` aufrufen. Windows schickt bei manchen Einstellungen (z. B.
/// der Position) kein WM_SETTINGCHANGE – so wirken sie trotzdem sofort.
pub fn watch(notify: impl Fn() + Send + 'static) {
    std::thread::spawn(move || unsafe {
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects, WaitForSingleObject, INFINITE};
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        let keys = [ADVANCED, r"Software\Microsoft\Windows\CurrentVersion\Search", r"Software\Microsoft\Windows\CurrentVersion\Explorer\StuckRects3"];
        let mut handles: Vec<(HKEY, HANDLE)> = Vec::new();
        for k in keys {
            let mut key: HKEY = null_mut();
            if RegOpenKeyExW(HKEY_CURRENT_USER, w16(k).as_ptr(), 0, KEY_NOTIFY, &mut key) != ERROR_SUCCESS {
                continue;
            }
            let event = CreateEventW(null(), 0, 0, null());
            handles.push((key, event));
        }
        if handles.is_empty() {
            return;
        }
        // Jede Meldung gilt nur einmal: anfangs alle Schlüssel anmelden, danach
        // nur den (bzw. die), deren Ereignis kam.
        let arm = |(key, event): (HKEY, HANDLE)| {
            RegNotifyChangeKeyValue(key, 0, REG_NOTIFY_CHANGE_LAST_SET, event, 1);
        };
        for &h in &handles {
            arm(h);
        }
        let events: Vec<HANDLE> = handles.iter().map(|h| h.1).collect();
        loop {
            let fired = WaitForMultipleObjects(events.len() as u32, events.as_ptr(), 0, INFINITE).wrapping_sub(WAIT_OBJECT_0) as usize;
            if fired >= events.len() {
                return;
            }
            // Einstellungen-App schreibt oft mehrere Werte nacheinander.
            std::thread::sleep(std::time::Duration::from_millis(150));
            arm(handles[fired]);
            // Was in der Zwischenzeit an anderen Schlüsseln kam, gleich mit
            // erledigen (eine Meldung an die Leiste) und neu anmelden.
            for (i, &h) in handles.iter().enumerate() {
                if i != fired && WaitForSingleObject(h.1, 0) == WAIT_OBJECT_0 {
                    arm(h);
                }
            }
            notify();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Popups starten auf der Seite der Leiste und gleiten von ihr weg in den
    /// Bildschirm: der Versatz zeigt immer zur Leiste hin.
    #[test]
    fn pop_offset_points_toward_bar() {
        assert_eq!(offset_from(Edge::Bottom, 12), (0, 12));
        assert_eq!(offset_from(Edge::Top, 12), (0, -12));
        assert_eq!(offset_from(Edge::Left, 12), (-12, 0));
        assert_eq!(offset_from(Edge::Right, 12), (12, 0));
    }
}
