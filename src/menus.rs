//! Kontextmenüs: Win+X (Rechtsklick auf Start), Energie, Konto.

use std::mem::zeroed;
use std::ptr::null;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Power::SetSuspendState;
use windows_sys::Win32::System::Shutdown::{ExitWindowsEx, LockWorkStation, EWX_LOGOFF};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::util::*;

pub enum Item<'a> {
    Cmd(usize, &'a str),
    /// Mit Häkchen, wenn aktiv.
    Check(usize, &'a str, bool),
    /// Ausgegraut, z. B. Überschrift mit App-Namen.
    Label(&'a str),
    Sep,
    Sub(&'a str, Vec<Item<'a>>),
}

unsafe fn build(items: &[Item]) -> HMENU {
    let menu = CreatePopupMenu();
    for item in items {
        match item {
            Item::Cmd(id, label) => {
                AppendMenuW(menu, MF_STRING, *id, w16(label).as_ptr());
            }
            Item::Check(id, label, on) => {
                let flags = MF_STRING | if *on { MF_CHECKED } else { MF_UNCHECKED };
                AppendMenuW(menu, flags, *id, w16(label).as_ptr());
            }
            Item::Label(label) => {
                AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, w16(label).as_ptr());
            }
            Item::Sep => {
                AppendMenuW(menu, MF_SEPARATOR, 0, null());
            }
            Item::Sub(label, sub) => {
                AppendMenuW(menu, MF_POPUP, build(sub) as usize, w16(label).as_ptr());
            }
        }
    }
    menu
}

static MENU_OPEN: AtomicBool = AtomicBool::new(false);

/// Solange ein Menü offen ist, darf sich die Leiste nicht in der Z-Order
/// nach vorne schieben – sonst verdeckt sie das Menü.
pub fn menu_open() -> bool {
    MENU_OPEN.load(Ordering::Relaxed)
}

/// Menü am Mauszeiger zeigen, gewählte ID zurückgeben (0 = abgebrochen).
/// Menüs der Leiste öffnen sich oberhalb der Leiste, wie bei Windows.
pub unsafe fn popup(owner: HWND, items: &[Item]) -> usize {
    let menu = build(items);
    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    // Menüs unserer Leisten (Haupt- oder Zweitleiste) öffnen auf der Seite der
    // Leiste, die zum Bildschirm zeigt – wie bei Windows.
    let mut align = TPM_BOTTOMALIGN;
    if crate::bar::is_bar(owner) {
        use crate::display::Edge;
        let mut r: RECT = zeroed();
        GetWindowRect(owner, &mut r);
        align = match crate::display::edge() {
            Edge::Bottom => {
                pt.y = r.top;
                TPM_BOTTOMALIGN
            }
            Edge::Top => {
                pt.y = r.bottom;
                TPM_TOPALIGN
            }
            Edge::Left => {
                pt.x = r.right;
                TPM_LEFTALIGN | TPM_TOPALIGN
            }
            Edge::Right => {
                pt.x = r.left;
                TPM_RIGHTALIGN | TPM_TOPALIGN
            }
        };
    }
    // Ohne Vordergrund schließt das Menü bei Klick daneben nicht.
    force_foreground(owner);
    MENU_OPEN.store(true, Ordering::Relaxed);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | align, pt.x, pt.y, 0, owner, null());
    MENU_OPEN.store(false, Ordering::Relaxed);
    PostMessageW(owner, WM_NULL, 0, 0);
    DestroyMenu(menu);
    cmd as usize
}

const P_LOCK: usize = 901;
const P_LOGOFF: usize = 902;
const P_SLEEP: usize = 903;
const P_RESTART: usize = 904;
const P_SHUTDOWN: usize = 905;

fn power_items() -> Vec<Item<'static>> {
    vec![
        Item::Cmd(P_LOCK, t!("Sperren", "Lock")),
        Item::Cmd(P_LOGOFF, t!("Abmelden", "Sign out")),
        Item::Sep,
        Item::Cmd(P_SLEEP, t!("Energie sparen", "Sleep")),
        Item::Cmd(P_RESTART, t!("Neu starten", "Restart")),
        Item::Cmd(P_SHUTDOWN, t!("Herunterfahren", "Shut down")),
    ]
}

/// Führt eine Energie-Aktion aus; `false`, wenn die ID keine war.
pub unsafe fn power_action(id: usize) -> bool {
    match id {
        P_LOCK => {
            LockWorkStation();
        }
        P_LOGOFF => {
            ExitWindowsEx(EWX_LOGOFF, 0);
        }
        P_SLEEP => {
            SetSuspendState(0, 0, 0);
        }
        P_RESTART => shell_open("shutdown.exe", "/r /t 0", SW_HIDE),
        P_SHUTDOWN => shell_open("shutdown.exe", "/s /t 0", SW_HIDE),
        _ => return false,
    }
    true
}

pub unsafe fn power_menu(owner: HWND) -> bool {
    let id = popup(owner, &power_items());
    id != 0 && power_action(id)
}

pub unsafe fn account_menu(owner: HWND) -> bool {
    let id = popup(
        owner,
        &[
            Item::Cmd(1, t!("Kontoeinstellungen ändern", "Change account settings")),
            Item::Sep,
            Item::Cmd(P_LOCK, t!("Sperren", "Lock")),
            Item::Cmd(P_LOGOFF, t!("Abmelden", "Sign out")),
        ],
    );
    match id {
        1 => shell_open("ms-settings:yourinfo", "", SW_SHOWNORMAL),
        0 => return false,
        other => {
            power_action(other);
        }
    }
    true
}

/// Terminal starten; ohne Windows Terminal auf PowerShell ausweichen.
unsafe fn terminal(admin: bool) {
    let verb = if admin { "runas" } else { "open" };
    if !shell_exec(verb, "wt.exe", "") {
        shell_exec(verb, "powershell.exe", "");
    }
}

/// Das Win+X-Menü (Rechtsklick auf Start).
pub unsafe fn winx(owner: HWND) {
    let id = popup(
        owner,
        &[
            Item::Cmd(1, t!("Installierte Apps", "Installed apps")),
            Item::Cmd(2, t!("Energieoptionen", "Power Options")),
            Item::Cmd(3, t!("Ereignisanzeige", "Event Viewer")),
            Item::Cmd(4, "System"),
            Item::Cmd(5, t!("Geräte-Manager", "Device Manager")),
            Item::Cmd(6, t!("Netzwerkverbindungen", "Network Connections")),
            Item::Cmd(7, t!("Datenträgerverwaltung", "Disk Management")),
            Item::Cmd(8, t!("Computerverwaltung", "Computer Management")),
            Item::Sep,
            Item::Cmd(9, "Terminal"),
            Item::Cmd(10, t!("Terminal (Administrator)", "Terminal (Admin)")),
            Item::Sep,
            Item::Cmd(11, t!("Task-Manager", "Task Manager")),
            Item::Cmd(12, t!("Einstellungen", "Settings")),
            Item::Cmd(13, t!("Datei-Explorer", "File Explorer")),
            Item::Cmd(14, t!("Suchen", "Search")),
            Item::Cmd(15, t!("Ausführen", "Run")),
            Item::Sep,
            Item::Sub(t!("Herunterfahren oder abmelden", "Shut down or sign out"), power_items()),
            Item::Cmd(16, "Desktop"),
        ],
    );
    match id {
        1 => shell_open("ms-settings:appsfeatures", "", SW_SHOWNORMAL),
        2 => shell_open("ms-settings:powersleep", "", SW_SHOWNORMAL),
        3 => shell_open("eventvwr.msc", "", SW_SHOWNORMAL),
        4 => shell_open("ms-settings:about", "", SW_SHOWNORMAL),
        5 => shell_open("devmgmt.msc", "", SW_SHOWNORMAL),
        6 => shell_open("ms-settings:network-status", "", SW_SHOWNORMAL),
        7 => shell_open("diskmgmt.msc", "", SW_SHOWNORMAL),
        8 => shell_open("compmgmt.msc", "", SW_SHOWNORMAL),
        9 => terminal(false),
        10 => terminal(true),
        11 => shell_open("taskmgr.exe", "", SW_SHOWNORMAL),
        12 => shell_open("ms-settings:", "", SW_SHOWNORMAL),
        13 => shell_open("explorer.exe", "", SW_SHOWNORMAL),
        14 => crate::start::toggle(true),
        15 => send_keys(&[(VK_LWIN, false), (b'R' as u16, false), (b'R' as u16, true), (VK_LWIN, true)]),
        16 => send_keys(&[(VK_LWIN, false), (b'D' as u16, false), (b'D' as u16, true), (VK_LWIN, true)]),
        other => {
            power_action(other);
        }
    }
}
