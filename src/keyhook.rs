//! Win-Taste übernehmen: Low-Level-Tastaturhook auf eigenem Thread.
//!
//! Win allein öffnet unser Startmenü, Win+S die Suche, Win+X das Kontextmenü,
//! Win+A die Schnelleinstellungen, Win+T/Win+B den Tastaturfokus in der
//! Leiste bzw. im Infobereich, Win+1…9/0 die Taskleisten-Apps (mit Shift
//! neue Instanz, mit Strg letztes Fenster, mit Alt Sprungliste). Andere
//! Kombinationen wie Win+E oder Win+Shift+S laufen unverändert an Windows
//! durch. Damit Windows beim Loslassen nicht zusätzlich sein eigenes
//! Startmenü öffnet, schicken wir vorher eine unbelegte Maskierungstaste (0xE8) – Windows sieht dann eine Kombination.
//!
//! Der Hook läuft auf einem eigenen Thread, damit er nie hinter langsamer
//! UI-Arbeit wartet (Windows entfernt Hooks, die zu oft zu lange brauchen).

use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::{bar, start};
use crate::util::send_keys;

const VK_MASK: VIRTUAL_KEY = 0xE8;
const VK_KEY_S: u16 = b'S' as u16;
const VK_KEY_X: u16 = b'X' as u16;
const VK_KEY_A: u16 = b'A' as u16;
const VK_KEY_T: u16 = b'T' as u16;
const VK_KEY_B: u16 = b'B' as u16;

static WIN_DOWN: AtomicBool = AtomicBool::new(false);
static COMBO: AtomicBool = AtomicBool::new(false);
/// Eine Kombi-Taste wurde von uns geschluckt – Windows hat dann nur Win
/// gesehen und würde beim Loslassen sein Startmenü öffnen.
static SWALLOWED: AtomicBool = AtomicBool::new(false);
/// Gerade gehaltene, geschluckte Kombi-Taste (0 = keine) – gegen Tastenwiederholung.
static SWALLOWED_VK: AtomicU16 = AtomicU16::new(0);

/// Tatsächlicher Zustand vor dem aktuellen Ereignis (im LL-Hook verlässlich).
unsafe fn win_physically_down() -> bool {
    (GetAsyncKeyState(VK_LWIN as i32) as u16 & 0x8000) != 0 || (GetAsyncKeyState(VK_RWIN as i32) as u16 & 0x8000) != 0
}

/// Gedrückte Zusatztasten neben Win: (Shift, Strg, Alt).
unsafe fn modifiers() -> (bool, bool, bool) {
    let down = |k: VIRTUAL_KEY| GetAsyncKeyState(k as i32) as u16 & 0x8000 != 0;
    (down(VK_SHIFT), down(VK_CONTROL), down(VK_MENU))
}

/// Win+Taste → (Zielfenster, Nachricht, wParam), `None` = an Windows durchlassen.
unsafe fn shortcut(vk: u16, mods: (bool, bool, bool)) -> Option<(HWND, u32, usize)> {
    Some(match action(vk, mods)? {
        Action::Search => (start::hwnd(), start::WM_TOGGLE, 1),
        Action::WinX => (bar::hwnd(), bar::WM_BAR_WINX, 0),
        Action::Quick => (bar::hwnd(), bar::WM_BAR_QUICK, 0),
        Action::FocusTasks => (bar::hwnd(), bar::WM_BAR_FOCUS, 0),
        Action::FocusTray => (bar::hwnd(), bar::WM_BAR_TRAYFOCUS, 0),
        Action::App(n) => (bar::hwnd(), bar::WM_BAR_NUM, n),
        Action::NewInstance(n) => (bar::hwnd(), bar::WM_BAR_NUM_NEW, n),
        Action::LastWindow(n) => (bar::hwnd(), bar::WM_BAR_NUM_LAST, n),
        Action::JumpList(n) => (bar::hwnd(), bar::WM_BAR_NUM_JUMP, n),
    })
}

#[derive(Debug, PartialEq)]
enum Action {
    Search,
    WinX,
    Quick,
    FocusTasks,
    FocusTray,
    App(usize),
    NewInstance(usize),
    LastWindow(usize),
    JumpList(usize),
}

/// Welche Win-Kombination wir selbst bedienen (rein, testbar). Zahlen 1…9 und
/// 0 (= zehnte App) wie bei Windows, mit genau einer Zusatztaste:
/// Shift = neue Instanz, Strg = letztes aktives Fenster, Alt = Sprungliste.
fn action(vk: u16, (shift, ctrl, alt): (bool, bool, bool)) -> Option<Action> {
    let nth = match vk {
        0x31..=0x39 => Some((vk - 0x31) as usize),
        0x30 => Some(9),
        _ => None,
    };
    match (shift, ctrl, alt) {
        (false, false, false) => Some(match vk {
            VK_KEY_S => Action::Search,
            VK_KEY_X => Action::WinX,
            VK_KEY_A => Action::Quick,
            VK_KEY_T => Action::FocusTasks,
            VK_KEY_B => Action::FocusTray,
            _ => Action::App(nth?),
        }),
        (true, false, false) => Some(Action::NewInstance(nth?)),
        (false, true, false) => Some(Action::LastWindow(nth?)),
        (false, false, true) => Some(Action::JumpList(nth?)),
        _ => None,
    }
}

/// Kann das Loslassen dieser Taste das Tastaturlayout umgeschaltet haben?
/// Win+Leertaste, Alt+Umschalt, Strg+Umschalt (Windows-Standardkürzel).
fn layout_hotkey(vk: u16, win: bool, shift: bool, ctrl: bool, alt: bool) -> bool {
    let is_shift = matches!(vk, VK_SHIFT | VK_LSHIFT | VK_RSHIFT);
    let is_alt = matches!(vk, VK_MENU | VK_LMENU | VK_RMENU);
    let is_ctrl = matches!(vk, VK_CONTROL | VK_LCONTROL | VK_RCONTROL);
    (vk == VK_SPACE && win) || (is_shift && (alt || ctrl)) || ((is_alt || is_ctrl) && shift)
}

unsafe extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = &*(lp as *const KBDLLHOOKSTRUCT);
        let injected = k.flags & LLKHF_INJECTED != 0;
        let down = wp as u32 == WM_KEYDOWN || wp as u32 == WM_SYSKEYDOWN;
        let vk = k.vkCode as u16;
        // Layout womöglich umgeschaltet: Die Leiste sieht gleich nach (statt
        // ständig nachzufragen). Nur bei den wenigen betroffenen Tasten wird
        // der Zustand der anderen abgefragt.
        if !injected && !down && matches!(vk, VK_SPACE | VK_SHIFT | VK_LSHIFT | VK_RSHIFT | VK_MENU | VK_LMENU | VK_RMENU | VK_CONTROL | VK_LCONTROL | VK_RCONTROL) {
            let (shift, ctrl, alt) = modifiers();
            if layout_hotkey(vk, win_physically_down(), shift, ctrl, alt) {
                PostMessageW(bar::hwnd(), bar::WM_BAR_KBD, 0, 0);
            }
        }
        if !injected {
            // Losgelassene Kombi-Taste (egal in welchem Zweig, auch nach der
            // Win-Taste): der nächste Druck zählt wieder als neuer.
            if !down {
                let _ = SWALLOWED_VK.compare_exchange(vk, 0, Ordering::Relaxed, Ordering::Relaxed);
            }
            if vk == VK_LWIN || vk == VK_RWIN {
                if down {
                    // Neuer Druck – auch wenn wir das letzte Loslassen verpasst haben
                    // (Win+L: das kommt auf dem Sperrbildschirm an): Vor diesem Ereignis
                    // war dann keine Win-Taste wirklich unten. Tastenwiederholung beim
                    // Festhalten zählt nicht als neuer Druck.
                    if !WIN_DOWN.swap(true, Ordering::Relaxed) || !win_physically_down() {
                        COMBO.store(false, Ordering::Relaxed);
                        SWALLOWED.store(false, Ordering::Relaxed);
                        SWALLOWED_VK.store(0, Ordering::Relaxed);
                    }
                } else {
                    WIN_DOWN.store(false, Ordering::Relaxed);
                    // Nur maskieren, wenn Windows sonst sein Startmenü öffnen
                    // würde. Bei durchgelassenen Kombis (Win+V, Win+.) würde der
                    // zusätzliche Tastendruck deren Fenster gleich wieder schließen.
                    if !COMBO.load(Ordering::Relaxed) || SWALLOWED.load(Ordering::Relaxed) {
                        send_keys(&[(VK_MASK, false), (VK_MASK, true)]);
                    }
                    if !COMBO.load(Ordering::Relaxed) {
                        PostMessageW(start::hwnd(), start::WM_TOGGLE, 0, 0);
                    }
                }
            } else if WIN_DOWN.load(Ordering::Relaxed) && !win_physically_down() {
                // Loslassen verpasst (z. B. Win+L: das Up kommt auf dem Sperrbildschirm an).
                WIN_DOWN.store(false, Ordering::Relaxed);
            } else if WIN_DOWN.load(Ordering::Relaxed) {
                COMBO.store(true, Ordering::Relaxed);
                // Kürzel, die wir selbst bedienen; die Taste wird geschluckt.
                // Alles andere – auch mit Shift/Strg/Alt (Win+Shift+S =
                // Bildschirmausschnitt, Strg+Win+Pfeil = Desktops) – gehört Windows.
                let Some((target, msg, wp)) = shortcut(vk, modifiers()) else {
                    return CallNextHookEx(null_mut(), code, wp, lp);
                };
                // Alt allein gedrückt und wieder losgelassen öffnet in vielen Apps
                // die Menüleiste – die geschluckte Zahl dazwischen sieht die App
                // nicht. Eine unbelegte Taste dazwischen verhindert das.
                if down && msg == bar::WM_BAR_NUM_JUMP {
                    send_keys(&[(VK_MASK, false), (VK_MASK, true)]);
                }
                // Nur beim ersten Druck auslösen: Tastenwiederholung beim
                // Festhalten (Win+S gedrückt) würde sonst wiederholt umschalten.
                // Geschluckt wird trotzdem jedes Ereignis.
                if down && SWALLOWED_VK.swap(vk, Ordering::Relaxed) != vk {
                    PostMessageW(target, msg, wp, 0);
                }
                SWALLOWED.store(true, Ordering::Relaxed);
                return 1;
            }
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

/// Thread-Nachricht an den Hook-Thread: Hook neu setzen.
const WM_HOOK_REINSTALL: u32 = WM_APP + 1;
/// Thread-ID des Hook-Threads (0 = läuft noch nicht).
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);

unsafe fn install() -> HHOOK {
    let h = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), GetModuleHandleW(null()), 0);
    if h.is_null() {
        crate::util::log(&format!("Tastaturhook konnte nicht gesetzt werden (Fehler {})", GetLastError()));
    }
    h
}

pub fn spawn() {
    std::thread::spawn(|| unsafe {
        let mut msg: MSG = zeroed();
        // Nachrichtenschlange anlegen, bevor andere Threads hierher posten.
        PeekMessageW(&mut msg, null_mut(), 0, 0, PM_NOREMOVE);
        HOOK_THREAD.store(GetCurrentThreadId(), Ordering::Release);
        let mut h = install();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if msg.hwnd.is_null() && msg.message == WM_HOOK_REINSTALL {
                // Windows entfernt LL-Hooks still (Zeitüberschreitung, Sperre,
                // Standby). Neu setzen ist harmlos, wenn er noch da war.
                if !h.is_null() {
                    UnhookWindowsHookEx(h);
                }
                h = install();
                // Zustand zurücksetzen: Loslassen kann verpasst worden sein.
                WIN_DOWN.store(false, Ordering::Relaxed);
                SWALLOWED_VK.store(0, Ordering::Relaxed);
            }
        }
        if !h.is_null() {
            UnhookWindowsHookEx(h);
        }
    });
}

/// Hook neu setzen lassen (nach Entsperren oder Standby; aus `bar`).
pub fn reinstall() {
    let tid = HOOK_THREAD.load(Ordering::Acquire);
    if tid != 0 {
        unsafe { PostThreadMessageW(tid, WM_HOOK_REINSTALL, 0, 0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_hotkeys() {
        // Win+Leertaste, Alt+Umschalt (beide Reihenfolgen), Strg+Umschalt.
        assert!(layout_hotkey(VK_SPACE, true, false, false, false));
        assert!(!layout_hotkey(VK_SPACE, false, false, false, false));
        assert!(layout_hotkey(VK_LSHIFT, false, false, false, true));
        assert!(layout_hotkey(VK_LMENU, false, true, false, false));
        assert!(layout_hotkey(VK_RSHIFT, false, false, true, false));
        assert!(!layout_hotkey(VK_LSHIFT, false, false, false, false));
        assert!(!layout_hotkey(VK_LMENU, false, false, false, false));
    }

    const NONE: (bool, bool, bool) = (false, false, false);
    const SHIFT: (bool, bool, bool) = (true, false, false);
    const CTRL: (bool, bool, bool) = (false, true, false);
    const ALT: (bool, bool, bool) = (false, false, true);

    #[test]
    fn digits_with_modifiers() {
        assert_eq!(action(0x31, NONE), Some(Action::App(0)));
        assert_eq!(action(0x39, NONE), Some(Action::App(8)));
        // Win+0 = zehnte App, wie bei Windows.
        assert_eq!(action(0x30, NONE), Some(Action::App(9)));
        assert_eq!(action(0x32, SHIFT), Some(Action::NewInstance(1)));
        assert_eq!(action(0x30, CTRL), Some(Action::LastWindow(9)));
        assert_eq!(action(0x33, ALT), Some(Action::JumpList(2)));
        // Mehrere Zusatztasten (Strg+Shift+Win+Zahl = als Admin): Windows.
        assert_eq!(action(0x31, (true, true, false)), None);
    }

    #[test]
    fn letters_and_passthrough() {
        assert_eq!(action(VK_KEY_S, NONE), Some(Action::Search));
        assert_eq!(action(VK_KEY_T, NONE), Some(Action::FocusTasks));
        assert_eq!(action(VK_KEY_B, NONE), Some(Action::FocusTray));
        // Win+Shift+S (Bildschirmausschnitt), Win+Strg+D, Win+E bleiben bei Windows.
        assert_eq!(action(VK_KEY_S, SHIFT), None);
        assert_eq!(action(b'D' as u16, CTRL), None);
        assert_eq!(action(b'E' as u16, NONE), None);
        assert_eq!(action(VK_KEY_B, ALT), None);
    }
}
