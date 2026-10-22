//! Schnelleinstellungen (Klick auf Netz/Lautstärke/Akku): WLAN, Bluetooth,
//! Projizieren, Lautstärkeregler, Akkustand, Einstellungen.
//!
//! Funk-Schalter laufen über die WinRT-Radio-API auf einem eigenen Thread,
//! weil deren Aufrufe asynchron sind und den UI-Thread nicht blockieren sollen.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};

use windows::Devices::Radios::{Radio, RadioKind, RadioState};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::status::{self, Net, Status};
use crate::util::*;

const WM_RADIOS: u32 = WM_APP + 20;
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_POLL: usize = 1;

const W: i32 = 360;
const PAD: i32 = 20;
const TILE_W: i32 = 100;
const TILE_H: i32 = 48;
const FOOT: i32 = 52;
const H: i32 = PAD + TILE_H + 28 + 16 + 40 + 16 + FOOT;

// Bits für den Funkstatus.
const WIFI_PRESENT: usize = 1;
const WIFI_ON: usize = 2;
const BT_PRESENT: usize = 4;
const BT_ON: usize = 8;

static HWND_QUICK: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

fn hwnd() -> HWND {
    HWND_QUICK.load(Ordering::Relaxed)
}

/// Schnelleinstellungen offen?
pub fn visible() -> bool {
    let h = hwnd();
    unsafe { !h.is_null() && IsWindowVisible(h) != 0 }
}

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Wifi,
    Bluetooth,
    Project,
    Mute,
    Slider,
    Settings,
    Battery,
}

struct State {
    dpi: u32,
    fonts: Option<(HFONT, HFONT, HFONT)>, // normal, small, icons
    radios: Option<usize>,
    status: Option<Status>,
    mouse: (i32, i32),
    tracking: bool,
    dragging: bool,
    track: RECT,
    hits: Vec<(RECT, Act)>,
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

// ------------------------------------------------------------- Funk

fn query_radios() {
    let target = hwnd() as isize;
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let bits = radio_bits().unwrap_or(0);
        PostMessageW(target as HWND, WM_RADIOS, bits, 0);
    });
}

fn radio_bits() -> windows::core::Result<usize> {
    let radios = Radio::GetRadiosAsync()?.get()?;
    let mut bits = 0;
    for i in 0..radios.Size()? {
        let r = radios.GetAt(i)?;
        let on = r.State()? == RadioState::On;
        match r.Kind()? {
            RadioKind::WiFi => bits |= WIFI_PRESENT | if on { WIFI_ON } else { 0 },
            RadioKind::Bluetooth => bits |= BT_PRESENT | if on { BT_ON } else { 0 },
            _ => {}
        }
    }
    Ok(bits)
}

fn toggle_radio(kind: RadioKind) {
    let target = hwnd() as isize;
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = (|| -> windows::core::Result<()> {
            Radio::RequestAccessAsync()?.get()?;
            let radios = Radio::GetRadiosAsync()?.get()?;
            for i in 0..radios.Size()? {
                let r = radios.GetAt(i)?;
                if r.Kind()? == kind {
                    let next = if r.State()? == RadioState::On { RadioState::Off } else { RadioState::On };
                    r.SetStateAsync(next)?.get()?;
                }
            }
            Ok(())
        })();
        let bits = radio_bits().unwrap_or(0);
        PostMessageW(target as HWND, WM_RADIOS, bits, 0);
    });
}

// ----------------------------------------------------------- Zeichnen

const LINE: u32 = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS;
const CENTER: u32 = DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS;

unsafe fn tile(st: &mut State, dc: HDC, fonts: (HFONT, HFONT, HFONT), x: i32, y: i32, glyph: &str, label: &str, on: bool, enabled: bool, act: Act) {
    let d = st.dpi;
    let r = RECT { left: x, top: y, right: x + px(d, TILE_W), bottom: y + px(d, TILE_H) };
    let hov = enabled && inside(&r, st.mouse.0, st.mouse.1);
    let bg = match (on, hov) {
        (true, false) => accent(),
        (true, true) => rgb(140, 200, 245),
        (false, false) => HOVER,
        (false, true) => ACTIVE,
    };
    round(dc, &r, bg, px(d, 8));
    let fg = if !enabled { DIM } else if on { rgb(0, 0, 0) } else { TEXT };
    text(dc, glyph, &r, fonts.2, fg, CENTER);
    let lr = RECT { left: r.left - px(d, 4), top: r.bottom + px(d, 4), right: r.right + px(d, 4), bottom: r.bottom + px(d, 24) };
    text(dc, label, &lr, fonts.1, if enabled { TEXT } else { DIM }, CENTER);
    if enabled {
        st.hits.push((r, act));
    }
}

unsafe fn draw(st: &mut State, dc: HDC, w: i32, h: i32) {
    let Some(fonts) = st.fonts else { return };
    let d = st.dpi;
    st.hits.clear();
    fill(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, BG);
    let s = st.status.clone().unwrap_or(Status { net: Net::Offline, volume: 0.0, muted: true, battery: None });
    let bits = st.radios.unwrap_or(0);

    // Kacheln
    let gap = (w - 2 * px(d, PAD) - 3 * px(d, TILE_W)) / 2;
    let y = px(d, PAD);
    let x0 = px(d, PAD);
    let wifi_label = match &s.net {
        Net::Wifi(_, ssid) if !ssid.is_empty() => ssid.clone(),
        _ => t!("WLAN", "Wi-Fi").to_string(),
    };
    let known = st.radios.is_some();
    tile(st, dc, fonts, x0, y, "\u{E701}", &wifi_label, bits & WIFI_ON != 0, !known || bits & WIFI_PRESENT != 0, Act::Wifi);
    tile(st, dc, fonts, x0 + px(d, TILE_W) + gap, y, "\u{E702}", "Bluetooth", bits & BT_ON != 0, !known || bits & BT_PRESENT != 0, Act::Bluetooth);
    tile(st, dc, fonts, x0 + 2 * (px(d, TILE_W) + gap), y, "\u{EBC6}", t!("Projizieren", "Project"), false, true, Act::Project);

    // Lautstärke
    let vy = y + px(d, TILE_H + 28 + 16);
    let mute = RECT { left: x0, top: vy, right: x0 + px(d, 40), bottom: vy + px(d, 40) };
    if inside(&mute, st.mouse.0, st.mouse.1) {
        round(dc, &mute, HOVER, px(d, 6));
    }
    text(dc, status::volume_glyph(s.volume, s.muted), &mute, fonts.2, TEXT, CENTER);
    st.hits.push((mute, Act::Mute));
    let pct = RECT { left: w - px(d, PAD + 40), top: vy, right: w - px(d, PAD), bottom: vy + px(d, 40) };
    text(dc, &format!("{}", (s.volume * 100.0).round() as i32), &pct, fonts.0, TEXT, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
    let track = RECT { left: mute.right + px(d, 12), top: vy, right: pct.left - px(d, 12), bottom: vy + px(d, 40) };
    st.track = track;
    let cy = (track.top + track.bottom) / 2;
    let th = px(d, 4).max(2);
    round(dc, &RECT { left: track.left, top: cy - th / 2, right: track.right, bottom: cy + th / 2 }, DIM, th);
    let v = if s.muted { 0.0 } else { s.volume };
    let kx = track.left + ((track.right - track.left) as f32 * v) as i32;
    round(dc, &RECT { left: track.left, top: cy - th / 2, right: kx, bottom: cy + th / 2 }, accent(), th);
    let k = px(d, 10);
    round(dc, &RECT { left: kx - k, top: cy - k, right: kx + k, bottom: cy + k }, rgb(69, 69, 69), 2 * k);
    let ki = px(d, if st.dragging { 7 } else { 5 });
    round(dc, &RECT { left: kx - ki, top: cy - ki, right: kx + ki, bottom: cy + ki }, accent(), 2 * ki);
    st.hits.push((track, Act::Slider));

    // Fußleiste
    let foot = RECT { left: 0, top: h - px(d, FOOT), right: w, bottom: h };
    fill(dc, &foot, PANEL);
    fill(dc, &RECT { bottom: foot.top + 1, ..foot }, BORDER);
    if let Some((p, charging)) = s.battery {
        let br = RECT { left: px(d, PAD - 6), top: foot.top + px(d, 8), right: px(d, PAD + 110), bottom: h - px(d, 8) };
        if inside(&br, st.mouse.0, st.mouse.1) {
            round(dc, &br, HOVER, px(d, 6));
        }
        text(dc, &status::battery_glyph(p, charging), &RECT { left: br.left + px(d, 8), ..br }, fonts.2, TEXT, LINE);
        let label = if charging { tf!("{p} % · lädt", "{p}% · charging") } else { format!("{p} %") };
        text(dc, &label, &RECT { left: br.left + px(d, 34), ..br }, fonts.0, TEXT, LINE);
        st.hits.push((br, Act::Battery));
    }
    let gear = RECT { left: w - px(d, PAD + 36), top: foot.top + px(d, 8), right: w - px(d, PAD - 4), bottom: h - px(d, 8) };
    if inside(&gear, st.mouse.0, st.mouse.1) {
        round(dc, &gear, HOVER, px(d, 6));
    }
    text(dc, "\u{E713}", &gear, fonts.2, TEXT, CENTER);
    st.hits.push((gear, Act::Settings));
}

// ------------------------------------------------------------ Steuerung

pub unsafe fn toggle() {
    let h = hwnd();
    if h.is_null() {
        return;
    }
    if IsWindowVisible(h) != 0 {
        ShowWindow(h, SW_HIDE);
        return;
    }
    let bar = crate::bar::anchor();
    let dpi = GetDpiForWindow(bar);
    let status = status::poll();
    with(|st| {
        if st.dpi != dpi || st.fonts.is_none() {
            if let Some((a, b, c)) = st.fonts.take() {
                DeleteObject(a);
                DeleteObject(b);
                DeleteObject(c);
            }
            st.fonts = Some((make_font(dpi, 13, FW_NORMAL), make_font(dpi, 12, FW_NORMAL), icon_font(dpi, 16)));
            st.dpi = dpi;
        }
        st.status = Some(status);
    });
    query_radios();
    // Angedockte Lage – beim automatischen Ausblenden kann die Leiste gerade weggeschoben sein.
    let br = crate::bar::docked_rect(bar);
    let (w, hh) = (px(dpi, W), px(dpi, H));
    // Am Ende der Leiste (rechts bzw. bei einer seitlichen Leiste unten).
    let (x, y) = crate::display::place_near(br, w, hh, px(dpi, 12), crate::display::Along::End);
    crate::anim::pop_in(h, x, y, w, hh, crate::display::pop_offset(px(dpi, 18)));
    force_foreground(h);
    SetTimer(h, TIMER_POLL, 1000, None);
    InvalidateRect(h, null(), 0);
}

unsafe fn set_from_x(x: i32) {
    let Some(track) = with(|st| st.track) else { return };
    let v = (x - track.left) as f32 / (track.right - track.left).max(1) as f32;
    status::set_volume(v);
    let s = status::poll();
    with(|st| st.status = Some(s));
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_buffered(hwnd, |dc, w, h| {
                with(|st| draw(st, dc, w, h));
            });
            0
        }
        WM_ERASEBKGND => 1,
        // Maus-Capture verloren (anderes Fenster, Esc): Ziehen ist vorbei.
        WM_CAPTURECHANGED => {
            with(|st| st.dragging = false);
            0
        }
        WM_ACTIVATE => {
            if (wp & 0xffff) as u32 == WA_INACTIVE {
                ShowWindow(hwnd, SW_HIDE);
                // Mitten im Ziehen geschlossen (Win, Alt+Tab): sonst springt die
                // Lautstärke beim nächsten Öffnen zur Mausposition.
                if with(|st| std::mem::replace(&mut st.dragging, false)) == Some(true) {
                    ReleaseCapture();
                }
                KillTimer(hwnd, TIMER_POLL);
            }
            0
        }
        WM_TIMER => {
            if with(|st| st.dragging) != Some(true) {
                let s = status::poll();
                with(|st| st.status = Some(s));
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_RADIOS => {
            with(|st| st.radios = Some(wp));
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_MOUSEMOVE => {
            let p = xy(lp);
            let (start, drag) = with(|st| {
                st.mouse = p;
                (!std::mem::replace(&mut st.tracking, true), st.dragging)
            })
            .unwrap_or((false, false));
            if start {
                let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                TrackMouseEvent(&mut tme);
            }
            if drag {
                set_from_x(p.0);
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_MOUSELEAVE => {
            with(|st| {
                st.mouse = (-1, -1);
                st.tracking = false;
            });
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(lp);
            let on_slider = with(|st| st.hits.iter().any(|(r, a)| *a == Act::Slider && inside(r, x, y))).unwrap_or(false);
            if on_slider {
                with(|st| st.dragging = true);
                SetCapture(hwnd);
                set_from_x(x);
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(lp);
            if with(|st| std::mem::replace(&mut st.dragging, false)) == Some(true) {
                ReleaseCapture();
                InvalidateRect(hwnd, null(), 0);
                return 0;
            }
            let act = with(|st| st.hits.iter().rev().find(|(r, _)| inside(r, x, y)).map(|(_, a)| *a)).flatten();
            match act {
                Some(Act::Wifi) => toggle_radio(RadioKind::WiFi),
                Some(Act::Bluetooth) => toggle_radio(RadioKind::Bluetooth),
                Some(Act::Project) => {
                    ShowWindow(hwnd, SW_HIDE);
                    send_keys(&[(VK_LWIN, false), (b'P' as u16, false), (b'P' as u16, true), (VK_LWIN, true)]);
                }
                Some(Act::Mute) => {
                    status::toggle_mute();
                    let s = status::poll();
                    with(|st| st.status = Some(s));
                }
                Some(Act::Settings) => {
                    ShowWindow(hwnd, SW_HIDE);
                    shell_open("ms-settings:", "", SW_SHOWNORMAL);
                }
                Some(Act::Battery) => {
                    ShowWindow(hwnd, SW_HIDE);
                    shell_open("ms-settings:batterysaver", "", SW_SHOWNORMAL);
                }
                Some(Act::Slider) | None => {}
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_RBUTTONUP => {
            // Rechtsklick auf eine Kachel öffnet die passende Einstellungsseite.
            let (x, y) = xy(lp);
            let act = with(|st| st.hits.iter().rev().find(|(r, _)| inside(r, x, y)).map(|(_, a)| *a)).flatten();
            let uri = match act {
                Some(Act::Wifi) => "ms-settings:network-wifi",
                Some(Act::Bluetooth) => "ms-settings:bluetooth",
                Some(Act::Mute | Act::Slider) => "ms-settings:sound",
                Some(Act::Battery) => "ms-settings:powersleep",
                _ => return 0,
            };
            ShowWindow(hwnd, SW_HIDE);
            shell_open(uri, "", SW_SHOWNORMAL);
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp >> 16) & 0xffff) as i16 as f32 / 120.0;
            if let Some(v) = with(|st| st.status.as_ref().map(|s| s.volume)).flatten() {
                status::set_volume(v + delta * 0.02);
                let s = status::poll();
                with(|st| st.status = Some(s));
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_KEYDOWN if wp as u16 == VK_ESCAPE => {
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub unsafe fn create(inst: HINSTANCE) {
    let class = w16("LightbarQuick");
    register_class(inst, &class, Some(wndproc), 0);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
        class.as_ptr(),
        w16(t!("Schnelleinstellungen", "Quick settings")).as_ptr(),
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
            dpi: 0,
            fonts: None,
            radios: None,
            status: None,
            mouse: (-1, -1),
            tracking: false,
            dragging: false,
            track: unsafe { zeroed() },
            hits: Vec::new(),
        })
    });
    HWND_QUICK.store(hwnd, Ordering::Relaxed);
}
