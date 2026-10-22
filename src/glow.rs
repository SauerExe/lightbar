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
//! DIB-Puffer – rund 40 000 Pixel pro Leiste und Bild. 30 fps nur, solange
//! sich etwas bewegt (Hover, Blinken, Lichtimpuls, Komet), sonst 2 fps; bei
//! gesperrter Sitzung oder ausgeschaltetem Bildschirm steht der Takt. Auf
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
/// Bildtakt, solange sich etwas bewegt (Hover, Blinken, Impuls, Komet).
const FAST_MS: u32 = 33;
/// Ruhetakt: nur der langsam fließende Verlauf.
const SLOW_MS: u32 = 500;
/// Niemand am Rechner (keine Eingabe seit `DORMANT_S`): kaum noch Bilder, keine Kometen.
const DORMANT_MS: u32 = 5000;
const DORMANT_S: u32 = 120;
/// Pause zwischen zwei Kometen: kurz, solange an der Leiste etwas passiert
/// (bis `AWAKE_S` nach Hover, Impuls oder Aufmerksamkeit), sonst länger.
const COMET_GAP_AWAKE: f32 = 5.4;
/// Ruhig: 2 Bilder/s plus ~48 Kometenbilder alle ~2,5 min ≈ 2,3 Aufwachen/s
/// (mit kurzer Pause wären es ~8/s – der Komet ist das Teuerste im Leerlauf).
const COMET_GAP_QUIET: f32 = 150.0;
const AWAKE_S: f32 = 60.0;
/// So lange pulsiert das Orange einer App, die Aufmerksamkeit will; danach
/// leuchtet es ruhig (wie die Buttons der Leiste).
pub const ATTENTION_PULSE_S: f32 = 7.0;
/// Wärme (Usage-Auslastung) höchstens so oft neu berechnen.
const WARMTH_TTL_S: f32 = 30.0;
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
    /// Stellen, die Aufmerksamkeit wollen, mit Beginn des Blinkens.
    attention: Vec<(i32, Instant)>,
    flashes: Vec<Flash>,
    /// Stil aus den Einstellungen (in `sync` gelesen, nicht in jedem Bild).
    style: GlowStyle,
    /// Gecachte Wärme und wann sie berechnet wurde (None = neu berechnen).
    warm: f32,
    warm_at: Option<Instant>,
    /// Beginn des aktuellen bzw. nächsten Kometen (Sekunden seit `start`).
    comet_at: f32,
    /// Zuletzt etwas an der Leiste passiert (Hover, Impuls, Aufmerksamkeit).
    activity: Instant,
    /// Sitzung gesperrt/getrennt bzw. Bildschirm aus (PAUSE_*-Bits): kein Takt.
    paused: u8,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// Fenster, an dem der Bildtakt für alle Leuchtfenster hängt.
    static TIMER_OWNER: Cell<HWND> = const { Cell::new(null_mut()) };
    /// Aktuell gesetzter Takt in ms (0 = keiner).
    static TIMER_MS: Cell<u32> = const { Cell::new(0) };
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
    let changed = with(|s| {
        if s.hover_x == x {
            return false;
        }
        s.hover_x = x;
        if x.is_some() {
            s.activity = Instant::now();
            // Die Maus bewegt sich über der Leiste: Sitzung offen, Bildschirm an
            // (Sicherheitsnetz, falls eine Entsperr-Meldung verloren ging).
            s.paused = 0;
        }
        true
    })
    .unwrap_or(false);
    if changed {
        unsafe { wake() };
    }
}

/// Stellen, die Aufmerksamkeit wollen (Buttons), entlang der Leiste, mit dem
/// Beginn ihres Blinkens: pulsieren `ATTENTION_PULSE_S` lang, dann ruhig.
pub fn set_attention(xs: Vec<(i32, Instant)>) {
    let changed = with(|s| {
        if s.attention == xs {
            return false;
        }
        if !xs.is_empty() {
            s.activity = Instant::now();
        }
        s.attention = xs;
        true
    })
    .unwrap_or(false);
    if changed {
        unsafe { wake() };
    }
}

/// Einmaliger Lichtimpuls an einer Stelle entlang der Leiste (z. B. App gestartet).
pub fn flash(at: i32) {
    with(|s| {
        s.flashes.push(Flash { at, start: Instant::now() });
        s.activity = Instant::now();
    });
    unsafe { wake() };
}

/// Usage hat sich geändert: Wärme beim nächsten Bild neu berechnen.
pub fn usage_changed() {
    with(|s| s.warm_at = None);
}

/// Gründe, den Takt anzuhalten (Bits in `State::paused`).
pub const PAUSE_LOCKED: u8 = 1;
pub const PAUSE_DISCONNECTED: u8 = 2;
pub const PAUSE_DISPLAY_OFF: u8 = 4;

/// Grund zum Anhalten setzen (Sitzung gesperrt, getrennt, Bildschirm aus) oder
/// aufheben; läuft wieder, sobald kein Grund mehr besteht.
pub unsafe fn set_paused(reason: u8, on: bool) {
    with(|s| if on { s.paused |= reason } else { s.paused &= !reason });
    reschedule_now();
}

/// Nach Pause/Fortsetzen: sofort ein Bild (bzw. Takt anhalten).
unsafe fn reschedule_now() {
    let paused = with(|s| s.paused != 0).unwrap_or(true);
    if paused {
        set_interval(0);
    } else {
        frame();
    }
}

/// Etwas Bewegtes beginnt: vom Ruhetakt sofort auf den schnellen Takt.
unsafe fn wake() {
    let ms = TIMER_MS.with(|t| t.get());
    if ms == 0 || ms > FAST_MS {
        let paused = with(|s| !s.visible || s.paused != 0).unwrap_or(true);
        if !paused {
            set_interval(FAST_MS);
        }
    }
}

/// Bildtakt setzen (0 = anhalten); unverändert nicht neu setzen.
unsafe fn set_interval(ms: u32) {
    let owner = TIMER_OWNER.with(|t| t.get());
    if owner.is_null() {
        TIMER_MS.with(|t| t.set(0));
        return;
    }
    if TIMER_MS.with(|t| t.replace(ms)) == ms {
        return;
    }
    if ms == 0 {
        KillTimer(owner, TIMER_FRAME);
    } else {
        SetTimer(owner, TIMER_FRAME, ms, None);
    }
}

/// Sekunden ohne Tastatur- oder Mauseingabe (ganze Sitzung).
unsafe fn input_idle_s() -> u32 {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut li = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if GetLastInputInfo(&mut li) == 0 {
        return 0;
    }
    GetTickCount().wrapping_sub(li.dwTime) / 1000
}

/// Was den nächsten Takt bestimmt (reine Logik, testbar).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Pace {
    /// Hover gleitet noch, Impuls läuft, Aufmerksamkeit pulsiert.
    effects: bool,
    /// Komet unterwegs.
    comet: bool,
    /// Sekunden bis zum nächsten Kometen (falls keiner unterwegs).
    comet_in: f32,
    /// Niemand am Rechner.
    dormant: bool,
}

/// Nächster Takt in ms: schnell bei Bewegung, sonst Ruhetakt – aber
/// rechtzeitig zum Start des nächsten Kometen wach.
fn next_interval(p: Pace) -> u32 {
    if p.effects || p.comet {
        return FAST_MS;
    }
    if p.dormant {
        return DORMANT_MS;
    }
    let until = (p.comet_in * 1000.0).ceil().max(0.0) as u32;
    until.clamp(FAST_MS, SLOW_MS)
}

/// Pause bis zum nächsten Kometen: kurz, solange an der Leiste kürzlich etwas los war.
fn comet_gap(since_activity: f32) -> f32 {
    if since_activity < AWAKE_S {
        COMET_GAP_AWAKE
    } else {
        COMET_GAP_QUIET
    }
}

/// Beginn des nächsten Kometen nach einem Bild zur Zeit `t`: Ist der aktuelle
/// durch, kommt der nächste nach der Pause; wird es wieder lebhaft, früher;
/// ist niemand am Rechner, wartet er.
fn next_comet(comet_at: f32, t: f32, travel: f32, since_activity: f32, dormant: bool) -> f32 {
    let gap = comet_gap(since_activity);
    let mut at = comet_at;
    if t >= at + travel {
        at = t + gap;
    }
    if t < at {
        at = at.min(t + gap);
        if dormant {
            at = at.max(t + COMET_GAP_AWAKE);
        }
    }
    at
}

/// Pulsstärke (0..1) einer Aufmerksamkeitsstelle: erst pulsieren, dann ruhig voll.
fn attention_pulse(t: f32, elapsed: f32) -> f32 {
    if elapsed < ATTENTION_PULSE_S {
        0.5 + 0.5 * (t * 6.0).sin()
    } else {
        1.0
    }
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
    let style = crate::settings::get().glow;
    with(|s| {
        s.visible = !surfaces.is_empty();
        s.surfaces = surfaces;
        s.dpi = dpi;
        s.span = span;
        s.edge = edge;
        s.style = style;
    });
    // Ein Takt für alle Fenster, am ersten Leuchtfenster.
    let prev = TIMER_OWNER.with(|t| t.replace(first));
    if prev != first {
        if !prev.is_null() && IsWindow(prev) != 0 {
            KillTimer(prev, TIMER_FRAME);
        }
        TIMER_MS.with(|t| t.set(0));
    }
    if first.is_null() {
        set_interval(0);
    } else {
        // Ein Bild und der passende Takt (bei Pause: keiner).
        reschedule_now();
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
    // Wärme nur gelegentlich neu (Mutex + Kopien in usage::current), nicht je Bild.
    if with(|s| s.warm_at.is_none_or(|at| at.elapsed().as_secs_f32() > WARMTH_TTL_S)).unwrap_or(false) {
        let w = warmth();
        with(|s| {
            s.warm = w;
            s.warm_at = Some(Instant::now());
        });
    }
    let dormant = input_idle_s() >= DORMANT_S;
    let Some((dc, jobs, pace)) = with(|s| {
        if !s.visible || s.paused != 0 {
            return (s.dc, Vec::new(), None);
        }
        let warm = s.warm;
        let style = s.style;
        let cool = stops(style);
        // Neon fließt schneller und über mehr Farbe pro Breite.
        let (spread, speed) = if style == GlowStyle::Neon { (2.4, 0.16) } else { (1.4, 0.06) };
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
            None => {
                s.hover_amt *= 0.85;
                if s.hover_amt < 0.01 {
                    s.hover_amt = 0.0;
                }
            }
        }
        s.flashes.retain(|f| f.start.elapsed().as_secs_f32() < 1.2);

        // Komet: von links nach rechts (seitlich von oben nach unten) über alle
        // Leisten (bei breiterer Gesamtfläche entsprechend länger unterwegs),
        // danach Pause – kurz, solange an der Leiste etwas los ist, sonst länger.
        let travel = 1.6 * (span_w / (1920.0 * scale)).max(1.0);
        s.comet_at = next_comet(s.comet_at, t, travel, s.activity.elapsed().as_secs_f32(), dormant);
        let ct = (t - s.comet_at) / travel;
        let comet = (0.0..1.0).contains(&ct).then(|| span_l + ct * (span_w + 400.0 * scale) - 200.0 * scale);
        let attention: Vec<(f32, f32)> = s.attention.iter().map(|&(x, t0)| (x as f32, attention_pulse(t, t0.elapsed().as_secs_f32()))).collect();
        // Hover-Glühen ruht erst, wenn es ganz aus oder an der Maus angekommen ist.
        let hover_moving = match s.hover_x {
            Some(x) => s.hover_amt < 0.99 || (s.hover_pos - x as f32).abs() > 0.5,
            None => s.hover_amt > 0.0,
        };
        let pace = Pace {
            effects: hover_moving || !s.flashes.is_empty() || s.attention.iter().any(|(_, t0)| t0.elapsed().as_secs_f32() < ATTENTION_PULSE_S),
            comet: comet.is_some(),
            comet_in: s.comet_at - t,
            dormant,
        };
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
                for &(ax, pulse) in &attention {
                    orange += (0.6 + 0.6 * pulse) * (-((xf - ax) / (60.0 * scale)).powi(2)).exp();
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
        (s.dc, jobs, Some(pace))
    }) else {
        return;
    };
    // Nächster Takt nach dem, was sich gerade bewegt (keiner bei Pause/unsichtbar).
    set_interval(pace.map(next_interval).unwrap_or(0));
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
            style: crate::settings::get().glow,
            warm: 0.0,
            warm_at: None,
            comet_at: 0.0,
            activity: Instant::now(),
            paused: 0,
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
    fn pace_fast_only_while_moving() {
        let idle = Pace { effects: false, comet: false, comet_in: 30.0, dormant: false };
        assert_eq!(next_interval(idle), SLOW_MS);
        assert_eq!(next_interval(Pace { effects: true, ..idle }), FAST_MS);
        assert_eq!(next_interval(Pace { comet: true, ..idle }), FAST_MS);
        // Rechtzeitig zum Kometen wach, aber nie schneller als der Bildtakt.
        assert_eq!(next_interval(Pace { comet_in: 0.2, ..idle }), 200);
        assert_eq!(next_interval(Pace { comet_in: 0.0, ..idle }), FAST_MS);
        assert_eq!(next_interval(Pace { comet_in: -1.0, ..idle }), FAST_MS);
        assert_eq!(next_interval(Pace { dormant: true, ..idle }), DORMANT_MS);
        // Bewegung schlägt "niemand da".
        assert_eq!(next_interval(Pace { dormant: true, effects: true, ..idle }), FAST_MS);
    }

    #[test]
    fn comet_cadence() {
        let travel = 1.6;
        // Unterwegs bleibt der Start stehen.
        assert_eq!(next_comet(10.0, 10.5, travel, 0.0, false), 10.0);
        // Durch: nächster nach der kurzen Pause, solange etwas los ist …
        assert_eq!(next_comet(10.0, 11.7, travel, 5.0, false), 11.7 + COMET_GAP_AWAKE);
        // … sonst nach der langen.
        assert_eq!(next_comet(10.0, 11.7, travel, 600.0, false), 11.7 + COMET_GAP_QUIET);
        // Wieder lebhaft: der ferne Komet kommt früher.
        assert_eq!(next_comet(50.0, 20.0, travel, 0.0, false), 20.0 + COMET_GAP_AWAKE);
        // Niemand am Rechner: der Komet wartet.
        assert_eq!(next_comet(20.1, 20.0, travel, 600.0, true), 20.0 + COMET_GAP_AWAKE);
    }

    #[test]
    fn attention_pulses_then_stays() {
        assert!(attention_pulse(0.3, 1.0) < 1.0);
        assert_eq!(attention_pulse(0.3, ATTENTION_PULSE_S + 0.1), 1.0);
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
