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
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Windows-Taskleiste ausblenden. Der Originalzustand wird nur beim ersten
/// Mal gemerkt und die Datei danach nie überschrieben (erst `--uninstall`
/// entfernt sie): Liest ein späterer Start den Zustand neu, kann er unseren
/// eigenen "automatisch ausblenden" als Original erwischen – dann bliebe die
/// Windows-Leiste ohne Lightbar dauerhaft versteckt.
pub unsafe fn takeover() {
    if let Some(f) = state_file() {
        if !f.exists() {
            let mut original = appbar_state(None);
            // Explorers Leiste ist schon versteckt: Das war Lightbar (ein früherer
            // Lauf ohne gemerkten Zustand). Dann ist "nicht ausblenden" das Original.
            if original & ABS_AUTOHIDE != 0 && explorer_bars().iter().any(|&h| IsWindowVisible(h) == 0) {
                original = ABS_ALWAYSONTOP;
            }
            if let Some(dir) = f.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&f, original.to_string());
        }
    }
    appbar_state(Some(ABS_AUTOHIDE));
    for h in explorer_bars() {
        ShowWindowAsync(h, SW_HIDE);
    }
    ACTIVE.store(true, Ordering::Relaxed);
}

/// Im Sekundentakt: Explorer zeigt seine Leiste z. B. nach einem Neustart
/// von explorer.exe wieder an.
pub unsafe fn ensure_hidden() {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    for h in explorer_bars() {
        if IsWindowVisible(h) != 0 {
            ShowWindowAsync(h, SW_HIDE);
        }
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
            set_run(Some(&format!("\"{path}\"")));
            // Run-Schlüssel bleibt als Rückfallebene; doppelt gestartet beendet
            // sich die zweite Instanz sofort (Mutex).
            set_logon_task(Some(&path));
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
