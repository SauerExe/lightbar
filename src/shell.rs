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
    let mut remembered = false;
    if let Some(f) = state_file() {
        remembered = f.exists();
        if !remembered {
            let mut original = appbar_state(None);
            // Explorers Leiste trägt unsere Markierung: Ein früherer Lauf ohne
            // gemerkten Zustand hat sie versteckt – das Autohide stammt dann von
            // uns, das Original ist "nicht ausblenden".
            let ours = explorer_bars().iter().any(|&h| !GetPropW(h, w16(HIDDEN_PROP).as_ptr()).is_null());
            if original & ABS_AUTOHIDE != 0 && ours {
                original = ABS_ALWAYSONTOP;
            }
            if let Some(dir) = f.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            remembered = std::fs::write(&f, original.to_string()).is_ok();
        }
    }
    if remembered {
        FORCED_AT.store(windows_sys::Win32::System::SystemInformation::GetTickCount64(), Ordering::Relaxed);
        appbar_state(Some(ABS_AUTOHIDE));
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

/// Zeitpunkt (GetTickCount64), zu dem `takeover` Explorer zuletzt auf
/// "automatisch ausblenden" gestellt hat.
static FORCED_AT: AtomicU64 = AtomicU64::new(0);

/// Explorers Leisten sind übernommen (versteckt, von uns auf Ausblenden gestellt).
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Hat `takeover` gerade erst (vor wenigen Sekunden) selbst auf Ausblenden
/// gestellt? Eine Änderung in der Registry stammt dann von uns, nicht vom Nutzer.
pub fn forced_recently() -> bool {
    let at = FORCED_AT.load(Ordering::Relaxed);
    at != 0 && unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }.saturating_sub(at) < 5000
}

/// "Automatisch ausblenden" im gemerkten Originalzustand – also wie der Nutzer
/// es will, solange Explorer von uns auf Ausblenden gestellt ist.
pub fn remembered_autohide() -> Option<bool> {
    let state = std::fs::read_to_string(state_file()?).ok()?.trim().parse::<u32>().ok()?;
    Some(state & ABS_AUTOHIDE != 0)
}

/// Der Nutzer hat "automatisch ausblenden" in Windows umgestellt, während
/// Lightbar läuft: als neuen Originalzustand merken. Das ist eine bewusste
/// Wahl (nicht unser eigenes Ausblenden) – `restore` stellt sie so wieder her.
pub fn remember_autohide(on: bool) {
    let Some(f) = state_file() else { return };
    let Some(old) = std::fs::read_to_string(&f).ok().and_then(|s| s.trim().parse::<u32>().ok()) else { return };
    let state = if on { old | ABS_AUTOHIDE } else { (old & !ABS_AUTOHIDE) | ABS_ALWAYSONTOP };
    if state != old {
        let _ = std::fs::write(&f, state.to_string());
    }
}

/// Originalzustand zurück und Explorers Leisten wieder zeigen.
pub unsafe fn restore() {
    ACTIVE.store(false, Ordering::Relaxed);
    let original = state_file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|s| s.trim().parse::<u32>().ok());
    if let Some(state) = original {
        appbar_state(Some(state));
    }
    for h in explorer_bars() {
        RemovePropW(h, w16(HIDDEN_PROP).as_ptr());
        ShowWindowAsync(h, SW_SHOWNA);
    }
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

/// Laufende Instanz sauber beenden (sie stellt dabei Explorer wieder her).
unsafe fn close_running() {
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
    for pid in other_instances() {
        let p = OpenProcess(windows_sys::Win32::System::Threading::PROCESS_TERMINATE, 0, pid);
        if !p.is_null() {
            crate::util::log(&format!("Prozess {pid} reagierte nicht auf Beenden – hart beendet"));
            windows_sys::Win32::System::Threading::TerminateProcess(p, 1);
            CloseHandle(p);
        }
    }
}

/// Andere laufende Lightbar-Prozesse (Leiste und Wächter), ohne diesen.
unsafe fn other_instances() -> Vec<u32> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    let me = std::process::id();
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
        .filter(|(pid, _, name)| (name == "lightbar.exe" || name == "rtaskbar.exe") && *pid != me && *pid != parent)
        .map(|p| p.0)
        .collect()
}

const TASK_NAME: &str = "Lightbar";

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
  <Actions Context="Author"><Exec><Command>{exe}</Command></Exec></Actions>
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
            restore();
            true
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
pub unsafe fn escape_package() -> bool {
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::Win32::System::WindowsProgramming::PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_OVERRIDE;
    // Schutz gegen eine Schleife, falls das Ausbrechen nicht greift.
    const MARKER: &str = "LIGHTBAR_OUTSIDE_PACKAGE";
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
    if std::env::args().nth(1).is_some_and(|a| a.starts_with("--")) {
        WaitForSingleObject(pi.hProcess, INFINITE);
    }
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    true
}
