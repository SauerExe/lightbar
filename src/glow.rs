//! Leuchtrand über der Leiste: ein fließender Aurora-Verlauf an der
//! Oberkante, der weich nach oben ausstrahlt, mit gelegentlichem Kometen.
//!
//! Reagiert auf die Leiste: glüht über dem Button unter der Maus, pulsiert
//! orange bei Apps, die Aufmerksamkeit wollen, und färbt sich warm, wenn ein
//! Claude-/Codex-Limit fast erreicht ist.
//!
//! Eigenes Layered-Fenster mit Pro-Pixel-Alpha (UpdateLayeredWindow),
//! klickdurchlässig und nie aktiv. Gerendert wird spaltenweise in einen
//! DIB-Puffer – rund 40 000 Pixel pro Bild bei 30 fps.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::Instant;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::settings::GlowStyle;
use crate::util::*;

const TIMER_FRAME: usize = 1;
/// Höhe des Leuchtens über der Leiste und Stärke der Linie (logisch).
const GLOW_H: i32 = 22;
const LINE_H: i32 = 2;

struct Flash {
    x: i32,
    start: Instant,
}

struct State {
    hwnd: HWND,
    start: Instant,
    visible: bool,
    dpi: u32,
    w: i32,
    h: i32,
    dc: HDC,
    bmp: HBITMAP,
    bits: *mut u8,
    hover_x: Option<i32>,
    /// Geglättete Hover-Position und -Stärke, damit das Glühen mitgleitet.
    hover_pos: f32,
    hover_amt: f32,
    attention: Vec<i32>,
    flashes: Vec<Flash>,
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

/// Stil wechseln (aus den Einstellungen) und sofort anwenden.
pub unsafe fn set_style(style: GlowStyle) {
    crate::settings::update(|s| s.glow = style);
    sync();
}

/// Mauszeiger über der Leiste (Bildschirm-x) oder None.
pub fn set_hover(x: Option<i32>) {
    with(|s| s.hover_x = x);
}

/// Dauerhaft pulsierende Stellen (Buttons, die Aufmerksamkeit wollen).
pub fn set_attention(xs: Vec<i32>) {
    with(|s| s.attention = xs);
}

/// Einmaliger Lichtimpuls an Bildschirm-x (z. B. App gestartet).
pub fn flash(x: i32) {
    with(|s| s.flashes.push(Flash { x, start: Instant::now() }));
}

/// An die Leiste anlegen bzw. ausblenden (Vollbild, abgeschaltet).
pub unsafe fn sync() {
    let bar = crate::bar::hwnd();
    let Some(hwnd) = with(|s| s.hwnd) else { return };
    let show = crate::settings::get().glow != GlowStyle::Off && !bar.is_null() && IsWindowVisible(bar) != 0;
    if !show {
        KillTimer(hwnd, TIMER_FRAME);
        ShowWindow(hwnd, SW_HIDE);
        with(|s| s.visible = false);
        return;
    }
    let mut br: RECT = zeroed();
    GetWindowRect(bar, &mut br);
    let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForWindow(bar);
    let (w, h) = (br.right - br.left, px(dpi, GLOW_H) + px(dpi, LINE_H));
    resize(w, h, dpi);
    SetWindowPos(hwnd, HWND_TOPMOST, br.left, br.top - px(dpi, GLOW_H), w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    let was = with(|s| std::mem::replace(&mut s.visible, true)).unwrap_or(true);
    if !was {
        SetTimer(hwnd, TIMER_FRAME, 33, None);
    }
    frame();
}

/// Über der Leiste halten (die schiebt sich selbst gelegentlich nach vorne).
pub unsafe fn raise() {
    if let Some((hwnd, true)) = with(|s| (s.hwnd, s.visible)) {
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

unsafe fn resize(w: i32, h: i32, dpi: u32) {
    with(|s| {
        s.dpi = dpi;
        if s.w == w && s.h == h && !s.bmp.is_null() {
            return;
        }
        if !s.bmp.is_null() {
            DeleteObject(s.bmp);
        }
        let mut bi: BITMAPINFO = zeroed();
        bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h; // von oben nach unten
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        let mut bits: *mut c_void = null_mut();
        s.bmp = CreateDIBSection(s.dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        SelectObject(s.dc, s.bmp);
        s.bits = bits as *mut u8;
        s.w = w;
        s.h = h;
    });
}

fn rgb_f(c: COLORREF) -> (f32, f32, f32) {
    ((c & 0xff) as f32, ((c >> 8) & 0xff) as f32, ((c >> 16) & 0xff) as f32)
}

/// Stützfarben je Stil.
fn stops(style: GlowStyle) -> [(f32, f32, f32); 4] {
    match style {
        GlowStyle::Accent => {
            // Akzentfarbe, aufgehellt, Akzent, leicht abgedunkelt – ruhiges Schimmern.
            let a = accent();
            [rgb_f(a), rgb_f(mix(a, rgb(255, 255, 255), 0.45)), rgb_f(a), rgb_f(mix(a, rgb(0, 0, 0), 0.25))]
        }
        GlowStyle::Neon => [(255.0, 40.0, 90.0), (255.0, 210.0, 0.0), (0.0, 255.0, 150.0), (60.0, 120.0, 255.0)],
        _ => [(76.0, 194.0, 255.0), (138.0, 92.0, 255.0), (255.0, 92.0, 200.0), (46.0, 230.0, 197.0)],
    }
}

/// Farbverlauf (zyklisch) nach Stil, warm bei fast erreichtem Limit.
fn palette(cool: &[(f32, f32, f32); 4], p: f32, warm: f32) -> (f32, f32, f32) {
    const HOT: [(f32, f32, f32); 4] = [(255.0, 120.0, 40.0), (255.0, 60.0, 60.0), (255.0, 170.0, 0.0), (255.0, 80.0, 120.0)];
    let p = p.rem_euclid(1.0) * 4.0;
    let i = p.floor() as usize % 4;
    let f = p - p.floor();
    // Glatter Übergang zwischen den Stützfarben.
    let f = f * f * (3.0 - 2.0 * f);
    let mix = |a: [(f32, f32, f32); 4]| {
        let (c0, c1) = (a[i], a[(i + 1) % 4]);
        (c0.0 + (c1.0 - c0.0) * f, c0.1 + (c1.1 - c0.1) * f, c0.2 + (c1.2 - c0.2) * f)
    };
    let (c, h) = (mix(*cool), mix(HOT));
    (c.0 + (h.0 - c.0) * warm, c.1 + (h.1 - c.1) * warm, c.2 + (h.2 - c.2) * warm)
}

/// Höchste aktuelle 5-Stunden-Auslastung (Claude/Codex) → 0..1 "Wärme".
fn warmth() -> f32 {
    let Some(u) = crate::usage::current() else { return 0.0 };
    let used = [&u.claude, &u.codex]
        .into_iter()
        .filter_map(|r| r.as_ref()?.as_ref().ok()?.five_hour.map(|w| w.used))
        .fold(0.0f32, f32::max);
    ((used - 80.0) / 15.0).clamp(0.0, 1.0)
}

unsafe fn frame() {
    let warm = warmth();
    let style = crate::settings::get().glow;
    let cool = stops(style);
    // Neon fließt schneller und über mehr Farbe pro Breite.
    let (spread, speed) = if style == GlowStyle::Neon { (2.4, 0.16) } else { (1.4, 0.06) };
    let Some((hwnd, dc, w, h)) = with(|s| {
        if s.bits.is_null() || !s.visible {
            return None;
        }
        let t = s.start.elapsed().as_secs_f32();
        let (w, h, d) = (s.w, s.h, s.dpi);
        let line = px(d, LINE_H).max(1);
        let glow = h - line;
        let scale = d as f32 / 96.0;

        // Hover-Glühen gleitet der Maus hinterher und blendet weich ein/aus.
        let screen_left = {
            let mut r: RECT = zeroed();
            GetWindowRect(s.hwnd, &mut r);
            r.left
        };
        match s.hover_x {
            Some(x) => {
                let target = (x - screen_left) as f32;
                if s.hover_amt < 0.01 {
                    s.hover_pos = target;
                }
                s.hover_pos += (target - s.hover_pos) * 0.35;
                s.hover_amt += (1.0 - s.hover_amt) * 0.2;
            }
            None => s.hover_amt *= 0.85,
        }
        s.flashes.retain(|f| f.start.elapsed().as_secs_f32() < 1.2);

        // Komet: alle ~7 s einmal quer über die Leiste.
        let period = 7.0;
        let ct = (t % period) / 1.6;
        let comet = (ct < 1.0).then(|| ct * (w as f32 + 400.0 * scale) - 200.0 * scale);

        let pixels = std::slice::from_raw_parts_mut(s.bits, (w * h * 4) as usize);
        for x in 0..w {
            let xf = x as f32;
            let mut i = 0.42 + 0.18 * (xf * 0.006 / scale + t * 1.1).sin() + 0.12 * (xf * 0.013 / scale - t * 1.7).sin();
            if let Some(cx) = comet {
                i += 1.1 * (-((xf - cx) / (110.0 * scale)).powi(2)).exp();
            }
            i += 0.9 * s.hover_amt * (-((xf - s.hover_pos) / (70.0 * scale)).powi(2)).exp();
            let pulse = 0.5 + 0.5 * (t * 6.0).sin();
            let mut orange = 0.0f32;
            for &ax in &s.attention {
                orange += (0.6 + 0.6 * pulse) * (-((xf - (ax - screen_left) as f32) / (60.0 * scale)).powi(2)).exp();
            }
            for f in &s.flashes {
                let ft = f.start.elapsed().as_secs_f32() / 1.2;
                let spread = 40.0 + 260.0 * ft;
                i += 1.2 * (1.0 - ft) * (-((xf - (f.x - screen_left) as f32) / (spread * scale)).powi(2)).exp();
            }
            let i = i.clamp(0.0, 1.6);
            let (mut r, mut g, mut b) = palette(&cool, xf / w as f32 * spread - t * speed, warm);
            let o = orange.clamp(0.0, 1.0);
            r += (255.0 - r) * o;
            g += (140.0 - g) * o;
            b += (0.0 - b) * o;
            let i = i.max(o * 1.2);
            for y in 0..h {
                let a = if y >= glow {
                    (0.55 + 0.45 * i).min(1.0)
                } else {
                    // Weiches Ausstrahlen nach oben (y = 0 ist der obere Rand).
                    let k = (y + 1) as f32 / glow as f32;
                    (i * 0.5 * k.powf(2.4)).min(1.0)
                };
                let idx = ((y * w + x) * 4) as usize;
                // Vormultipliziertes BGRA für UpdateLayeredWindow.
                pixels[idx] = (b * a) as u8;
                pixels[idx + 1] = (g * a) as u8;
                pixels[idx + 2] = (r * a) as u8;
                pixels[idx + 3] = (a * 255.0) as u8;
            }
        }
        Some((s.hwnd, s.dc, w, h))
    })
    .flatten() else {
        return;
    };
    let mut r: RECT = zeroed();
    GetWindowRect(hwnd, &mut r);
    let pos = POINT { x: r.left, y: r.top };
    let size = SIZE { cx: w, cy: h };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    let screen = GetDC(null_mut());
    UpdateLayeredWindow(hwnd, screen, &pos, &size, dc, &src, 0, &blend, ULW_ALPHA);
    ReleaseDC(null_mut(), screen);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wp == TIMER_FRAME => {
            frame();
            0
        }
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub unsafe fn create(inst: HINSTANCE) {
    let class = w16("LightbarGlow");
    register_class(inst, &class, Some(wndproc), 0);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
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
    let screen = GetDC(null_mut());
    let dc = CreateCompatibleDC(screen);
    ReleaseDC(null_mut(), screen);
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            hwnd,
            start: Instant::now(),
            visible: false,
            dpi: 96,
            w: 0,
            h: 0,
            dc,
            bmp: null_mut(),
            bits: null_mut(),
            hover_x: None,
            hover_pos: 0.0,
            hover_amt: 0.0,
            attention: Vec::new(),
            flashes: Vec::new(),
        })
    });
    sync();
}
