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
//! - bewacht auch die Instanz, die er neu startet (keine Lücke beim Hochfahren),
//! - wartet, wenn die neue Instanz mit Code 2 endet (Mutex noch von der alten
//!   belegt), bis die alte wirklich weg ist, und versucht es dann erneut,
//! - startet nicht neu, solange `--install`/`--uninstall`/`--restore` Lightbar
//!   bewusst beenden (`shell::no_restart_requested`),
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
/// Höchstens so viele Neustarts innerhalb von `RESTART_WINDOW` Sekunden. Ein
/// Neustart nach einem Hänger dauert schon gut 90 s – das Fenster muss deutlich
/// länger sein, sonst greift die Bremse bei Dauerhängern nie.
const MAX_RESTARTS: usize = 3;
const RESTART_WINDOW: u64 = 600;

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
    let _ = crate::util::write_atomic(&path, recent.iter().map(|x| format!("{x}\n")).collect::<String>().as_bytes());
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

/// Umgebungsvariable für eine vom Wächter neu gestartete Instanz: Sie wird
/// schon bewacht und startet keinen eigenen Wächter.
pub const WATCHED: &str = "LIGHTBAR_WATCHED";

/// Exit-Code einer Instanz, die den Einzelinstanz-Mutex nicht bekam (siehe `main`).
pub const EXIT_MUTEX_BUSY: i32 = 2;
const MUTEX_NAME: &str = "lightbar-single-instance";

/// Ausgang einer bewachten Instanz.
enum Outcome {
    /// Regulär beendet (Exit-Code 0).
    Clean,
    /// Mutex noch belegt – meist hält ihn die alte, gerade beendete Instanz noch.
    MutexBusy,
    /// Abgestürzt, hart beendet oder hängend: der Grund.
    Failed(String),
}

/// Gibt es den Einzelinstanz-Mutex gerade (hält ihn also ein Prozess)?
unsafe fn mutex_held() -> bool {
    let h = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, w16(MUTEX_NAME).as_ptr());
    if h.is_null() {
        return false;
    }
    CloseHandle(h);
    true
}

unsafe fn watch(proc: HANDLE, pid: u32) -> Outcome {
    let alive = CreateEventW(null(), 0, 0, alive_event_name(pid).as_ptr());
    let mut silent = 0u32;
    let exited = |proc: HANDLE| {
        let mut code = 0u32;
        GetExitCodeProcess(proc, &mut code);
        match code {
            0 => Outcome::Clean,
            c if c == EXIT_MUTEX_BUSY as u32 => Outcome::MutexBusy,
            c => Outcome::Failed(format!("Lightbar unerwartet beendet (Code 0x{c:X})")),
        }
    };
    let outcome = loop {
        if WaitForSingleObject(proc, 10_000) == WAIT_OBJECT_0 {
            break exited(proc);
        }
        // Noch keine Leiste (Start wartet evtl. auf Explorer): nichts zu prüfen.
        let bar = bar_of(pid);
        if bar.is_null() || alive.is_null() {
            continue;
        }
        PostMessageW(bar, WM_BAR_PING, 0, 0);
        // Auf die Antwort warten – oder darauf, dass der Prozess endet (dann
        // sofort reagieren, z. B. damit --install die exe nicht gesperrt findet).
        let handles = [alive, proc];
        match WaitForMultipleObjects(2, handles.as_ptr(), 0, 5_000) {
            WAIT_OBJECT_0 => silent = 0,
            w if w == WAIT_OBJECT_0 + 1 => break exited(proc),
            _ => {
                silent += 1;
                if silent >= MAX_SILENT {
                    TerminateProcess(proc, 0xDEAD);
                    WaitForSingleObject(proc, 5_000);
                    break Outcome::Failed(format!("Leiste reagiert seit {} s nicht", silent * 15));
                }
            }
        }
    };
    if !alive.is_null() {
        CloseHandle(alive);
    }
    outcome
}

/// `--watchdog <pid>`: bewacht die Instanz und jede, die er selbst neu
/// startet, bis eine regulär endet oder er aufgibt.
pub unsafe fn run(pid: u32) {
    use std::os::windows::io::IntoRawHandle;
    let mut proc = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE, 0, pid);
    let mut pid = pid;
    // Die zuvor bewachte Instanz: Startet die neue zu früh, hält die alte den
    // Mutex evtl. noch (hart beendet, räumt noch auf) – dann auf sie warten.
    let mut prev: HANDLE = null_mut();
    let close = |h: HANDLE| {
        if !h.is_null() {
            CloseHandle(h);
        }
    };
    while !proc.is_null() {
        let reason = match watch(proc, pid) {
            Outcome::Clean => {
                close(proc);
                close(prev);
                return;
            }
            Outcome::MutexBusy => {
                if !prev.is_null() && WaitForSingleObject(prev, OLD_EXIT_WAIT_MS) != WAIT_OBJECT_0 {
                    log("Wächter: vorige Instanz endet nicht – Windows-Taskleiste wird wiederhergestellt");
                    crate::shell::restore();
                    return;
                }
                // Die vorige ist weg und der Mutex trotzdem belegt: Eine andere
                // Instanz (mit eigenem Wächter) läuft – nichts mehr zu tun.
                if mutex_held() {
                    log("Wächter: eine andere Lightbar-Instanz läuft bereits – Wächter endet");
                    close(proc);
                    close(prev);
                    return;
                }
                "Neustart traf noch auf die vorige Instanz".to_string()
            }
            Outcome::Failed(reason) => reason,
        };
        close(prev);
        prev = proc;
        // Beim Abmelden/Herunterfahren beendet Windows Lightbar selbst.
        if GetSystemMetrics(SM_SHUTTINGDOWN) != 0 {
            log(&format!("Wächter: {reason} – System fährt herunter, kein Neustart"));
            return;
        }
        // --install/--uninstall/--restore beenden Lightbar gerade bewusst.
        if crate::shell::no_restart_requested() {
            log(&format!("Wächter: {reason} – Beenden durch Installation/Wiederherstellung, kein Neustart"));
            return;
        }
        if !allow_restart() {
            log(&format!("Wächter: {reason} – zu viele Neustarts, Windows-Taskleiste wird wiederhergestellt"));
            crate::shell::restore();
            return;
        }
        log(&format!("Wächter: {reason} – starte Lightbar neu"));
        let child = std::env::current_exe().and_then(|me| std::process::Command::new(me).env(WATCHED, "1").spawn());
        let Ok(child) = child else {
            log("Wächter: Neustart fehlgeschlagen, Windows-Taskleiste wird wiederhergestellt");
            crate::shell::restore();
            return;
        };
        pid = child.id();
        // Das eigene Handle des Kinds (volle Rechte) statt neu per PID öffnen:
        // Endet es sehr schnell, könnte die PID schon einem anderen Prozess gehören.
        proc = child.into_raw_handle() as HANDLE;
    }
}

/// So lange wartet der Wächter höchstens darauf, dass die vorige Instanz den
/// Mutex freigibt (5 min).
const OLD_EXIT_WAIT_MS: u32 = 300_000;
