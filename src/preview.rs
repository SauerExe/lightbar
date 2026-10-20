//! Fenster-Vorschau beim Überfahren eines Taskleisten-Buttons: je Fenster der
//! App eine Live-Miniatur über die DWM-Thumbnail-API (die Grafikkarte
//! zeichnet, wir kopieren nichts), dazu Icon, Titel und Schließen-Knopf.
//!
//! Klick auf eine Miniatur aktiviert das Fenster, Mittelklick oder X schließt es.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::util::*;

const TIMER_HIDE: usize = 1;
const WM_MOUSELEAVE: u32 = 0x02A3;

// Logische Maße
const MAX_W: i32 = 240;
const MAX_H: i32 = 150;
const MIN_W: i32 = 160;
const PAD: i32 = 8;
const TITLE_H: i32 = 28;

static HWND_PREVIEW: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

struct Item {
    target: HWND,
    thumb: isize,
    title: String,
    /// Gesamte Kachel, Miniatur und X-Knopf (Client-Koordinaten).
    cell: RECT,
    thumb_rect: RECT,
    close_rect: RECT,
    /// Keine Live-Miniatur (minimiert/leer): großes Icon zeichnen.
    placeholder: bool,
}

struct State {
    items: Vec<Item>,
    icon: HICON,
    dpi: u32,
    font: HFONT,
    icons: HFONT,
    mouse: (i32, i32),
    tracking: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| {
        let mut g = s.try_borrow_mut().ok()?;
        g.as_mut().map(f)
    })
}

fn hwnd() -> HWND {
    HWND_PREVIEW.load(Ordering::Relaxed)
}

pub fn visible() -> bool {
    let h = hwnd();
    unsafe { !h.is_null() && IsWindowVisible(h) != 0 }
}

/// Vorschau für die Fenster `targets` zeigen; `center_x`/`bar_top` in Bildschirmkoordinaten.
pub unsafe fn show(targets: &[HWND], icon: HICON, center_x: i32, bar_top: i32, dpi: u32) {
    let h = hwnd();
    let targets: Vec<HWND> = targets.iter().copied().filter(|&t| IsWindow(t) != 0).collect();
    if h.is_null() || targets.is_empty() {
        return;
    }
    KillTimer(h, TIMER_HIDE);
    let same = with(|s| s.items.iter().map(|i| i.target).eq(targets.iter().copied())).unwrap_or(false);
    if visible() && same {
        return;
    }
    release_thumbs();

    // Passt nicht alles nebeneinander, werden die Miniaturen kleiner.
    let pad = px(dpi, PAD);
    let title_h = px(dpi, TITLE_H);
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let fit = ((sw - px(dpi, 16)) / targets.len() as i32 - 2 * pad).max(px(dpi, 80));
    let max_w = px(dpi, MAX_W).min(fit);
    let max_h = px(dpi, MAX_H) * max_w / px(dpi, MAX_W);
    let cell_w = max_w.max(px(dpi, MIN_W).min(fit)) + 2 * pad;
    let close = px(dpi, 28);

    let mut items = Vec::with_capacity(targets.len());
    let mut thumb_h_max = 0;
    for (n, &target) in targets.iter().enumerate() {
        let mut thumb: isize = 0;
        let registered = DwmRegisterThumbnail(h, target, &mut thumb) == 0;
        let mut src: SIZE = zeroed();
        if registered {
            DwmQueryThumbnailSourceSize(thumb, &mut src);
        }
        let placeholder = !registered || IsIconic(target) != 0 || src.cx <= 0 || src.cy <= 0;
        let (tw, th) = if placeholder {
            (max_w, max_h * 2 / 3)
        } else {
            let scale = (max_w as f32 / src.cx as f32).min(max_h as f32 / src.cy as f32);
            ((src.cx as f32 * scale) as i32, (src.cy as f32 * scale) as i32)
        };
        thumb_h_max = thumb_h_max.max(th);
        let x0 = n as i32 * cell_w;
        let top = pad + title_h + pad / 2;
        items.push(Item {
            target,
            thumb: if registered { thumb } else { 0 },
            title: window_title(target),
            cell: RECT { left: x0, top: 0, right: x0 + cell_w, bottom: 0 },
            thumb_rect: RECT { left: x0 + (cell_w - tw) / 2, top, right: x0 + (cell_w - tw) / 2 + tw, bottom: top + th },
            close_rect: RECT { left: x0 + cell_w - pad - close, top: pad, right: x0 + cell_w - pad, bottom: pad + title_h },
            placeholder,
        });
    }
    let hgt = pad + title_h + pad / 2 + thumb_h_max + pad;
    for item in &mut items {
        item.cell.bottom = hgt;
        // Unterschiedlich hohe Miniaturen unten bündig ausrichten.
        let off = thumb_h_max - (item.thumb_rect.bottom - item.thumb_rect.top);
        item.thumb_rect.top += off;
        item.thumb_rect.bottom += off;
    }
    let w = cell_w * items.len() as i32;

    with(|s| {
        if s.dpi != dpi {
            DeleteObject(s.font);
            DeleteObject(s.icons);
            s.font = make_font(dpi, 12, FW_NORMAL);
            s.icons = icon_font(dpi, 10);
            s.dpi = dpi;
        }
        s.icon = icon;
        s.items = items;
    });

    let x = (center_x - w / 2).clamp(px(dpi, 4), (sw - w - px(dpi, 4)).max(0));
    let y = bar_top - hgt - px(dpi, 8);
    // Beim Wechsel zwischen Buttons nur verschieben, sonst hochgleiten.
    if visible() {
        SetWindowPos(h, HWND_TOPMOST, x, y, w, hgt, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    } else {
        crate::anim::pop_in(h, x, y, w, hgt, px(dpi, 12));
    }

    with(|s| {
        for item in &s.items {
            if item.thumb == 0 {
                continue;
            }
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: item.thumb_rect,
                rcSource: zeroed(),
                opacity: 255,
                fVisible: (!item.placeholder) as BOOL,
                fSourceClientAreaOnly: 0,
            };
            DwmUpdateThumbnailProperties(item.thumb, &props);
        }
    });
    InvalidateRect(h, null(), 0);
}

unsafe fn window_title(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

unsafe fn release_thumbs() {
    let thumbs: Vec<isize> = with(|s| s.items.drain(..).map(|i| i.thumb).collect()).unwrap_or_default();
    for t in thumbs {
        if t != 0 {
            DwmUnregisterThumbnail(t);
        }
    }
}

pub unsafe fn hide() {
    let h = hwnd();
    if h.is_null() {
        return;
    }
    KillTimer(h, TIMER_HIDE);
    release_thumbs();
    if IsWindowVisible(h) != 0 {
        ShowWindow(h, SW_HIDE);
    }
}

/// Mit kurzer Verzögerung schließen – genug Zeit, um mit der Maus vom
/// Button in die Vorschau zu wechseln.
pub unsafe fn schedule_hide() {
    let h = hwnd();
    if !h.is_null() && IsWindowVisible(h) != 0 {
        SetTimer(h, TIMER_HIDE, 250, None);
    }
}

unsafe fn paint(hwnd: HWND) {
    paint_buffered(hwnd, |dc, w, h| {
        with(|s| {
            let d = s.dpi;
            fill(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, rgb(44, 44, 44));
            let pad = px(d, PAD);
            for item in &s.items {
                let hovering = inside(&item.cell, s.mouse.0, s.mouse.1);
                if hovering && s.items.len() > 1 {
                    let c = RECT { left: item.cell.left + 2, top: 2, right: item.cell.right - 2, bottom: item.cell.bottom - 2 };
                    round(dc, &c, rgb(58, 58, 58), px(d, 6));
                }
                let row = RECT { left: item.cell.left + pad, top: pad, right: item.cell.right - pad, bottom: pad + px(d, TITLE_H) };
                let is = px(d, 16);
                DrawIconEx(dc, row.left + px(d, 4), row.top + (row.bottom - row.top - is) / 2, s.icon, is, is, 0, null_mut(), DI_NORMAL);
                let text_right = if hovering { item.close_rect.left - px(d, 4) } else { row.right };
                let tr = RECT { left: row.left + px(d, 28), right: text_right, ..row };
                text(dc, &item.title, &tr, s.font, TEXT, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
                if hovering {
                    if inside(&item.close_rect, s.mouse.0, s.mouse.1) {
                        round(dc, &item.close_rect, rgb(196, 43, 28), px(d, 4));
                    }
                    text(dc, "\u{E8BB}", &item.close_rect, s.icons, TEXT, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                }
                if item.placeholder {
                    // Minimiert oder ohne Inhalt: großes App-Icon statt Live-Bild.
                    round(dc, &item.thumb_rect, rgb(36, 36, 36), px(d, 4));
                    let big = px(d, 48);
                    let r = item.thumb_rect;
                    DrawIconEx(dc, (r.left + r.right - big) / 2, (r.top + r.bottom - big) / 2, s.icon, big, big, 0, null_mut(), DI_NORMAL);
                }
            }
        });
    });
}

/// Element unter dem Punkt: (Fenster, auf dem X-Knopf).
fn item_at(x: i32, y: i32) -> Option<(HWND, bool)> {
    with(|s| s.items.iter().find(|i| inside(&i.cell, x, y)).map(|i| (i.target, inside(&i.close_rect, x, y)))).flatten()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_TIMER => {
            if wp == TIMER_HIDE {
                hide();
            }
            0
        }
        WM_MOUSEMOVE => {
            KillTimer(hwnd, TIMER_HIDE);
            let p = xy(lp);
            let start = with(|s| {
                s.mouse = p;
                !std::mem::replace(&mut s.tracking, true)
            })
            .unwrap_or(false);
            if start {
                let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                TrackMouseEvent(&mut tme);
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_MOUSELEAVE => {
            with(|s| {
                s.mouse = (-1, -1);
                s.tracking = false;
            });
            InvalidateRect(hwnd, null(), 0);
            schedule_hide();
            0
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(lp);
            let Some((target, on_close)) = item_at(x, y) else { return 0 };
            if on_close {
                PostMessageW(target, WM_CLOSE, 0, 0);
                // Bei mehreren Fenstern bleibt die Vorschau offen; die Leiste
                // aktualisiert sie beim nächsten Überfahren.
                hide();
            } else {
                hide();
                if IsIconic(target) != 0 {
                    ShowWindow(target, SW_RESTORE);
                }
                force_foreground(target);
            }
            0
        }
        WM_MBUTTONUP => {
            let (x, y) = xy(lp);
            if let Some((target, _)) = item_at(x, y) {
                hide();
                PostMessageW(target, WM_CLOSE, 0, 0);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub unsafe fn create(inst: HINSTANCE) {
    let class = w16("RustTaskbarPreview");
    register_class(inst, &class, Some(wndproc), CS_DROPSHADOW);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED,
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
    if hwnd.is_null() {
        return;
    }
    crate::anim::make_opaque(hwnd);
    let round_pref: u32 = 2;
    let border = BORDER;
    DwmSetWindowAttribute(hwnd, 33, &round_pref as *const _ as _, 4);
    DwmSetWindowAttribute(hwnd, 34, &border as *const _ as _, 4);
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            items: Vec::new(),
            icon: null_mut(),
            dpi: 0,
            font: null_mut(),
            icons: null_mut(),
            mouse: (-1, -1),
            tracking: false,
        })
    });
    HWND_PREVIEW.store(hwnd, Ordering::Relaxed);
}
