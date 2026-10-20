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
    Some(Path::new(&std::env::var("APPDATA").ok()?).join("rtaskbar").join("explorer-taskbar-state"))
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
    process_exe(h).is_some_and(|e| e.ends_with("rtaskbar.exe"))
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

/// Windows-Taskleiste ausblenden. Merkt sich den Originalzustand nur, wenn
/// nicht schon einer gemerkt ist (sonst würde nach einem Absturz der
/// Ausblende-Zustand als "Original" gespeichert).
pub unsafe fn takeover() {
    if let Some(f) = state_file() {
        if !f.exists() {
            let original = appbar_state(None);
            if let Some(dir) = f.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&f, original.to_string());
        }
    }
    appbar_state(Some(ABS_AUTOHIDE));
    for h in explorer_bars() {
        ShowWindow(h, SW_HIDE);
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
            ShowWindow(h, SW_HIDE);
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
    if let Some(f) = state_file() {
        let _ = std::fs::remove_file(f);
    }
    for h in explorer_bars() {
        ShowWindow(h, SW_SHOWNA);
    }
}

// --------------------------------------------------------------- Autostart

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "rtaskbar";

fn install_dir() -> Option<PathBuf> {
    Some(Path::new(&std::env::var("LOCALAPPDATA").ok()?).join("Programs").join("rtaskbar"))
}

unsafe fn set_run(value: Option<&str>) -> bool {
    let mut key: HKEY = null_mut();
    if RegOpenKeyExW(HKEY_CURRENT_USER, w16(RUN_KEY).as_ptr(), 0, KEY_SET_VALUE, &mut key) != 0 {
        return false;
    }
    let name = w16(RUN_NAME);
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

/// `--install`: exe nach %LOCALAPPDATA%\Programs\rtaskbar kopieren, Autostart
/// eintragen, starten. `--uninstall`: beenden, Autostart entfernen, Explorer
/// wiederherstellen. `--restore`: nur Explorer wiederherstellen.
/// Rückgabe `true` = Befehl erledigt, Programm soll nicht normal starten.
pub unsafe fn handle_args() -> bool {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "--install" => {
            close_running();
            let (Ok(me), Some(dir)) = (std::env::current_exe(), install_dir()) else { return true };
            let target = dir.join("rtaskbar.exe");
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
            shell_open(&path, "", SW_SHOWNORMAL);
            true
        }
        "--uninstall" => {
            close_running();
            set_run(None);
            restore();
            true
        }
        "--restore" => {
            restore();
            true
        }
        _ => false,
    }
}
