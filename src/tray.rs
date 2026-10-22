//! Infobereich (Tray-Icons).
//!
//! Shell_NotifyIcon schickt seine Daten per WM_COPYDATA (dwData = 1) an das
//! erste Fenster der Klasse "Shell_TrayWnd". Unsere Leiste trägt diese Klasse
//! und liegt in der Z-Order vor der von Explorer, also landen die Icons bei
//! uns. Alles andere, was an Shell_TrayWnd geht (vor allem AppBar-Nachrichten,
//! dwData = 0), reichen wir an Explorer weiter – Explorer bleibt für Desktop,
//! Arbeitsbereich und Dateifenster zuständig.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Graphics::Gdi::InvalidateRect;

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::util::w16;

const TRAY_SIGNATURE: u32 = 0x3475_3423;
const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIM_SETVERSION: u32 = 4;
const NIF_MESSAGE: u32 = 0x01;
const NIF_ICON: u32 = 0x02;
const NIF_TIP: u32 = 0x04;
const NIF_STATE: u32 = 0x08;
const NIF_GUID: u32 = 0x20;
const NIS_HIDDEN: u32 = 0x01;
const NIN_SELECT: u32 = WM_USER;

/// NOTIFYICONDATAW, wie die Shell es über Prozess- und Bitness-Grenzen
/// transportiert: Handles sind immer 32 Bit breit.
#[repr(C)]
#[derive(Clone, Copy)]
struct NotifyIconData32 {
    cb_size: u32,
    hwnd: u32,
    uid: u32,
    flags: u32,
    callback: u32,
    hicon: u32,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    version: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: GUID,
    balloon_icon: u32,
}

#[repr(C)]
struct ShellTrayData {
    signature: u32,
    message: u32,
    nid: NotifyIconData32,
}

pub struct TrayIcon {
    owner: HWND,
    uid: u32,
    guid: Option<GUID>,
    callback: u32,
    version: u32,
    pub icon: HICON,
    pub tip: String,
    hidden: bool,
    exe: Option<String>,
    /// In Windows' Einstellungen als "in der Taskleiste anzeigen" markiert.
    promoted: bool,
}

thread_local! {
    static ICONS: RefCell<Vec<TrayIcon>> = const { RefCell::new(Vec::new()) };
    /// Kleingeschriebene exe-Pfade der hervorgehobenen Icons.
    static PROMOTED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Known-Folder-GUIDs, mit denen Windows Pfade in NotifyIconSettings abkürzt.
const KNOWN_FOLDERS: &[(&str, &str)] = &[
    ("{6D809377-6AF0-444B-8957-A3773F02200E}", "ProgramFiles"),
    ("{7C5A40EF-A0FB-4BFC-874A-C0F2E0B9FA8E}", "ProgramFiles(x86)"),
    ("{F38BF404-1D43-42F2-9305-67DE0B28FC23}", "SystemRoot"),
    ("{F1B32785-6FBA-4FCF-9D55-7B8E7F157091}", "LOCALAPPDATA"),
    ("{3EB685DB-65F9-4CF6-A03A-E3EF65729F3D}", "APPDATA"),
    ("{5E6C858F-0E22-4760-9AFE-EA3317B67173}", "USERPROFILE"),
    ("{62AB5D82-FDC1-4DC3-A9DD-070D1D495D97}", "ProgramData"),
];

fn expand_known(path: &str) -> String {
    if path.to_uppercase().starts_with("{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}") {
        let root = std::env::var("SystemRoot").unwrap_or_default();
        return format!("{root}\\System32{}", &path[38..]);
    }
    for (guid, var) in KNOWN_FOLDERS {
        // get(): nie mitten in einem Umlaut schneiden (Pfade stammen aus der Registry).
        if path.get(..guid.len()).is_some_and(|p| p.eq_ignore_ascii_case(guid)) {
            let root = std::env::var(var).unwrap_or_default();
            return format!("{root}{}", &path[guid.len()..]);
        }
    }
    path.to_string()
}

/// Liest HKCU\Control Panel\NotifyIconSettings: welche Apps ihr Icon direkt
/// in der Leiste zeigen sollen (dieselbe Einstellung wie bei Windows).
unsafe fn read_promoted() -> Vec<String> {
    let mut out = Vec::new();
    let mut key: HKEY = null_mut();
    if RegOpenKeyExW(HKEY_CURRENT_USER, w16(r"Control Panel\NotifyIconSettings").as_ptr(), 0, KEY_READ, &mut key) != 0 {
        return out;
    }
    let (promoted_name, path_name) = (w16("IsPromoted"), w16("ExecutablePath"));
    for i in 0.. {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        if RegEnumKeyExW(key, i, name.as_mut_ptr(), &mut len, null_mut(), null_mut(), null_mut(), null_mut()) != 0 {
            break;
        }
        let mut promoted: u32 = 0;
        let mut size = 4u32;
        let ok = RegGetValueW(key, name.as_ptr(), promoted_name.as_ptr(), RRF_RT_REG_DWORD, null_mut(), &mut promoted as *mut _ as _, &mut size);
        if ok != 0 || promoted == 0 {
            continue;
        }
        let mut buf = [0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        if RegGetValueW(key, name.as_ptr(), path_name.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr() as _, &mut bytes) == 0 {
            out.push(expand_known(&wstr(&buf)).to_lowercase());
        }
    }
    RegCloseKey(key);
    out
}

fn is_promoted(exe: Option<&str>) -> bool {
    let Some(exe) = exe else { return false };
    PROMOTED.with(|p| p.borrow().iter().any(|e| e == exe))
}

/// Hervorhebungs-Einstellung (neu) lesen und auf alle Icons anwenden. Beim
/// Start vor `announce`, damit die Icons gleich am richtigen Platz landen
/// statt erst im Überlauf; danach bei jeder Änderung (`watch_promoted`).
pub unsafe fn load_promoted() {
    let list = read_promoted();
    PROMOTED.with(|p| *p.borrow_mut() = list);
    ICONS.with(|cell| {
        for i in cell.borrow_mut().iter_mut() {
            i.promoted = is_promoted(i.exe.as_deref());
        }
    });
    let ov = HWND_OVERFLOW.load(std::sync::atomic::Ordering::Relaxed);
    if !ov.is_null() && IsWindowVisible(ov) != 0 {
        InvalidateRect(ov, null(), 0);
    }
}

/// HKCU\Control Panel\NotifyIconSettings beobachten (samt Unterschlüsseln:
/// je App einer, IsPromoted darin) und bei Änderungen `notify` rufen – statt
/// die Registry regelmäßig abzufragen. Wie `display::watch` auf eigenem Thread.
pub fn watch_promoted(notify: impl Fn() + Send + 'static) {
    std::thread::spawn(move || unsafe {
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
        let path = w16(r"Control Panel\NotifyIconSettings");
        let mut key: HKEY = null_mut();
        // Frisches Profil: Den Schlüssel legt Explorer erst mit dem ersten Icon an.
        while RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_NOTIFY, &mut key) != 0 {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
        notify();
        let event = CreateEventW(null(), 0, 0, null());
        if event.is_null() {
            RegCloseKey(key);
            return;
        }
        loop {
            let filter = REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET;
            if RegNotifyChangeKeyValue(key, 1, filter, event, 1) != 0 {
                break;
            }
            if WaitForSingleObject(event, INFINITE) != WAIT_OBJECT_0 {
                break;
            }
            // Die Einstellungen-App schreibt oft mehrere Werte nacheinander.
            std::thread::sleep(std::time::Duration::from_millis(150));
            notify();
        }
        CloseHandle(event);
        RegCloseKey(key);
    });
}

unsafe fn owner_exe(owner: HWND) -> Option<String> {
    let mut pid = 0u32;
    GetWindowThreadProcessId(owner, &mut pid);
    let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if proc.is_null() {
        return None;
    }
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(proc, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
    CloseHandle(proc);
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]).to_lowercase())
}

fn handle32(v: u32) -> *mut c_void {
    // 32-Bit-Handles werden vorzeichenerweitert, wie es Windows selbst tut.
    v as i32 as isize as *mut c_void
}

fn same_guid(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

fn wstr(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

impl TrayIcon {
    fn matches(&self, nid: &NotifyIconData32) -> bool {
        if nid.flags & NIF_GUID != 0 {
            if let Some(g) = &self.guid {
                return same_guid(g, &nid.guid);
            }
        }
        self.owner == handle32(nid.hwnd) && self.uid == nid.uid
    }

    unsafe fn apply(&mut self, nid: &NotifyIconData32) {
        if nid.flags & NIF_MESSAGE != 0 {
            self.callback = nid.callback;
        }
        if nid.flags & NIF_ICON != 0 {
            // Das Handle gehört dem Absender – wir brauchen eine eigene Kopie.
            let copy = CopyIcon(handle32(nid.hicon));
            if !self.icon.is_null() {
                DestroyIcon(self.icon);
            }
            self.icon = copy;
        }
        if nid.flags & NIF_TIP != 0 {
            self.tip = wstr(&nid.tip);
        }
        if nid.flags & NIF_STATE != 0 && nid.state_mask & NIS_HIDDEN != 0 {
            self.hidden = nid.state & NIS_HIDDEN != 0;
        }
    }
}

/// Was eine Tray-Meldung an der Leiste ändert.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TrayChange {
    /// Nichts Sichtbares (nur Tooltip, Version, ausgeblendetes Icon …).
    None,
    /// Nur Bilder von Icons in der Leiste: Infobereich neu zeichnen.
    Icons,
    /// Anzahl der Icons in Leiste oder Überlauf: Aufteilung der Leiste ändert sich.
    Layout,
}

/// Wo ein Icon steht: gar nicht (ausgeblendet/ohne Bild), in der Leiste
/// (hervorgehoben) oder nur im Überlauf.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Place {
    Gone,
    Bar,
    Overflow,
}

impl TrayIcon {
    fn place(&self) -> Place {
        if self.hidden || self.icon.is_null() {
            Place::Gone
        } else if self.promoted {
            Place::Bar
        } else {
            Place::Overflow
        }
    }
}

/// Auswirkung einer Meldung (rein, testbar): Platz des Icons vorher/nachher,
/// ob sein Bild gewechselt hat und ob es (in der Leiste) zu den angezeigten
/// gehört. Ergebnis: (Leiste, Überlauf betroffen).
fn classify(before: Place, after: Place, icon_changed: bool, shown_in_bar: bool) -> (TrayChange, bool) {
    if before != after {
        return (TrayChange::Layout, true);
    }
    match after {
        Place::Gone => (TrayChange::None, false),
        _ if !icon_changed => (TrayChange::None, false),
        Place::Bar if shown_in_bar => (TrayChange::Icons, false),
        // Hervorgehoben, aber aus Platzmangel im Überlauf, oder ohnehin dort.
        _ => (TrayChange::None, true),
    }
}

/// WM_COPYDATA an Shell_TrayWnd. `None` heißt: nicht unser Thema, weiterreichen.
/// Sonst die Antwort an die App und was sich an der Leiste sichtbar ändert.
pub unsafe fn handle_copydata(lp: LPARAM) -> Option<(LRESULT, TrayChange)> {
    let cds = &*(lp as *const COPYDATASTRUCT);
    if cds.dwData != 1 || cds.lpData.is_null() || (cds.cbData as usize) < 8 + 12 {
        return None;
    }
    // Ältere Absender schicken kürzere Strukturen: nur kopieren, was da ist.
    let mut data: ShellTrayData = zeroed();
    let n = (cds.cbData as usize).min(size_of::<ShellTrayData>());
    std::ptr::copy_nonoverlapping(cds.lpData as *const u8, &mut data as *mut _ as *mut u8, n);
    if data.signature != TRAY_SIGNATURE {
        return None;
    }
    let nid = data.nid;
    let shown = SHOWN.load(std::sync::atomic::Ordering::Relaxed);
    let (ok, change, overflow_dirty) = ICONS.with(|cell| {
        let mut icons = cell.borrow_mut();
        let pos = icons.iter().position(|i| i.matches(&nid));
        let before = pos.map(|i| icons[i].place()).unwrap_or(Place::Gone);
        let ok = match (data.message, pos) {
            (NIM_ADD, None) => {
                let exe = owner_exe(handle32(nid.hwnd));
                let promoted = is_promoted(exe.as_deref());
                let mut icon = TrayIcon {
                    exe,
                    promoted,
                    owner: handle32(nid.hwnd),
                    uid: nid.uid,
                    guid: (nid.flags & NIF_GUID != 0).then_some(nid.guid),
                    callback: 0,
                    version: 0,
                    icon: null_mut(),
                    tip: String::new(),
                    hidden: false,
                };
                icon.apply(&nid);
                icons.push(icon);
                true
            }
            (NIM_MODIFY, Some(i)) => {
                icons[i].apply(&nid);
                true
            }
            (NIM_DELETE, Some(i)) => {
                let icon = icons.remove(i);
                if !icon.icon.is_null() {
                    DestroyIcon(icon.icon);
                }
                true
            }
            (NIM_SETVERSION, Some(i)) => {
                icons[i].version = nid.version;
                true
            }
            // NIM_SETFOCUS und Unbekanntes: bestätigen, sonst versuchen es manche Apps endlos.
            (m, Some(_)) if m > NIM_SETVERSION || m == 3 => true,
            _ => false,
        };
        // Das Icon danach: neu angelegt hinten, sonst an seinem Platz.
        let (after, at) = match (data.message, pos) {
            (NIM_ADD, None) if ok => (icons.last().map(|i| i.place()).unwrap_or(Place::Gone), icons.len().checked_sub(1)),
            (NIM_DELETE, _) => (Place::Gone, None),
            (_, Some(i)) => (icons[i].place(), Some(i)),
            _ => (Place::Gone, None),
        };
        let icon_changed = data.message == NIM_MODIFY && nid.flags & NIF_ICON != 0;
        // Rang unter den Leisten-Icons: nur die ersten `shown` stehen wirklich dort.
        let shown_in_bar = at.is_some_and(|at| icons[..at].iter().filter(|i| i.place() == Place::Bar).count() < shown);
        let (change, overflow_dirty) = classify(before, after, icon_changed, shown_in_bar);
        // Entfernen verschiebt die Plätze aller späteren Icons (Überlauf-Felder!).
        let overflow_dirty = overflow_dirty || data.message == NIM_DELETE && pos.is_some();
        (ok, change, overflow_dirty)
    });
    // Offener Überlauf: sofort neu zeichnen. Seine Felder verweisen auf Plätze in
    // der Icon-Liste, die sich beim Entfernen verschieben – ein Klick träfe
    // sonst die falsche App.
    let overflow = HWND_OVERFLOW.load(std::sync::atomic::Ordering::Relaxed);
    if overflow_dirty && !overflow.is_null() && IsWindowVisible(overflow) != 0 {
        InvalidateRect(overflow, std::ptr::null(), 0);
        windows_sys::Win32::Graphics::Gdi::UpdateWindow(overflow);
    }
    Some((ok as LRESULT, change))
}

/// Tray-Nachricht zusätzlich an Explorer weiterreichen. Explorer führt so
/// seine eigene Kopie der Icons und macht aus Sprechblasen (NIF_INFO)
/// Windows-Benachrichtigungen – das kann nur Explorer. Seine Leiste ist
/// versteckt (`shell::active`), seine Kopie also unsichtbar; ohne Übernahme
/// (Explorers Leiste sichtbar) wird nichts weitergereicht, sonst stünden die
/// Icons doppelt da. Die App bekommt weiter unsere Antwort, nicht Explorers.
///
/// Gesendet wird auf einem eigenen Thread in der Reihenfolge des Eingangs:
/// Ein hängender Explorer bremst so weder die Leiste noch die App.
pub unsafe fn forward_to_explorer(ours: HWND, wp: WPARAM, lp: LPARAM) {
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Mutex, OnceLock};
    static QUEUE: OnceLock<Mutex<Sender<(isize, usize, usize, Vec<u8>)>>> = OnceLock::new();
    if !crate::shell::active() {
        return;
    }
    let cds = &*(lp as *const COPYDATASTRUCT);
    if cds.lpData.is_null() || cds.cbData == 0 {
        return;
    }
    let data = std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize).to_vec();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = channel::<(isize, usize, usize, Vec<u8>)>();
        std::thread::spawn(move || unsafe {
            for (ours, wp, kind, mut data) in rx {
                let ex = explorer_tray(ours as HWND);
                if ex.is_null() {
                    continue;
                }
                let cds = COPYDATASTRUCT { dwData: kind, cbData: data.len() as u32, lpData: data.as_mut_ptr() as *mut c_void };
                let mut result = 0usize;
                SendMessageTimeoutW(ex, WM_COPYDATA, wp, &cds as *const _ as LPARAM, SMTO_ABORTIFHUNG, 2000, &mut result);
            }
        });
        Mutex::new(tx)
    });
    if let Ok(tx) = queue.lock() {
        let _ = tx.send((ours as isize, wp, cds.dwData, data));
    }
}

/// Explorers eigenes Shell_TrayWnd (nicht unseres).
pub unsafe fn explorer_tray(ours: HWND) -> HWND {
    let class = w16("Shell_TrayWnd");
    let mut h = null_mut();
    loop {
        h = FindWindowExW(null_mut(), h, class.as_ptr(), std::ptr::null());
        if h.is_null() || h != ours {
            return h;
        }
    }
}

/// Sicherheitsnetz (aus dem langsamen Takt der Leiste): verwaiste Icons
/// entfernen (Absturz ohne NIM_DELETE). Die Hervorhebung meldet `watch_promoted`.
pub unsafe fn tick() {
    ICONS.with(|cell| {
        cell.borrow_mut().retain(|i| {
            let alive = IsWindow(i.owner) != 0;
            if !alive && !i.icon.is_null() {
                DestroyIcon(i.icon);
            }
            alive
        });
    });
}

/// Icon an `index`, falls sein Fenster noch lebt: (Besitzer, uid, Callback, Version).
/// Ist der Besitzer weg (Absturz ohne NIM_DELETE), fliegt das Icon sofort
/// raus und Leiste samt Überlauf werden neu gezeichnet – wie bei Explorer,
/// wo tote Icons beim Darüberfahren verschwinden.
unsafe fn live(index: usize) -> Option<(HWND, u32, u32, u32)> {
    let (owner, uid, callback, version) = ICONS.with(|cell| cell.borrow().get(index).map(|i| (i.owner, i.uid, i.callback, i.version)))?;
    if IsWindow(owner) != 0 {
        return Some((owner, uid, callback, version));
    }
    ICONS.with(|cell| {
        let mut icons = cell.borrow_mut();
        if index < icons.len() {
            let icon = icons.remove(index);
            if !icon.icon.is_null() {
                DestroyIcon(icon.icon);
            }
        }
    });
    let ov = HWND_OVERFLOW.load(std::sync::atomic::Ordering::Relaxed);
    if !ov.is_null() && IsWindowVisible(ov) != 0 {
        if overflow().is_empty() {
            hide_overflow();
        } else {
            InvalidateRect(ov, null(), 0);
        }
    }
    for bar in crate::bar::all_bars() {
        InvalidateRect(bar, null(), 0);
    }
    None
}

fn listed(promoted: bool) -> Vec<(usize, HICON)> {
    ICONS.with(|cell| {
        cell.borrow()
            .iter()
            .enumerate()
            .filter(|(_, i)| !i.hidden && !i.icon.is_null() && i.promoted == promoted)
            .map(|(n, i)| (n, i.icon))
            .collect()
    })
}

/// Direkt in der Leiste sichtbare Icons: (Index, Icon).
pub fn visible() -> Vec<(usize, HICON)> {
    listed(true)
}

/// Wie viele der sichtbaren Icons in die Leiste passen (setzt das Layout).
static SHOWN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);

pub fn set_shown(n: usize) {
    SHOWN.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// Icons, die Windows ausblendet (unabhängig vom Platz in der Leiste).
pub fn has_hidden() -> bool {
    !listed(false).is_empty()
}

/// Icons hinter dem Überlauf-Pfeil: die, die nicht mehr in die Leiste
/// passen, und die, die Windows ausblendet.
pub fn overflow() -> Vec<(usize, HICON)> {
    let shown = SHOWN.load(std::sync::atomic::Ordering::Relaxed);
    let mut v: Vec<(usize, HICON)> = listed(true).into_iter().skip(shown).collect();
    v.extend(listed(false));
    v
}

pub fn tip(index: usize) -> String {
    ICONS.with(|cell| cell.borrow().get(index).map(|i| i.tip.clone()).unwrap_or_default())
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Click {
    Left,
    Right,
    Double,
    /// Enter/Leertaste auf dem Icon im Tastaturfokus.
    Key,
    /// Kontextmenü-Taste bzw. Umschalt+F10 im Tastaturfokus.
    KeyMenu,
}

const NIN_KEYSELECT: u32 = WM_USER + 1;

/// Nachrichten an die App, wie Explorer sie schickt. `after_double`: Das ist
/// das Loslassen nach einem Doppelklick – dort kommt kein weiteres
/// WM_LBUTTONDOWN (die App hat schon WM_LBUTTONDBLCLK bekommen), nur das
/// Loslassen samt NIN_SELECT. Qt-Apps z. B. erwarten genau dieses eine
/// NIN_SELECT nach dem Doppelklick und verwerfen es; ein zusätzliches würde
/// als neuer Klick zählen.
fn messages(kind: Click, version: u32, after_double: bool) -> Vec<u32> {
    let v3 = version >= 3;
    let mut out = Vec::new();
    match kind {
        Click::Left => {
            if !after_double {
                out.push(WM_LBUTTONDOWN);
            }
            out.push(WM_LBUTTONUP);
            if v3 {
                out.push(NIN_SELECT);
            }
        }
        Click::Double => out.push(WM_LBUTTONDBLCLK),
        Click::Right => {
            out.extend([WM_RBUTTONDOWN, WM_RBUTTONUP]);
            if v3 {
                out.push(WM_CONTEXTMENU);
            }
        }
        // Ältere Apps (vor Version 3) kennen keine Tastatur-Meldungen: Klick nachbilden.
        Click::Key if v3 => out.push(NIN_KEYSELECT),
        Click::Key => out.extend([WM_LBUTTONDOWN, WM_LBUTTONUP]),
        Click::KeyMenu if v3 => out.push(WM_CONTEXTMENU),
        Click::KeyMenu => out.extend([WM_RBUTTONDOWN, WM_RBUTTONUP]),
    }
    out
}

/// wParam/lParam einer Callback-Nachricht: ab Version 4 mit Ankerpunkt und
/// uid im oberen Wort, davor uid und Nachricht.
fn encode(version: u32, uid: u32, anchor: POINT, msg: u32) -> (WPARAM, LPARAM) {
    if version >= 4 {
        let wp = (anchor.x as u16 as usize) | ((anchor.y as u16 as usize) << 16);
        let lp = (msg & 0xffff) as isize | ((uid as isize & 0xffff) << 16);
        (wp, lp)
    } else {
        (uid as usize, msg as isize)
    }
}

thread_local! {
    /// Letzter Doppelklick: (Icon-Index, GetTickCount) – das folgende Loslassen
    /// gehört noch zu ihm.
    static LAST_DOUBLE: std::cell::Cell<Option<(usize, u32)>> = const { std::cell::Cell::new(None) };
}

/// Klick so an die App melden, wie Explorer es tut. `anchor` = Bildschirmpunkt.
pub unsafe fn click(index: usize, kind: Click, anchor: POINT) {
    let now = windows_sys::Win32::System::SystemInformation::GetTickCount();
    let last = LAST_DOUBLE.with(|c| c.take());
    if kind == Click::Double {
        LAST_DOUBLE.with(|c| c.set(Some((index, now))));
    }
    let after_double = kind == Click::Left
        && last.is_some_and(|(i, t)| i == index && now.wrapping_sub(t) <= windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime());
    let Some((owner, uid, callback, version)) = live(index) else { return };
    if callback == 0 {
        return;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(owner, &mut pid);
    // Die App darf ihr Menü/Fenster in den Vordergrund holen.
    AllowSetForegroundWindow(pid);
    for msg in messages(kind, version, after_double) {
        let (wp, lp) = encode(version, uid, anchor, msg);
        SendNotifyMessageW(owner, callback, wp, lp);
    }
}

/// Maus über dem Icon: der App melden, wie Explorer es tut (manche Apps
/// zeigen daraufhin eigene Infos oder aktualisieren ihren Tooltip).
pub unsafe fn hover(index: usize, anchor: POINT) {
    let Some((owner, uid, callback, version)) = live(index) else { return };
    if callback == 0 {
        return;
    }
    let (wp, lp) = encode(version, uid, anchor, WM_MOUSEMOVE);
    SendNotifyMessageW(owner, callback, wp, lp);
}

/// Alle Apps bitten, ihre Icons neu anzumelden (bei uns bzw. nach dem
/// Beenden wieder bei Explorer).
pub unsafe fn announce() {
    let msg = RegisterWindowMessageW(w16("TaskbarCreated").as_ptr());
    SendNotifyMessageW(HWND_BROADCAST, msg, 0, 0);
}

// ------------------------------------------------------------ Einfarbig

thread_local! {
    /// Silhouetten der Tray-Icons in der Akzentfarbe, je (Quell-Icon, Größe).
    static MONO: RefCell<Vec<(HICON, i32, HICON)>> = const { RefCell::new(Vec::new()) };
    /// Akzentfarbe, in der die gemerkten Silhouetten gefärbt sind.
    static MONO_COLOR: std::cell::Cell<u32> = const { std::cell::Cell::new(u32::MAX) };
}

/// Icon als Silhouette in der Akzentfarbe (Alpha bleibt), gecacht. Ändert sich
/// die Akzentfarbe, werden alle neu gefärbt.
pub unsafe fn mono(icon: HICON, size: i32) -> HICON {
    let color = crate::util::accent();
    if MONO_COLOR.with(|c| c.replace(color)) != color {
        MONO.with(|c| {
            for e in c.borrow_mut().drain(..) {
                DestroyIcon(e.2);
            }
        });
    }
    if let Some(m) = MONO.with(|c| c.borrow().iter().find(|e| e.0 == icon && e.1 == size).map(|e| e.2)) {
        return m;
    }
    // Fehlgeschlagen (z. B. GDI erschöpft): Original zeigen, nichts merken –
    // sonst würde das fremde Icon später als "unseres" freigegeben.
    let Some(m) = make_mono(icon, size) else { return icon };
    MONO.with(|c| {
        let mut c = c.borrow_mut();
        // Alte Einträge freigeben, deren Quell-Icon es nicht mehr gibt.
        let live: Vec<HICON> = ICONS.with(|i| i.borrow().iter().map(|t| t.icon).collect());
        c.retain(|e| {
            let keep = live.contains(&e.0);
            if !keep {
                DestroyIcon(e.2);
            }
            keep
        });
        c.push((icon, size, m));
    });
    m
}

unsafe fn make_mono(icon: HICON, size: i32) -> Option<HICON> {
    let c = crate::util::accent();
    let bgr = (((c >> 16) & 0xff) as u8, ((c >> 8) & 0xff) as u8, (c & 0xff) as u8);
    use windows_sys::Win32::Graphics::Gdi::*;
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = size;
    bi.bmiHeader.biHeight = -size;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    let mut bits: *mut c_void = null_mut();
    let screen = GetDC(null_mut());
    let dib = CreateDIBSection(screen, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if dib.is_null() || bits.is_null() {
        ReleaseDC(null_mut(), screen);
        if !dib.is_null() {
            DeleteObject(dib);
        }
        return None;
    }
    let dc = CreateCompatibleDC(screen);
    ReleaseDC(null_mut(), screen);
    let old = SelectObject(dc, dib);
    DrawIconEx(dc, 0, 0, icon, size, size, 0, null_mut(), DI_NORMAL);
    let px = std::slice::from_raw_parts_mut(bits as *mut u8, (size * size * 4) as usize);
    let has_alpha = px.chunks(4).any(|p| p[3] != 0);
    // Ohne Alphakanal (alte Icons) zählt jedes nicht-schwarze Pixel als Form.
    let alpha = |p: &[u8]| if has_alpha { p[3] } else if p[0] | p[1] | p[2] != 0 { 255 } else { 0 };
    // Helligkeit, auf die volle Deckkraft hochgerechnet (Pixel sind vormultipliziert).
    let lum = |p: &[u8], a: u8| -> f32 {
        let l = 0.114 * p[0] as f32 + 0.587 * p[1] as f32 + 0.299 * p[2] as f32;
        if has_alpha && a > 0 { l * 255.0 / a as f32 } else { l }
    };
    // "Badge"-Icons (Symbol auf gefülltem Kreis/Quadrat) würden als Silhouette
    // zum Klecks. Dort zählt stattdessen, was sich vom Hintergrund abhebt.
    let opaque: Vec<f32> = px.chunks(4).filter(|p| alpha(p) > 128).map(|p| lum(p, alpha(p))).collect();
    let badge = opaque.len() as f32 > 0.45 * (size * size) as f32;
    let mean = opaque.iter().sum::<f32>() / opaque.len().max(1) as f32;
    for p in px.chunks_mut(4) {
        let a = alpha(p);
        let a = if badge {
            let contrast = ((lum(p, a) - mean).abs() * 3.0).min(255.0);
            (contrast * a as f32 / 255.0) as u8
        } else {
            a
        };
        // Icon-Bitmaps tragen ungemultipliziertes Alpha: Farbe bleibt voll (BGR).
        p[0] = bgr.0;
        p[1] = bgr.1;
        p[2] = bgr.2;
        p[3] = a;
    }
    SelectObject(dc, old);
    DeleteDC(dc);
    let mask = CreateBitmap(size, size, 1, 1, null());
    let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: dib };
    let out = CreateIconIndirect(&info);
    DeleteObject(mask);
    DeleteObject(dib);
    (!out.is_null()).then_some(out)
}

// ------------------------------------------------------------ Überlauf

static HWND_OVERFLOW: std::sync::atomic::AtomicPtr<c_void> = std::sync::atomic::AtomicPtr::new(null_mut());

struct Overflow {
    dpi: u32,
    mouse: (i32, i32),
    cells: Vec<(RECT, usize)>,
    /// Icon-Index unter der Maus (für Tooltip und Hover-Meldung).
    hover: Option<usize>,
}

thread_local! {
    static OVERFLOW: RefCell<Overflow> = const { RefCell::new(Overflow { dpi: 96, mouse: (-1, -1), cells: Vec::new(), hover: None }) };
}

const CELL: i32 = 40;
const COLS: usize = 5;

unsafe extern "system" fn overflow_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    use crate::util::*;
    use windows_sys::Win32::Graphics::Gdi::*;
    match msg {
        WM_PAINT => {
            let icons = overflow();
            let mono_on = crate::settings::get().mono_tray;
            paint_buffered(hwnd, |dc, w, h| {
                fill(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, BG);
                OVERFLOW.with(|o| {
                    let mut o = o.borrow_mut();
                    let d = o.dpi;
                    let (c, pad, is) = (px(d, CELL), px(d, 8), px(d, 16));
                    o.cells.clear();
                    for (n, &(index, icon)) in icons.iter().enumerate() {
                        let (col, row) = ((n % COLS) as i32, (n / COLS) as i32);
                        let r = RECT { left: pad + col * c, top: pad + row * c, right: pad + (col + 1) * c, bottom: pad + (row + 1) * c };
                        if inside(&r, o.mouse.0, o.mouse.1) {
                            round(dc, &r, HOVER, px(d, 6));
                        }
                        let icon = if mono_on { mono(icon, is) } else { icon };
                        DrawIconEx(dc, r.left + (c - is) / 2, r.top + (c - is) / 2, icon, is, is, 0, null_mut(), DI_NORMAL);
                        o.cells.push((r, index));
                    }
                });
            });
            0
        }
        WM_ERASEBKGND => 1,
        // Nie aktiv werden: Sonst klappt der Überlauf zu, sobald eine App ihr
        // Kontextmenü öffnet (das holt sich den Fokus).
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_TIMER if wp == TIMER_OUTSIDE => {
            // Nach einem Klick: Die App hat ihr Fenster/Menü nach vorne geholt.
            if PENDING_HIDE.with(|p| p.get()).is_some_and(|(fg, _)| GetForegroundWindow() != fg) {
                hide_overflow();
                return 0;
            }
            watch_outside_click(hwnd);
            0
        }
        WM_TIMER if wp == TIMER_TIP => {
            KillTimer(hwnd, TIMER_TIP);
            let hit = OVERFLOW.with(|o| {
                let o = o.borrow();
                let index = o.hover?;
                o.cells.iter().find(|c| c.1 == index).map(|c| (c.0, index, o.dpi))
            });
            if let Some((r, index, dpi)) = hit {
                crate::tooltip::show(&tip(index), rect_to_screen(hwnd, r), dpi);
            }
            0
        }
        WM_MOUSEMOVE => {
            let p = crate::util::xy(lp);
            let (changed, now) = OVERFLOW.with(|o| {
                let mut o = o.borrow_mut();
                o.mouse = p;
                let now = o.cells.iter().find(|c| crate::util::inside(&c.0, p.0, p.1)).map(|c| (c.1, c.0));
                let changed = o.hover != now.map(|n| n.0);
                o.hover = now.map(|n| n.0);
                (changed, now)
            });
            if changed {
                crate::tooltip::hide();
                if let Some((index, r)) = now {
                    SetTimer(hwnd, TIMER_TIP, 450, None);
                    let mut pt = POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
                    ClientToScreen(hwnd, &mut pt);
                    hover(index, pt);
                }
            }
            let mut tme = windows_sys::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT {
                cbSize: size_of::<windows_sys::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT>() as u32,
                dwFlags: windows_sys::Win32::UI::Input::KeyboardAndMouse::TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            windows_sys::Win32::UI::Input::KeyboardAndMouse::TrackMouseEvent(&mut tme);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        0x02A3 => {
            KillTimer(hwnd, TIMER_TIP);
            crate::tooltip::hide();
            OVERFLOW.with(|o| {
                let mut o = o.borrow_mut();
                o.mouse = (-1, -1);
                o.hover = None;
            });
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_LBUTTONDBLCLK => {
            KillTimer(hwnd, TIMER_TIP);
            crate::tooltip::hide();
            let (x, y) = crate::util::xy(lp);
            let hit = OVERFLOW.with(|o| o.borrow().cells.iter().find(|(r, _)| crate::util::inside(r, x, y)).map(|(r, i)| (*r, *i)));
            if let Some((r, index)) = hit {
                let mut pt = POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
                ClientToScreen(hwnd, &mut pt);
                let kind = match msg {
                    WM_RBUTTONUP => Click::Right,
                    WM_LBUTTONDBLCLK => Click::Double,
                    _ => Click::Left,
                };
                click(index, kind, pt);
                // Rechtsklick lässt den Überlauf offen, wie bei Windows. Links
                // nicht sofort schließen: Ein Doppelklick braucht das Fenster
                // noch (zweiter Klick und sein Loslassen). Zu geht er nach der
                // Doppelklickzeit, sobald die App etwas in den Vordergrund holt
                // oder gleich nach dem Loslassen eines Doppelklicks.
                use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
                match kind {
                    Click::Right => {}
                    Click::Double => {
                        PENDING_HIDE.with(|p| p.set(Some((GetForegroundWindow(), true))));
                        SetTimer(hwnd, TIMER_HIDE, GetDoubleClickTime(), None);
                    }
                    _ if PENDING_HIDE.with(|p| p.get()).is_some_and(|p| p.1) => hide_overflow(),
                    _ => {
                        PENDING_HIDE.with(|p| p.set(Some((GetForegroundWindow(), false))));
                        SetTimer(hwnd, TIMER_HIDE, GetDoubleClickTime(), None);
                    }
                }
            }
            0
        }
        WM_TIMER if wp == TIMER_HIDE => {
            hide_overflow();
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub unsafe fn create_overflow(inst: HINSTANCE) {
    use crate::util::*;
    let class = w16("LightbarTrayOverflow");
    register_class(inst, &class, Some(overflow_proc), CS_DBLCLKS);
    let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED, class.as_ptr(), std::ptr::null(), WS_POPUP, 0, 0, 0, 0, null_mut(), null_mut(), inst, std::ptr::null());
    crate::anim::make_opaque(hwnd);
    let round_pref: u32 = 2;
    let border = BORDER;
    windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute(hwnd, 33, &round_pref as *const _ as _, 4);
    windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute(hwnd, 34, &border as *const _ as _, 4);
    HWND_OVERFLOW.store(hwnd, std::sync::atomic::Ordering::Relaxed);
}

/// Überlauf neben dem Pfeil (`anchor`, Bildschirmkoordinaten) öffnen/schließen.
pub unsafe fn toggle_overflow(anchor: RECT, dpi: u32) {
    use crate::util::*;
    let hwnd = HWND_OVERFLOW.load(std::sync::atomic::Ordering::Relaxed);
    if hwnd.is_null() {
        return;
    }
    if IsWindowVisible(hwnd) != 0 {
        hide_overflow();
        return;
    }
    let n = overflow().len().max(1);
    let cols = n.min(COLS) as i32;
    let rows = n.div_ceil(COLS) as i32;
    OVERFLOW.with(|o| o.borrow_mut().dpi = dpi);
    let (w, h) = (cols * px(dpi, CELL) + px(dpi, 16), rows * px(dpi, CELL) + px(dpi, 16));
    let (x, y) = crate::display::place_near(anchor, w, h, px(dpi, 8), crate::display::Along::Center);
    crate::anim::pop_in(hwnd, x, y, w, h, crate::display::pop_offset(px(dpi, 12)));
    MOUSE_WAS_DOWN.store(true, std::sync::atomic::Ordering::Relaxed);
    SetTimer(hwnd, TIMER_OUTSIDE, 50, None);
    InvalidateRect(hwnd, std::ptr::null(), 0);
}

const TIMER_OUTSIDE: usize = 7;
const TIMER_TIP: usize = 8;
/// Schließen nach einem Linksklick, verzögert um die Doppelklickzeit.
const TIMER_HIDE: usize = 9;
static MOUSE_WAS_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    /// Schließen steht an: (Vordergrundfenster beim Klick, war ein Doppelklick).
    static PENDING_HIDE: std::cell::Cell<Option<(HWND, bool)>> = const { std::cell::Cell::new(None) };
}

pub unsafe fn hide_overflow() {
    let hwnd = HWND_OVERFLOW.load(std::sync::atomic::Ordering::Relaxed);
    PENDING_HIDE.with(|p| p.set(None));
    if !hwnd.is_null() {
        KillTimer(hwnd, TIMER_HIDE);
    }
    if !hwnd.is_null() && IsWindowVisible(hwnd) != 0 {
        KillTimer(hwnd, TIMER_TIP);
        crate::tooltip::hide();
        KillTimer(hwnd, TIMER_OUTSIDE);
        ShowWindow(hwnd, SW_HIDE);
    }
}

/// Ohne Fokus merkt der Überlauf keinen Klick daneben – also selbst schauen:
/// Neuer Mausklick außerhalb (und nicht in einem Menü oder auf der Leiste,
/// die das selbst regelt) schließt ihn.
unsafe fn watch_outside_click(hwnd: HWND) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    let down = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON].iter().any(|&k| GetAsyncKeyState(k as i32) as u16 & 0x8000 != 0);
    let was = MOUSE_WAS_DOWN.swap(down, std::sync::atomic::Ordering::Relaxed);
    if !down || was {
        return;
    }
    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    let target = WindowFromPoint(pt);
    let root = GetAncestor(target, GA_ROOT);
    if root == hwnd || root == crate::bar::hwnd() {
        return;
    }
    let mut class = [0u16; 16];
    let n = GetClassNameW(root, class.as_mut_ptr(), class.len() as i32) as usize;
    if String::from_utf16_lossy(&class[..n]) == "#32768" {
        return; // Kontextmenü einer App
    }
    hide_overflow();
}

pub fn overflow_visible() -> bool {
    let hwnd = HWND_OVERFLOW.load(std::sync::atomic::Ordering::Relaxed);
    unsafe { !hwnd.is_null() && IsWindowVisible(hwnd) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_messages_like_explorer() {
        assert_eq!(messages(Click::Left, 0, false), vec![WM_LBUTTONDOWN, WM_LBUTTONUP]);
        // NIN_SELECT schon ab Version 3, nicht erst ab 4.
        assert_eq!(messages(Click::Left, 3, false), vec![WM_LBUTTONDOWN, WM_LBUTTONUP, NIN_SELECT]);
        assert_eq!(messages(Click::Left, 4, false), vec![WM_LBUTTONDOWN, WM_LBUTTONUP, NIN_SELECT]);
        // Doppelklick: nur WM_LBUTTONDBLCLK, das Loslassen danach ohne neues Drücken.
        assert_eq!(messages(Click::Double, 4, false), vec![WM_LBUTTONDBLCLK]);
        assert_eq!(messages(Click::Left, 4, true), vec![WM_LBUTTONUP, NIN_SELECT]);
        assert_eq!(messages(Click::Left, 0, true), vec![WM_LBUTTONUP]);
        assert_eq!(messages(Click::Right, 4, false), vec![WM_RBUTTONDOWN, WM_RBUTTONUP, WM_CONTEXTMENU]);
        assert_eq!(messages(Click::Right, 0, false), vec![WM_RBUTTONDOWN, WM_RBUTTONUP]);
    }

    #[test]
    fn keyboard_messages() {
        assert_eq!(messages(Click::Key, 3, false), vec![NIN_KEYSELECT]);
        assert_eq!(messages(Click::Key, 4, false), vec![NIN_KEYSELECT]);
        assert_eq!(messages(Click::Key, 0, false), vec![WM_LBUTTONDOWN, WM_LBUTTONUP]);
        assert_eq!(messages(Click::KeyMenu, 4, false), vec![WM_CONTEXTMENU]);
        assert_eq!(messages(Click::KeyMenu, 0, false), vec![WM_RBUTTONDOWN, WM_RBUTTONUP]);
    }

    #[test]
    fn callback_encoding() {
        let p = POINT { x: 1900, y: -5 };
        assert_eq!(encode(0, 7, p, WM_LBUTTONUP), (7, WM_LBUTTONUP as isize));
        let (wp, lp) = encode(4, 7, p, NIN_SELECT);
        assert_eq!(wp & 0xffff, 1900);
        assert_eq!((wp >> 16) as u16 as i16, -5);
        assert_eq!(lp & 0xffff, NIN_SELECT as isize);
        assert_eq!(lp >> 16, 7);
    }

    #[test]
    fn tray_change_only_for_visible_effects() {
        use Place::*;
        // Nur Tooltip: nichts.
        assert_eq!(classify(Bar, Bar, false, true), (TrayChange::None, false));
        // Neues Bild in der Leiste: nur der Infobereich.
        assert_eq!(classify(Bar, Bar, true, true), (TrayChange::Icons, false));
        // Neues Bild im Überlauf (ausgeblendet oder kein Platz): nur der Überlauf.
        assert_eq!(classify(Overflow, Overflow, true, false), (TrayChange::None, true));
        assert_eq!(classify(Bar, Bar, true, false), (TrayChange::None, true));
        // Unsichtbares Icon: gar nichts.
        assert_eq!(classify(Gone, Gone, true, false), (TrayChange::None, false));
        // Kommt, geht oder wechselt den Platz: Aufteilung.
        assert_eq!(classify(Gone, Bar, false, true), (TrayChange::Layout, true));
        assert_eq!(classify(Bar, Gone, false, false), (TrayChange::Layout, true));
        assert_eq!(classify(Gone, Overflow, false, false), (TrayChange::Layout, true));
        assert_eq!(classify(Overflow, Bar, false, true), (TrayChange::Layout, true));
    }
}
