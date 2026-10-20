//! Win-Taste übernehmen: Low-Level-Tastaturhook auf eigenem Thread.
//!
//! Win allein öffnet unser Startmenü, Win+S die Suche, Win+X das Kontextmenü,
//! Win+A die Schnelleinstellungen, Win+1…9 die Taskleisten-Apps. Andere
//! Kombinationen wie Win+E
//! laufen unverändert an Windows durch. Damit Windows beim Loslassen nicht
//! zusätzlich sein eigenes Startmenü öffnet, schicken wir vorher eine
//! unbelegte Maskierungstaste (0xE8) – Windows sieht dann eine Kombination.
//!
//! Der Hook läuft auf einem eigenen Thread, damit er nie hinter langsamer
//! UI-Arbeit wartet (Windows entfernt Hooks, die zu oft zu lange brauchen).

use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::{bar, start};
use crate::util::send_keys;

const VK_MASK: VIRTUAL_KEY = 0xE8;
const VK_KEY_S: u16 = b'S' as u16;
const VK_KEY_X: u16 = b'X' as u16;
const VK_KEY_A: u16 = b'A' as u16;
const VK_KEY_T: u16 = b'T' as u16;

static WIN_DOWN: AtomicBool = AtomicBool::new(false);
static COMBO: AtomicBool = AtomicBool::new(false);

/// Tatsächlicher Zustand vor dem aktuellen Ereignis (im LL-Hook verlässlich).
unsafe fn win_physically_down() -> bool {
    (GetAsyncKeyState(VK_LWIN as i32) as u16 & 0x8000) != 0 || (GetAsyncKeyState(VK_RWIN as i32) as u16 & 0x8000) != 0
}

unsafe fn modifier_down() -> bool {
    [VK_SHIFT, VK_CONTROL, VK_MENU].iter().any(|&k| GetAsyncKeyState(k as i32) as u16 & 0x8000 != 0)
}

unsafe extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = &*(lp as *const KBDLLHOOKSTRUCT);
        let injected = k.flags & LLKHF_INJECTED != 0;
        let down = wp as u32 == WM_KEYDOWN || wp as u32 == WM_SYSKEYDOWN;
        let vk = k.vkCode as u16;
        if !injected {
            if vk == VK_LWIN || vk == VK_RWIN {
                if down {
                    if !WIN_DOWN.swap(true, Ordering::Relaxed) {
                        COMBO.store(false, Ordering::Relaxed);
                    }
                } else {
                    WIN_DOWN.store(false, Ordering::Relaxed);
                    send_keys(&[(VK_MASK, false), (VK_MASK, true)]);
                    if !COMBO.load(Ordering::Relaxed) {
                        PostMessageW(start::hwnd(), start::WM_TOGGLE, 0, 0);
                    }
                }
            } else if WIN_DOWN.load(Ordering::Relaxed) && !win_physically_down() {
                // Loslassen verpasst (z. B. Win+L: das Up kommt auf dem Sperrbildschirm an).
                WIN_DOWN.store(false, Ordering::Relaxed);
            } else if WIN_DOWN.load(Ordering::Relaxed) {
                COMBO.store(true, Ordering::Relaxed);
                // Mit Shift/Strg/Alt gehört das Kürzel Windows (z. B. Win+Shift+S
                // = Bildschirmausschnitt, Strg+Win+Pfeil = Desktops).
                if modifier_down() {
                    return CallNextHookEx(null_mut(), code, wp, lp);
                }
                // Kürzel, die wir selbst bedienen; die Taste wird geschluckt.
                let (target, msg, wp) = match vk {
                    VK_KEY_S => (start::hwnd(), start::WM_TOGGLE, 1),
                    VK_KEY_X => (bar::hwnd(), bar::WM_BAR_WINX, 0),
                    VK_KEY_A => (bar::hwnd(), bar::WM_BAR_QUICK, 0),
                    VK_KEY_T => (bar::hwnd(), bar::WM_BAR_FOCUS, 0),
                    0x31..=0x39 => (bar::hwnd(), bar::WM_BAR_NUM, (vk - 0x31) as usize),
                    _ => return CallNextHookEx(null_mut(), code, wp, lp),
                };
                if down {
                    PostMessageW(target, msg, wp, 0);
                }
                return 1;
            }
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

pub fn spawn() {
    std::thread::spawn(|| unsafe {
        let h = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), GetModuleHandleW(null()), 0);
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {}
        UnhookWindowsHookEx(h);
    });
}
