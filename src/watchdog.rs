//! Wächter: ein zweiter Lightbar-Prozess ohne Oberfläche (`--watchdog <pid>`).
//!
//! Solange Lightbar läuft, ist Explorers Taskleiste versteckt. Stürzt Lightbar
//! ab oder hängt es, stünde man ohne Taskleiste da. Der Wächter
//!
//! - startet Lightbar neu, wenn es unerwartet endet (Exit-Code ≠ 0; ein
//!   reguläres Beenden endet mit 0),
//! - erkennt einen Hänger: Er schickt der Leiste regelmäßig eine eingereihte
//!   Nachricht, auf die sie ein benanntes Ereignis setzt. Hängt ihr Thread in
//!   einem blockierenden Aufruf, kommt die Nachricht nie an – direkt gesendete
//!   Nachrichten (`WM_NULL`) würden dagegen weiter beantwortet,
//! - gibt nach zu vielen Neustarts kurz hintereinander auf und holt die
//!   Windows-Taskleiste zurück.

use std::ptr::{null, null_mut};
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::util::{log, w16};

/// Eingereihte Nachricht an die Leiste: "Lebst du noch?"
pub const WM_BAR_PING: u32 = WM_APP + 38;

/// So lange (in Ping-Runden à 10 s) darf die Leiste schweigen, bevor sie als
/// hängend gilt.
const MAX_SILENT: u32 = 6;
/// Höchstens so viele Neustarts innerhalb von `RESTART_WINDOW` Sekunden.
const MAX_RESTARTS: usize = 3;
const RESTART_WINDOW: u64 = 120;

fn alive_event_name(pid: u32) -> Vec<u16> {
    w16(&format!("Local\\lightbar-alive-{pid}"))
}

/// Von der Leiste aufgerufen, wenn der Ping ankommt.
pub unsafe fn pong() {
    let h = OpenEventW(EVENT_MODIFY_STATE, 0, alive_event_name(GetCurrentProcessId()).as_ptr());
    if !h.is_null() {
        SetEvent(h);
        CloseHandle(h);
    }
}

/// Aus dem normalen Start heraus: Wächter für diesen Prozess starten. Er
/// verlässt das Job-Objekt der Aufgabenplanung, damit er nicht mit der
/// Aufgabe endet.
pub fn spawn() {
    use std::os::windows::process::CommandExt;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let Ok(me) = std::env::current_exe() else { return };
    let pid = std::process::id().to_string();
    let start = |flags| std::process::Command::new(&me).args(["--watchdog", &pid]).creation_flags(flags).spawn();
    if start(CREATE_BREAKAWAY_FROM_JOB).or_else(|_| start(0)).is_err() {
        log("Wächter konnte nicht gestartet werden");
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn restarts_path() -> Option<std::path::PathBuf> {
    Some(crate::util::data_dir()?.join("restarts.txt"))
}

/// Neustart vermerken; `false`, wenn es zu viele in kurzer Zeit waren.
fn allow_restart() -> bool {
    let Some(path) = restarts_path() else { return true };
    let t = now();
    let mut recent: Vec<u64> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .filter(|&x: &u64| t.saturating_sub(x) < RESTART_WINDOW)
        .collect();
    if recent.len() >= MAX_RESTARTS {
        return false;
    }
    recent.push(t);
    let _ = std::fs::write(&path, recent.iter().map(|x| format!("{x}\n")).collect::<String>());
    true
}

unsafe fn bar_of(pid: u32) -> HWND {
    let class = w16("Shell_TrayWnd");
    let mut h = null_mut();
    loop {
        h = FindWindowExW(null_mut(), h, class.as_ptr(), null());
        if h.is_null() {
            return h;
        }
        let mut owner = 0u32;
        GetWindowThreadProcessId(h, &mut owner);
        if owner == pid {
            return h;
        }
    }
}

unsafe fn restart_or_give_up(reason: &str) {
    // Beim Abmelden/Herunterfahren beendet Windows Lightbar selbst.
    if GetSystemMetrics(SM_SHUTTINGDOWN) != 0 {
        log(&format!("Wächter: {reason} – System fährt herunter, kein Neustart"));
        return;
    }
    if allow_restart() {
        log(&format!("Wächter: {reason} – starte Lightbar neu"));
        if let Ok(me) = std::env::current_exe() {
            let _ = std::process::Command::new(me).spawn();
        }
    } else {
        log(&format!("Wächter: {reason} – zu viele Neustarts, Windows-Taskleiste wird wiederhergestellt"));
        crate::shell::restore();
    }
}

/// `--watchdog <pid>`: läuft, bis Lightbar endet.
pub unsafe fn run(pid: u32) {
    let proc = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE, 0, pid);
    if proc.is_null() {
        return;
    }
    let alive = CreateEventW(null(), 0, 0, alive_event_name(pid).as_ptr());
    let mut silent = 0u32;
    loop {
        if WaitForSingleObject(proc, 10_000) == WAIT_OBJECT_0 {
            let mut code = 0u32;
            GetExitCodeProcess(proc, &mut code);
            if code != 0 {
                restart_or_give_up(&format!("Lightbar unerwartet beendet (Code 0x{code:X})"));
            }
            break;
        }
        // Noch keine Leiste (Start wartet evtl. auf Explorer): nichts zu prüfen.
        let bar = bar_of(pid);
        if bar.is_null() || alive.is_null() {
            continue;
        }
        PostMessageW(bar, WM_BAR_PING, 0, 0);
        if WaitForSingleObject(alive, 5_000) == WAIT_OBJECT_0 {
            silent = 0;
            continue;
        }
        silent += 1;
        if silent >= MAX_SILENT {
            TerminateProcess(proc, 0xDEAD);
            WaitForSingleObject(proc, 5_000);
            restart_or_give_up(&format!("Leiste reagiert seit {} s nicht", silent * 15));
            break;
        }
    }
    if !alive.is_null() {
        CloseHandle(alive);
    }
    CloseHandle(proc);
}
