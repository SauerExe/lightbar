//! Windows-Taskleiste ausblenden/wiederherstellen und Autostart.
//!
//! Explorer läuft weiter (Desktop, Dateifenster, Arbeitsbereich). Seine
//! Leisten werden versteckt und auf "automatisch ausblenden" gestellt, damit
//! sie keinen Platz mehr reservieren. Der ursprüngliche Zustand wird in einer
//! Datei gemerkt, damit ihn auch ein späterer Start nach einem Absturz oder
//! `--restore` zurückholen kann.

use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::util::{shell_open, w16};

static ACTIVE: AtomicBool = AtomicBool::new(false);

fn state_file() -> Option<PathBuf> {
    Some(crate::util::data_dir()?.join("explorer-taskbar-state"))
}

/// Gemerkter Originalzustand (ABM_GETSTATE-Wert). `None`, wenn die Datei
/// fehlt oder unlesbar/kaputt ist.
fn read_state() -> Option<u32> {
    std::fs::read_to_string(state_file()?).ok()?.trim().parse::<u32>().ok()
}

fn write_state(state: u32) -> bool {
    let Some(f) = state_file() else { return false };
    match crate::util::write_atomic(&f, state.to_string().as_bytes()) {
        Ok(()) => true,
        Err(e) => {
            crate::util::log(&format!("Originalzustand der Windows-Taskleiste nicht speicherbar ({e})"));
            false
        }
    }
}

unsafe fn process_exe(h: HWND) -> Option<String> {
    let mut pid = 0u32;
    GetWindowThreadProcessId(h, &mut pid);
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

unsafe fn is_ours(h: HWND) -> bool {
    // Auch der frühere Name, damit ein Update die alte Version sauber ablöst.
    process_exe(h).is_some_and(|e| e.ends_with("lightbar.exe") || e.ends_with("rtaskbar.exe"))
}

/// Beim Start nach der Anmeldung kann Lightbar vor Explorer da sein. Ohne
/// Explorers Taskleiste laufen AppBar-Anmeldung und Übernahme ins Leere, und
/// Explorer zeigt seine Leiste später über unserer. Also kurz warten, bis
/// sie existiert – höchstens `max`, danach (z. B. ohne Explorer) trotzdem los.
pub unsafe fn wait_for_explorer(max: std::time::Duration) {
    let start = std::time::Instant::now();
    while explorer_bars().is_empty() && start.elapsed() < max {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Explorers Leisten: Haupt-Taskleiste (nicht unsere) und Zweitmonitor-Leisten.
unsafe fn explorer_bars() -> Vec<HWND> {
    let mut out = Vec::new();
    for class in ["Shell_TrayWnd", "Shell_SecondaryTrayWnd"] {
        let c = w16(class);
        let mut h = null_mut();
        loop {
            h = FindWindowExW(null_mut(), h, c.as_ptr(), null());
            if h.is_null() {
                break;
            }
            if !is_ours(h) {
                out.push(h);
            }
        }
    }
    out
}

/// ABM_GETSTATE / ABM_SETSTATE. Geht über Shell_TrayWnd – also über unsere
/// Leiste, die AppBar-Nachrichten an Explorer weiterreicht.
unsafe fn appbar_state(set: Option<u32>) -> u32 {
    let mut abd: APPBARDATA = std::mem::zeroed();
    abd.cbSize = size_of::<APPBARDATA>() as u32;
    match set {
        Some(state) => {
            abd.lParam = state as LPARAM;
            SHAppBarMessage(ABM_SETSTATE, &mut abd) as u32
        }
        None => SHAppBarMessage(ABM_GETSTATE, &mut abd) as u32,
    }
}

/// Markierung an Explorers Leisten: "von Lightbar versteckt". Sie bleibt am
/// Fenster, auch wenn Lightbar abstürzt, und verschwindet mit Explorer.
const HIDDEN_PROP: &str = "LightbarHidden";

unsafe fn hide_explorer_bar(h: HWND) {
    SetPropW(h, w16(HIDDEN_PROP).as_ptr(), 1 as HANDLE);
    ShowWindowAsync(h, SW_HIDE);
    // Ganz nach hinten: Apps finden die Taskleiste per FindWindow("Shell_TrayWnd"),
    // und das liefert das oberste Fenster dieser Klasse. Liegt Explorers
    // versteckte Leiste über unserer, gehen Tray-Symbole und ITaskbarList-Aufrufe
    // (Fortschritt, Zähler) an Explorer statt an uns.
    let flags = SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_ASYNCWINDOWPOS;
    SetWindowPos(h, HWND_NOTOPMOST, 0, 0, 0, 0, flags);
    SetWindowPos(h, HWND_BOTTOM, 0, 0, 0, 0, flags);
}

/// Windows-Taskleiste ausblenden. Der Originalzustand wird nur beim ersten
/// Mal gemerkt und die Datei danach nie überschrieben (erst `--uninstall`
/// entfernt sie): Liest ein späterer Start den Zustand neu, kann er unseren
/// eigenen "automatisch ausblenden" als Original erwischen – dann bliebe die
/// Windows-Leiste ohne Lightbar dauerhaft versteckt.
pub unsafe fn takeover() {
    // Ohne gemerkten Originalzustand kein "automatisch ausblenden": Den könnten
    // wir später nicht zurücknehmen, er bliebe für immer.
    // Eine kaputte Datei (z. B. abgeschnitten) zählt wie eine fehlende.
    let mut remembered = read_state().is_some();
    if let (false, Some(f)) = (remembered, state_file()) {
        if f.exists() {
            crate::util::log("Gemerkter Zustand der Windows-Taskleiste unlesbar – wird neu bestimmt");
        }
        let mut original = appbar_state(None);
        // Explorers Leiste trägt unsere Markierung: Ein früherer Lauf ohne
        // gemerkten Zustand hat sie versteckt – das Autohide stammt dann von
        // uns, das Original ist "nicht ausblenden".
        let ours = explorer_bars().iter().any(|&h| !GetPropW(h, w16(HIDDEN_PROP).as_ptr()).is_null());
        if original & ABS_AUTOHIDE != 0 && ours {
            original = ABS_ALWAYSONTOP;
        }
        remembered = write_state(original);
    }
    if remembered {
        force_autohide();
    } else {
        crate::util::log("Originalzustand der Windows-Taskleiste nicht speicherbar – kein automatisches Ausblenden");
    }
    for h in explorer_bars() {
        hide_explorer_bar(h);
    }
    ACTIVE.store(true, Ordering::Relaxed);
}

/// Im Sekundentakt: Explorer zeigt seine Leiste z. B. nach einem Neustart
/// von explorer.exe wieder an.
pub unsafe fn ensure_hidden() {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    // Auch versteckt kann Explorer seine Leiste wieder nach vorne holen – dann
    // fände FindWindow sie statt unserer.
    let first = FindWindowW(w16("Shell_TrayWnd").as_ptr(), null());
    let explorer_first = !first.is_null() && !is_ours(first);
    for h in explorer_bars() {
        if IsWindowVisible(h) != 0 || explorer_first {
            hide_explorer_bar(h);
        }
    }
}

/// Explorers Leisten sind übernommen (versteckt, von uns auf Ausblenden gestellt).
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// "Automatisch ausblenden" im gemerkten Originalzustand – also wie der Nutzer
/// es will, solange Explorer von uns auf Ausblenden gestellt ist.
pub fn remembered_autohide() -> Option<bool> {
    Some(read_state()? & ABS_AUTOHIDE != 0)
}

/// Der Nutzer hat "automatisch ausblenden" in Windows umgestellt, während
/// Lightbar läuft: als neuen Originalzustand merken. Das ist eine bewusste
/// Wahl (nicht unser eigenes Ausblenden) – `restore` stellt sie so wieder her.
/// `false`, wenn es keinen gemerkten Zustand gibt (dann ist nichts geändert).
pub fn remember_autohide(on: bool) -> bool {
    let Some(old) = read_state() else { return false };
    let state = if on { old | ABS_AUTOHIDE } else { (old & !ABS_AUTOHIDE) | ABS_ALWAYSONTOP };
    state == old || write_state(state)
}

/// Schalter "Automatisch ausblenden" im Kontextmenü der Leiste. Mit
/// übernommener Windows-Leiste wird nur der gemerkte Originalzustand geändert
/// (Explorer bleibt von uns auf Ausblenden); sonst direkt die Windows-Einstellung.
pub unsafe fn choose_autohide(on: bool) {
    if !(active() && remember_autohide(on)) {
        appbar_state(Some(if on { ABS_AUTOHIDE } else { ABS_ALWAYSONTOP }));
    }
}

/// Explorers (versteckte) Leiste auf "automatisch ausblenden" stellen, damit
/// sie keinen Platz reserviert. Nach der Übernahme und nachdem der Nutzer die
/// Einstellung abgeschaltet hat – sonst reservierte Explorers unsichtbare
/// Leiste wieder Platz. Das "an" in der Registry stammt dann von uns; solange
/// die Leiste übernommen ist, gilt deshalb nur ein "aus" als Wahl des Nutzers.
pub unsafe fn force_autohide() {
    appbar_state(Some(ABS_AUTOHIDE));
}

/// Zeitpunkt (GetTickCount64) unserer letzten eigenen "TaskbarCreated"-Rundmeldung.
static OWN_ANNOUNCE: AtomicU64 = AtomicU64::new(0);

/// Tray-Programme auffordern, ihre Symbole neu anzumelden ("TaskbarCreated").
/// Die Leiste empfängt die Meldung selbst auch; `own_announce_recent` lässt
/// sie erkennen, dass es kein Neustart von Explorer war.
pub unsafe fn announce_tray() {
    OWN_ANNOUNCE.store(windows_sys::Win32::System::SystemInformation::GetTickCount64(), Ordering::Relaxed);
    crate::tray::announce();
}

/// Kam "TaskbarCreated" eben (vor höchstens 2 s) von uns selbst?
pub fn own_announce_recent() -> bool {
    let at = OWN_ANNOUNCE.load(Ordering::Relaxed);
    at != 0 && unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }.saturating_sub(at) < 2000
}

/// Originalzustand zurück und Explorers Leisten wieder zeigen.
/// Auch aus dem Wächter und mit `--restore` aufgerufen (nach einem Absturz):
/// Dann holt erst die "TaskbarCreated"-Meldung die Tray-Symbole zu Explorer
/// zurück. Mehrfaches Melden schadet nicht.
pub unsafe fn restore() {
    ACTIVE.store(false, Ordering::Relaxed);
    if let Some(state) = read_state() {
        appbar_state(Some(state));
    }
    for h in explorer_bars() {
        RemovePropW(h, w16(HIDDEN_PROP).as_ptr());
        ShowWindowAsync(h, SW_SHOWNA);
        // `hide_explorer_bar` hat sie ganz nach hinten gelegt – wieder nach vorn.
        SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
    }
    announce_tray();
}

// --------------------------------------------------------------- Autostart

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "Lightbar";
/// Autostart-Eintrag unter dem früheren Namen (wird beim Update entfernt).
const LEGACY_RUN_NAME: &str = "rtaskbar";

fn install_dir() -> Option<PathBuf> {
    Some(Path::new(&std::env::var("LOCALAPPDATA").ok()?).join("Programs").join("Lightbar"))
}

unsafe fn set_run(value: Option<&str>) -> bool {
    set_run_named(RUN_NAME, value)
}

unsafe fn set_run_named(name: &str, value: Option<&str>) -> bool {
    let mut key: HKEY = null_mut();
    if RegOpenKeyExW(HKEY_CURRENT_USER, w16(RUN_KEY).as_ptr(), 0, KEY_SET_VALUE, &mut key) != 0 {
        return false;
    }
    let name = w16(name);
    let ok = match value {
        Some(v) => {
            let data = w16(v);
            RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, (data.len() * 2) as u32) == 0
        }
        None => {
            let r = RegDeleteValueW(key, name.as_ptr());
            r == 0 || r == ERROR_FILE_NOT_FOUND
        }
    };
    RegCloseKey(key);
    ok
}

/// Benanntes Ereignis "kein Neustart": `--install`/`--uninstall`/`--restore`
/// setzen es, bevor sie die laufende Instanz beenden. Der Wächter prüft es,
/// bevor er neu startet – sonst holte er eine Instanz, die wir gerade hart
/// beenden mussten, sofort zurück.
fn no_restart_event_name() -> Vec<u16> {
    w16("Local\\lightbar-no-restart")
}

/// Für den Wächter: Läuft gerade ein Befehl, der Lightbar bewusst beendet?
pub unsafe fn no_restart_requested() -> bool {
    use windows_sys::Win32::System::Threading::{OpenEventW, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE};
    let h = OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, no_restart_event_name().as_ptr());
    if h.is_null() {
        return false;
    }
    let set = WaitForSingleObject(h, 0) == WAIT_OBJECT_0;
    CloseHandle(h);
    set
}

/// Laufende Instanz sauber beenden (sie stellt dabei Explorer wieder her).
unsafe fn close_running() {
    use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE};
    // Manuell zurückzusetzen und gesetzt: gilt, solange wir es offen halten.
    let no_restart = CreateEventW(null(), 1, 1, no_restart_event_name().as_ptr());
    if !no_restart.is_null() {
        SetEvent(no_restart);
    }
    let c = w16("Shell_TrayWnd");
    let mut h = null_mut();
    loop {
        h = FindWindowExW(null_mut(), h, c.as_ptr(), null());
        if h.is_null() {
            break;
        }
        if is_ours(h) {
            let mut r = 0usize;
            SendMessageTimeoutW(h, WM_CLOSE, 0, 0, SMTO_ABORTIFHUNG, 5000, &mut r);
        }
    }
    // Warten, bis wirklich alle anderen Lightbar-Prozesse weg sind – auch der
    // Wächter, der sonst die exe sperrt (--install kopiert dann nicht) oder
    // eine hängende Instanz nach --uninstall wieder hochholt. Wer nach 8 s noch
    // läuft, wird beendet.
    let start = std::time::Instant::now();
    while !other_instances().is_empty() && start.elapsed() < std::time::Duration::from_secs(8) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    // Ein Wächter kann zwischen Schnappschuss und Beenden noch eine Instanz
    // gestartet haben: in ein paar Runden, bis keine mehr übrig ist.
    for _ in 0..5 {
        let pids = other_instances();
        if pids.is_empty() {
            break;
        }
        for pid in pids {
            let p = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid);
            if !p.is_null() {
                crate::util::log(&format!("Prozess {pid} reagierte nicht auf Beenden – hart beendet"));
                TerminateProcess(p, 1);
                WaitForSingleObject(p, 2000);
                CloseHandle(p);
            }
        }
    }
    if !other_instances().is_empty() {
        crate::util::log("Andere Lightbar-Prozesse ließen sich nicht beenden");
    }
    if !no_restart.is_null() {
        CloseHandle(no_restart);
    }
}

/// Andere laufende Lightbar-Prozesse (Leiste und Wächter) in dieser Sitzung,
/// ohne diesen. Lightbar anderer angemeldeter Benutzer (schneller
/// Benutzerwechsel, RDP) bleibt unberührt.
unsafe fn other_instances() -> Vec<u32> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let me = std::process::id();
    let mut my_session = 0u32;
    if ProcessIdToSessionId(me, &mut my_session) == 0 {
        return Vec::new();
    }
    let same_session = |pid: u32| {
        let mut s = 0u32;
        ProcessIdToSessionId(pid, &mut s) != 0 && s == my_session
    };
    let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if snap == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut all: Vec<(u32, u32, String)> = Vec::new();
    let mut e: PROCESSENTRY32W = std::mem::zeroed();
    e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut ok = Process32FirstW(snap, &mut e) != 0;
    while ok {
        let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
        all.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..n]).to_lowercase()));
        ok = Process32NextW(snap, &mut e) != 0;
    }
    CloseHandle(snap);
    // Den eigenen Elternprozess auslassen: Beim Ausbruch aus einem
    // Paket-Container wartet er auf genau diesen Befehl.
    let parent = all.iter().find(|p| p.0 == me).map(|p| p.1).unwrap_or(0);
    all.into_iter()
        .filter(|(pid, _, name)| (name == "lightbar.exe" || name == "rtaskbar.exe") && *pid != me && *pid != parent && same_session(*pid))
        .map(|p| p.0)
        .collect()
}

const TASK_NAME: &str = "Lightbar";
/// Argument der Anmelde-Aufgabe: Daran erkennt der Start, dass er aus dem
/// Autostart kommt (ältere Aufgaben ohne Argument starten wie bisher).
const AUTOSTART_ARG: &str = "--autostart";
const STARTUP_APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// Hat der Nutzer Lightbar im Task-Manager unter "Autostart-Apps"
/// deaktiviert? Windows vermerkt das im ersten Byte des Werts unter
/// `StartupApproved\Run` (gerade = aktiviert, ungerade = deaktiviert). Die
/// Aufgabenplanung kennt diese Liste nicht, also prüfen wir selbst.
unsafe fn startup_disabled() -> bool {
    let mut buf = [0u8; 12];
    let mut size = buf.len() as u32;
    let ok = RegGetValueW(
        HKEY_CURRENT_USER,
        w16(STARTUP_APPROVED_KEY).as_ptr(),
        w16(RUN_NAME).as_ptr(),
        RRF_RT_REG_BINARY,
        null_mut(),
        buf.as_mut_ptr() as _,
        &mut size,
    );
    ok == 0 && size >= 1 && startup_byte_disabled(buf[0])
}

fn startup_byte_disabled(b: u8) -> bool {
    b & 1 != 0
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// schtasks ohne Konsolenfenster; `true` bei Erfolg.
fn schtasks(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|s| s.success())
}

/// Autostart über die Aufgabenplanung (bei Anmeldung dieses Benutzers).
/// Zuverlässiger als der Run-Schlüssel: Den arbeitet Explorer selbst ab und
/// hat Lightbar dort auf manchen Systemen ohne Meldung übersprungen. Startet
/// außerdem früher. Ein Anmelde-Auslöser nur für den eigenen Benutzer braucht
/// keine Administratorrechte (anders als `schtasks /sc onlogon`).
fn set_logon_task(exe: Option<&str>) -> bool {
    let Some(exe) = exe else {
        return schtasks(&["/delete", "/tn", TASK_NAME, "/f"]);
    };
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) => format!("{d}\\{u}"),
        _ => return false,
    };
    let (user, exe) = (xml_escape(&user), xml_escape(exe));
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Lightbar – Taskleiste bei der Anmeldung starten</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>{AUTOSTART_ARG}</Arguments></Exec></Actions>
</Task>
"#
    );
    let file = std::env::temp_dir().join("lightbar-task.xml");
    // schtasks erwartet UTF-16 mit BOM.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(|c| c.to_le_bytes()));
    if std::fs::write(&file, bytes).is_err() {
        return false;
    }
    let ok = schtasks(&["/create", "/tn", TASK_NAME, "/xml", &file.to_string_lossy(), "/f"]);
    let _ = std::fs::remove_file(&file);
    ok
}

/// `--install`: exe nach %LOCALAPPDATA%\Programs\Lightbar kopieren, Autostart
/// eintragen, starten. `--uninstall`: beenden, Autostart entfernen, Explorer
/// wiederherstellen. `--restore`: nur Explorer wiederherstellen.
/// Rückgabe `true` = Befehl erledigt, Programm soll nicht normal starten.
pub unsafe fn handle_args() -> bool {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "--install" => {
            close_running();
            let (Ok(me), Some(dir)) = (std::env::current_exe(), install_dir()) else { return true };
            let target = dir.join("lightbar.exe");
            let _ = std::fs::create_dir_all(&dir);
            // Kurz warten, bis die alte Instanz die Datei freigibt.
            for _ in 0..20 {
                if me == target || std::fs::copy(&me, &target).is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            let path = target.to_string_lossy().into_owned();
            let run = set_run(Some(&format!("\"{path}\"")));
            // Run-Schlüssel bleibt als Rückfallebene; doppelt gestartet beendet
            // sich die zweite Instanz sofort (Mutex).
            let task = set_logon_task(Some(&path));
            crate::util::log(&format!("Installiert (Run-Eintrag: {run}, Aufgabe: {task}, im Paket-Container: {})", in_package()));
            // Umzug vom früheren Namen: alten Autostart und Programmordner entfernen.
            set_run_named(LEGACY_RUN_NAME, None);
            if let Ok(local) = std::env::var("LOCALAPPDATA") {
                // Die alte exe ist evtl. noch kurz gesperrt, während sie sich beendet.
                let old = Path::new(&local).join("Programs").join("rtaskbar");
                for _ in 0..20 {
                    if !old.exists() || std::fs::remove_dir_all(&old).is_ok() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
            shell_open(&path, "", SW_SHOWNORMAL);
            true
        }
        "--uninstall" => {
            close_running();
            set_run(None);
            set_logon_task(None);
            set_run_named(LEGACY_RUN_NAME, None);
            restore();
            // Erst jetzt: Der gemerkte Originalzustand wird nicht mehr gebraucht.
            if let Some(f) = state_file() {
                let _ = std::fs::remove_file(f);
            }
            true
        }
        "--restore" => {
            close_running();
            restore();
            true
        }
        // Start über die Anmelde-Aufgabe: im Task-Manager abgeschaltet → nicht starten.
        AUTOSTART_ARG => {
            if startup_disabled() {
                crate::util::log("Autostart im Task-Manager deaktiviert – Start über die Aufgabe übersprungen");
                return true;
            }
            false
        }
        _ => false,
    }
}

// ------------------------------------------------------ Paket-Container

/// Läuft dieser Prozess mit Paket-Identität (MSIX-Container)?
///
/// Das passiert, wenn Lightbar aus einer paketierten App heraus gestartet
/// wird, z. B. aus deren Terminal. Windows leitet dann Schreibzugriffe auf
/// `%APPDATA%`, `%LOCALAPPDATA%` und HKCU in eine private Kopie des Pakets um:
/// Einstellungen, Protokoll und sogar der Autostart-Eintrag landen dort, wo
/// sie außer diesem Container niemand sieht (Explorer hat den Run-Eintrag so
/// nie zu Gesicht bekommen), und gelesen werden womöglich veraltete Werte.
unsafe fn in_package() -> bool {
    use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
    const APPMODEL_ERROR_NO_PACKAGE: u32 = 15700;
    let mut len = 0u32;
    GetCurrentPackageFullName(&mut len, null_mut()) != APPMODEL_ERROR_NO_PACKAGE
}

/// Im Container: sich selbst mit denselben Argumenten außerhalb neu starten
/// (dokumentierte Ausnahme `PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_OVERRIDE`).
/// `true` = neu gestartet, dieser Prozess soll enden. Befehle (`--install`
/// usw.) wartet er noch ab, damit sie wie gewohnt fertig sind, wenn er endet.
/// Merker "schon aus dem Container ausgebrochen" (Schutz gegen eine Schleife).
/// `main` entfernt ihn danach, damit von Lightbar gestartete Programme ihn nicht erben.
pub const MARKER: &str = "LIGHTBAR_OUTSIDE_PACKAGE";

pub unsafe fn escape_package() -> bool {
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::Win32::System::WindowsProgramming::PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_OVERRIDE;
    if !in_package() || std::env::var_os(MARKER).is_some() {
        return false;
    }
    std::env::set_var(MARKER, "1");
    let mut size = 0usize;
    InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
    let mut buf = vec![0u8; size];
    let list = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    if InitializeProcThreadAttributeList(list, 1, 0, &mut size) == 0 {
        return false;
    }
    let mut policy: u32 = PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_OVERRIDE;
    let ok = UpdateProcThreadAttribute(
        list,
        0,
        PROC_THREAD_ATTRIBUTE_DESKTOP_APP_POLICY as usize,
        &mut policy as *mut u32 as *const _,
        size_of::<u32>(),
        null_mut(),
        null(),
    ) != 0;
    let mut started = false;
    let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
    if ok {
        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = list;
        // CreateProcessW darf die Befehlszeile verändern: eigene Kopie.
        let cmd = windows_sys::Win32::System::Environment::GetCommandLineW();
        let len = (0..).take_while(|&i| *cmd.add(i) != 0).count();
        let mut line: Vec<u16> = std::slice::from_raw_parts(cmd, len).to_vec();
        line.push(0);
        started = CreateProcessW(null(), line.as_mut_ptr(), null(), null(), 0, EXTENDED_STARTUPINFO_PRESENT, null(), null(), &si.StartupInfo, &mut pi) != 0;
    }
    DeleteProcThreadAttributeList(list);
    if !started {
        return false;
    }
    // Befehle abwarten – den normalen Start (auch aus dem Autostart) nicht.
    if std::env::args().nth(1).is_some_and(|a| a.starts_with("--") && a != AUTOSTART_ARG) {
        WaitForSingleObject(pi.hProcess, INFINITE);
    }
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn startup_approved_first_byte() {
        // 02 = aktiviert, 03 = im Task-Manager deaktiviert (06/07 bei manchen Builds).
        assert!(!super::startup_byte_disabled(2));
        assert!(super::startup_byte_disabled(3));
        assert!(!super::startup_byte_disabled(6));
        assert!(super::startup_byte_disabled(7));
    }
}
