//! Leuchtrand an der Leiste: ein fließender Aurora-Verlauf an der Kante, die
//! zum Bildschirm zeigt (unten: Oberkante, oben: Unterkante, seitlich:
//! Innenkante), der weich in den Bildschirm ausstrahlt, mit gelegentlichem Kometen.
//!
//! Reagiert auf die Leiste: glüht über dem Button unter der Maus, pulsiert
//! orange bei Apps, die Aufmerksamkeit wollen, und färbt sich warm, wenn ein
//! Claude-/Codex-Limit fast erreicht ist.
//!
//! Eigene Layered-Fenster mit Pro-Pixel-Alpha (UpdateLayeredWindow),
//! klickdurchlässig und nie aktiv. Gerendert wird spaltenweise in einen
//! DIB-Puffer – rund 40 000 Pixel pro Leiste und Bild bei 30 fps. Auf
//! mehreren Monitoren hat jede Leiste ihr Leuchtfenster; Farbverlauf und Komet
//! laufen in Bildschirmkoordinaten als ein Band von links nach rechts (bei
//! seitlichen Leisten von oben nach unten).

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::Instant;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::display::Edge;
use crate::settings::GlowStyle;
use crate::util::*;

const TIMER_FRAME: usize = 1;
/// Tiefe des Leuchtens neben der Leiste und Stärke der Linie (logisch).
const GLOW_H: i32 = 22;
const LINE_H: i32 = 2;

struct Flash {
    /// Position entlang der Leiste (Bildschirm-x, seitlich Bildschirm-y).
    at: i32,
    start: Instant,
}

/// Ein Leuchtfenster über einer Leiste (Haupt- oder Zweitleiste).
struct Surface {
    hwnd: HWND,
    bar: HWND,
    /// Skalierung der Leiste darunter (Zweitleisten können anders skaliert sein).
    dpi: u32,
    /// Position auf dem Bildschirm; w × h ist die Puffergröße (seitlich hochkant).
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    bmp: HBITMAP,
    bits: *mut u8,
}

struct State {
    inst: HINSTANCE,
    start: Instant,
    visible: bool,
    dpi: u32,
    dc: HDC,
    surfaces: Vec<Surface>,
    /// Anfang und Länge aller Monitore zusammen entlang der Leiste (waagerecht
    /// x, seitlich y): Farbverlauf und Komet laufen als ein Band über alle Leisten.
    span: (i32, i32),
    /// Kante der Leisten: bestimmt, wohin das Leuchten ausstrahlt.
    edge: Edge,
    hover_x: Option<i32>,
    /// Geglättete Hover-Position und -Stärke, damit das Glühen mitgleitet.
    hover_pos: f32,
    hover_amt: f32,
    attention: Vec<i32>,
    flashes: Vec<Flash>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// Fenster, an dem der Bildtakt für alle Leuchtfenster hängt.
    static TIMER_OWNER: Cell<HWND> = const { Cell::new(null_mut()) };
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

/// Mauszeiger über der Leiste (Position entlang der Leiste: Bildschirm-x,
/// seitlich Bildschirm-y) oder None.
pub fn set_hover(x: Option<i32>) {
    with(|s| s.hover_x = x);
}

/// Dauerhaft pulsierende Stellen (Buttons, die Aufmerksamkeit wollen), entlang der Leiste.
pub fn set_attention(xs: Vec<i32>) {
    with(|s| s.attention = xs);
}

/// Einmaliger Lichtimpuls an einer Stelle entlang der Leiste (z. B. App gestartet).
pub fn flash(at: i32) {
    with(|s| s.flashes.push(Flash { at, start: Instant::now() }));
}

/// Fenster des Leuchtrands zu einer Leiste `bar` (Bildschirmkoordinaten): die
/// Linie (`line`) liegt auf dem Rand der Leiste, das Leuchten (`glow`) ragt
/// davor in den Bildschirm.
fn surface_rect(edge: Edge, bar: RECT, glow: i32, line: i32) -> RECT {
    match edge {
        Edge::Bottom => RECT { top: bar.top - glow, bottom: bar.top + line, ..bar },
        Edge::Top => RECT { top: bar.bottom - line, bottom: bar.bottom + glow, ..bar },
        Edge::Left => RECT { left: bar.right - line, right: bar.right + glow, ..bar },
        Edge::Right => RECT { left: bar.left - glow, right: bar.left + line, ..bar },
    }
}

/// Pufferkoordinaten (x, y) für Stelle `a` entlang der Leiste und Tiefe `d`
/// quer dazu (0 = am weitesten von der Leiste weg, `depth - 1` = an der Leiste).
fn pixel_at(edge: Edge, a: i32, d: i32, depth: i32) -> (i32, i32) {
    match edge {
        Edge::Bottom => (a, d),
        Edge::Top => (a, depth - 1 - d),
        Edge::Right => (d, a),
        Edge::Left => (depth - 1 - d, a),
    }
}

unsafe fn create_window(inst: HINSTANCE) -> HWND {
    let class = w16("LightbarGlow");
    CreateWindowExW(
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
    )
}

unsafe fn free(sf: &Surface) {
    DestroyWindow(sf.hwnd);
    if !sf.bmp.is_null() {
        DeleteObject(sf.bmp);
    }
}

/// Puffer für die Fläche in passender Größe bereithalten.
unsafe fn ensure_buffer(sf: &mut Surface, w: i32, h: i32) {
    if sf.w == w && sf.h == h && !sf.bmp.is_null() {
        return;
    }
    if !sf.bmp.is_null() {
        DeleteObject(sf.bmp);
    }
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h; // von oben nach unten
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    let mut bits: *mut c_void = null_mut();
    sf.bmp = CreateDIBSection(null_mut(), &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    sf.bits = bits as *mut u8;
    sf.w = w;
    sf.h = h;
}

/// An alle sichtbaren Leisten anlegen bzw. ausblenden (Vollbild, abgeschaltet,
/// automatisch ausgeblendet).
pub unsafe fn sync() {
    let Some((inst, mut old)) = with(|s| (s.inst, std::mem::take(&mut s.surfaces))) else { return };
    let edge = crate::display::edge();
    let bars: Vec<HWND> = if crate::settings::get().glow != GlowStyle::Off {
        crate::bar::all_bars().into_iter().filter(|&b| !b.is_null() && IsWindowVisible(b) != 0 && crate::bar::fully_shown(b)).collect()
    } else {
        Vec::new()
    };
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    // Muster (Verlauf, Komet) läuft in der Skalierung der Hauptleiste über alle
    // Leisten; Höhe von Leuchten und Linie richtet sich nach der jeweiligen Leiste.
    let dpi = GetDpiForWindow(crate::bar::hwnd()).max(96);
    let span = if edge.vertical() {
        (GetSystemMetrics(SM_YVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1))
    } else {
        (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1))
    };
    let mut surfaces = Vec::new();
    for bar in bars {
        let mut br: RECT = zeroed();
        GetWindowRect(bar, &mut br);
        let bar_dpi = GetDpiForWindow(bar).max(96);
        let r = surface_rect(edge, br, px(bar_dpi, GLOW_H), px(bar_dpi, LINE_H));
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let mut sf = match old.iter().position(|o| o.bar == bar) {
            Some(i) => old.remove(i),
            None => {
                let hwnd = create_window(inst);
                if hwnd.is_null() {
                    continue;
                }
                Surface { hwnd, bar, dpi: bar_dpi, x: 0, y: 0, w: 0, h: 0, bmp: null_mut(), bits: null_mut() }
            }
        };
        sf.dpi = bar_dpi;
        ensure_buffer(&mut sf, w, h);
        sf.x = r.left;
        sf.y = r.top;
        SetWindowPos(sf.hwnd, HWND_TOPMOST, sf.x, sf.y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        surfaces.push(sf);
    }
    for o in &old {
        free(o);
    }
    let first = surfaces.first().map(|s| s.hwnd).unwrap_or(null_mut());
    with(|s| {
        s.visible = !surfaces.is_empty();
        s.surfaces = surfaces;
        s.dpi = dpi;
        s.span = span;
        s.edge = edge;
    });
    // Ein Takt für alle Fenster, am ersten Leuchtfenster.
    let prev = TIMER_OWNER.with(|t| t.replace(first));
    if prev != first {
        if !prev.is_null() && IsWindow(prev) != 0 {
            KillTimer(prev, TIMER_FRAME);
        }
        if !first.is_null() {
            SetTimer(first, TIMER_FRAME, 33, None);
        }
    }
    if !first.is_null() {
        frame();
    }
}

/// Über der Leiste halten (die schiebt sich selbst gelegentlich nach vorne).
pub unsafe fn raise() {
    for h in with(|s| s.surfaces.iter().map(|f| f.hwnd).collect::<Vec<_>>()).unwrap_or_default() {
        SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
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
    let Some((dc, jobs)) = with(|s| {
        if !s.visible {
            return (s.dc, Vec::new());
        }
        let t = s.start.elapsed().as_secs_f32();
        let d = s.dpi;
        let scale = d as f32 / 96.0;
        let (span_l, span_w) = (s.span.0 as f32, s.span.1 as f32);
        let edge = s.edge;
        let vertical = edge.vertical();

        // Hover-Glühen gleitet der Maus hinterher und blendet weich ein/aus
        // (alles in Bildschirmkoordinaten entlang der Leiste).
        match s.hover_x {
            Some(x) => {
                let target = x as f32;
                if s.hover_amt < 0.01 {
                    s.hover_pos = target;
                }
                s.hover_pos += (target - s.hover_pos) * 0.35;
                s.hover_amt += (1.0 - s.hover_amt) * 0.2;
            }
            None => s.hover_amt *= 0.85,
        }
        s.flashes.retain(|f| f.start.elapsed().as_secs_f32() < 1.2);

        // Komet: alle ~7 s einmal von links nach rechts (seitlich von oben nach
        // unten) über alle Leisten
        // (bei breiterer Gesamtfläche entsprechend länger unterwegs).
        let travel = 1.6 * (span_w / (1920.0 * scale)).max(1.0);
        let ct = (t % (travel + 5.4)) / travel;
        let comet = (ct < 1.0).then(|| span_l + ct * (span_w + 400.0 * scale) - 200.0 * scale);
        let pulse = 0.5 + 0.5 * (t * 6.0).sin();
        let flashes: Vec<(f32, f32)> = s.flashes.iter().map(|f| (f.at as f32, f.start.elapsed().as_secs_f32() / 1.2)).collect();

        let mut jobs = Vec::new();
        for sf in &s.surfaces {
            if sf.bits.is_null() {
                continue;
            }
            let (w, h) = (sf.w, sf.h);
            let line = px(sf.dpi, LINE_H).max(1);
            // Länge entlang der Leiste und Tiefe quer dazu.
            let (len, depth, origin) = if vertical { (h, w, sf.y) } else { (w, h, sf.x) };
            let glow = depth - line;
            let pixels = std::slice::from_raw_parts_mut(sf.bits, (w * h * 4) as usize);
            // Ausblend-Verlauf weg von der Leiste hängt nur von der Tiefe ab: einmal
            // pro Bild statt für jedes Pixel (powf ist teuer).
            let fade: Vec<f32> = (0..glow).map(|y| 0.5 * ((y + 1) as f32 / glow as f32).powf(2.4)).collect();
            for pos in 0..len {
                // Bildschirmposition entlang der Leiste: das Muster läuft nahtlos
                // über alle Monitore.
                let xf = (origin + pos) as f32;
                let gx = xf - span_l;
                let mut i = 0.42 + 0.18 * (gx * 0.006 / scale + t * 1.1).sin() + 0.12 * (gx * 0.013 / scale - t * 1.7).sin();
                if let Some(cx) = comet {
                    i += 1.1 * (-((xf - cx) / (110.0 * scale)).powi(2)).exp();
                }
                i += 0.9 * s.hover_amt * (-((xf - s.hover_pos) / (70.0 * scale)).powi(2)).exp();
                let mut orange = 0.0f32;
                for &ax in &s.attention {
                    orange += (0.6 + 0.6 * pulse) * (-((xf - ax as f32) / (60.0 * scale)).powi(2)).exp();
                }
                for &(fx, ft) in &flashes {
                    let spread = 40.0 + 260.0 * ft;
                    i += 1.2 * (1.0 - ft) * (-((xf - fx) / (spread * scale)).powi(2)).exp();
                }
                let i = i.clamp(0.0, 1.6);
                let (mut r, mut g, mut b) = palette(&cool, gx / span_w * spread - t * speed, warm);
                let o = orange.clamp(0.0, 1.0);
                r += (255.0 - r) * o;
                g += (140.0 - g) * o;
                b += (0.0 - b) * o;
                let i = i.max(o * 1.2);
                let line_alpha = (0.55 + 0.45 * i).min(1.0);
                for d in 0..depth {
                    // An der Leiste die Linie, davor weiches Ausstrahlen (d = 0 ist am
                    // weitesten weg); pixel_at dreht bzw. spiegelt je nach Kante.
                    let a = if d >= glow { line_alpha } else { (i * fade[d as usize]).min(1.0) };
                    let (bx, by) = pixel_at(edge, pos, d, depth);
                    let idx = ((by * w + bx) * 4) as usize;
                    // Vormultipliziertes BGRA für UpdateLayeredWindow.
                    pixels[idx] = (b * a) as u8;
                    pixels[idx + 1] = (g * a) as u8;
                    pixels[idx + 2] = (r * a) as u8;
                    pixels[idx + 3] = (a * 255.0) as u8;
                }
            }
            jobs.push((sf.hwnd, sf.bmp, POINT { x: sf.x, y: sf.y }, SIZE { cx: w, cy: h }));
        }
        (s.dc, jobs)
    }) else {
        return;
    };
    if jobs.is_empty() {
        return;
    }
    let screen = GetDC(null_mut());
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    for (hwnd, bmp, pos, size) in jobs {
        let old = SelectObject(dc, bmp);
        UpdateLayeredWindow(hwnd, screen, &pos, &size, dc, &src, 0, &blend, ULW_ALPHA);
        SelectObject(dc, old);
    }
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
    let screen = GetDC(null_mut());
    let dc = CreateCompatibleDC(screen);
    ReleaseDC(null_mut(), screen);
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            inst,
            start: Instant::now(),
            visible: false,
            dpi: 96,
            dc,
            surfaces: Vec::new(),
            span: (0, 1),
            edge: Edge::Bottom,
            hover_x: None,
            hover_pos: 0.0,
            hover_amt: 0.0,
            attention: Vec::new(),
            flashes: Vec::new(),
        })
    });
    sync();
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAR: RECT = RECT { left: 100, top: 200, right: 172, bottom: 1000 };

    #[test]
    fn surface_sits_on_inner_edge() {
        let (glow, line) = (22, 2);
        // Unten: über der Leiste, Linie auf deren Oberkante.
        let r = surface_rect(Edge::Bottom, BAR, glow, line);
        assert_eq!((r.left, r.right, r.top, r.bottom), (100, 172, 178, 202));
        // Oben: unter der Leiste, Linie auf deren Unterkante.
        let r = surface_rect(Edge::Top, BAR, glow, line);
        assert_eq!((r.left, r.right, r.top, r.bottom), (100, 172, 998, 1022));
        // Links: rechts neben der Leiste, über ihre ganze Höhe.
        let r = surface_rect(Edge::Left, BAR, glow, line);
        assert_eq!((r.left, r.right, r.top, r.bottom), (170, 194, 200, 1000));
        // Rechts: links neben der Leiste.
        let r = surface_rect(Edge::Right, BAR, glow, line);
        assert_eq!((r.left, r.right, r.top, r.bottom), (78, 102, 200, 1000));
    }

    #[test]
    fn line_pixels_touch_the_bar() {
        let depth = 24;
        // Die Linie (d = depth - 1) liegt immer auf der Seite der Leiste,
        // d = 0 am weitesten weg.
        assert_eq!(pixel_at(Edge::Bottom, 5, depth - 1, depth), (5, depth - 1));
        assert_eq!(pixel_at(Edge::Bottom, 5, 0, depth), (5, 0));
        assert_eq!(pixel_at(Edge::Top, 5, depth - 1, depth), (5, 0));
        assert_eq!(pixel_at(Edge::Left, 5, depth - 1, depth), (0, 5));
        assert_eq!(pixel_at(Edge::Left, 5, 0, depth), (depth - 1, 5));
        assert_eq!(pixel_at(Edge::Right, 5, depth - 1, depth), (depth - 1, 5));
        assert_eq!(pixel_at(Edge::Right, 5, 0, depth), (0, 5));
        // Jede Kante füllt ihren Puffer lückenlos und ohne Doppelungen.
        for edge in [Edge::Left, Edge::Top, Edge::Right, Edge::Bottom] {
            let len = 7;
            let (w, h) = if edge.vertical() { (depth, len) } else { (len, depth) };
            let mut seen = std::collections::HashSet::new();
            for a in 0..len {
                for d in 0..depth {
                    let (x, y) = pixel_at(edge, a, d, depth);
                    assert!((0..w).contains(&x) && (0..h).contains(&y));
                    assert!(seen.insert((x, y)));
                }
            }
            assert_eq!(seen.len() as i32, w * h);
        }
    }
}
