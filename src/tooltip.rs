//! Eigener Tooltip im Win11-Stil: klickdurchlässig, nie aktiv, zentriert über
//! dem Element.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::util::*;

static HWND_TIP: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

thread_local! {
    static TIP: RefCell<(String, HFONT, u32)> = const { RefCell::new((String::new(), std::ptr::null_mut(), 0)) };
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let (text_s, font, dpi) = TIP.with(|t| t.borrow().clone());
            paint_buffered(hwnd, |dc, w, h| {
                fill(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, rgb(43, 43, 43));
                let r = RECT { left: px(dpi, 10), top: px(dpi, 6), right: w - px(dpi, 10), bottom: h - px(dpi, 6) };
                text(dc, &text_s, &r, font, TEXT, DT_LEFT | DT_TOP | DT_WORDBREAK);
            });
            0
        }
        WM_ERASEBKGND => 1,
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub unsafe fn create(inst: HINSTANCE) {
    let class = w16("LightbarTip");
    register_class(inst, &class, Some(wndproc), 0);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED,
        class.as_ptr(),
        null(),
        WS_POPUP,
        0,
        0,
        0,
        0,
        null_mut(),
        null_mut(),
        inst,
        null(),
    );
    crate::anim::make_opaque(hwnd);
    let round_pref: u32 = 2;
    let border = BORDER;
    DwmSetWindowAttribute(hwnd, 33, &round_pref as *const _ as _, 4);
    DwmSetWindowAttribute(hwnd, 34, &border as *const _ as _, 4);
    HWND_TIP.store(hwnd, Ordering::Relaxed);
}

/// Tooltip neben `anchor` (Bildschirmkoordinaten) zeigen: über einem Element
/// einer unteren Leiste, unter einem einer oberen, neben einem einer seitlichen.
pub unsafe fn show(s: &str, anchor: RECT, dpi: u32) {
    let hwnd = HWND_TIP.load(Ordering::Relaxed);
    if hwnd.is_null() || s.is_empty() {
        return;
    }
    let font = TIP.with(|t| {
        let mut t = t.borrow_mut();
        if t.2 != dpi {
            if !t.1.is_null() {
                DeleteObject(t.1);
            }
            t.1 = make_font(dpi, 12, FW_NORMAL);
            t.2 = dpi;
        }
        t.0 = s.to_string();
        t.1
    });
    // Textgröße messen (mehrzeilig, max. 400 px breit).
    let dc = GetDC(null_mut());
    let of = SelectObject(dc, font);
    let w = w16(s);
    let mut r = RECT { left: 0, top: 0, right: px(dpi, 400), bottom: 0 };
    DrawTextW(dc, w.as_ptr(), -1, &mut r, DT_CALCRECT | DT_LEFT | DT_WORDBREAK | DT_NOPREFIX);
    SelectObject(dc, of);
    ReleaseDC(null_mut(), dc);
    let (tw, th) = (r.right + px(dpi, 20), r.bottom + px(dpi, 12));
    let (x, y) = crate::display::place_near(anchor, tw, th, px(dpi, 8), crate::display::Along::Center);
    if IsWindowVisible(hwnd) != 0 {
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, tw, th, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    } else {
        crate::anim::pop_in(hwnd, x, y, tw, th, crate::display::pop_offset(px(dpi, 6)));
    }
    InvalidateRect(hwnd, null(), 0);
}

pub unsafe fn hide() {
    let hwnd = HWND_TIP.load(Ordering::Relaxed);
    if !hwnd.is_null() && IsWindowVisible(hwnd) != 0 {
        ShowWindow(hwnd, SW_HIDE);
    }
}
