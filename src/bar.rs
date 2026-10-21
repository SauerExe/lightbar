//! Die Leiste selbst: AppBar am unteren Rand, Fensterliste über den Shell-Hook,
//! angeheftete Apps, Infobereich, Status-Symbole, Uhr – doppelt gepuffert in GDI.
//!
//! Die Fensterklasse heißt "Shell_TrayWnd": So finden Apps (Shell_NotifyIcon)
//! und Tools, die nach "der Taskleiste" suchen, unsere statt Explorers.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::time::Instant;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::anim;
use crate::menus::{self, Item};
use crate::status::{self, Status};
use crate::tray::{self, Click};
use crate::util::*;

// Maße in logischen Pixeln (96 DPI), werden pro Monitor-DPI skaliert.
const BAR_H: i32 = 48;
/// Breite einer Leiste am linken oder rechten Rand.
const BAR_W_V: i32 = 72;
const BTN_W: i32 = 44;
const BTN_H: i32 = 40;
const BTN_GAP: i32 = 4;
const ICON: i32 = 24;
const CLOCK_W: i32 = 84;
const TRAY_W: i32 = 28;
const TRAY_ICON: i32 = 16;
const STATUS_GLYPH_W: i32 = 26;
const DESKTOP_W: i32 = 8;

// WM_APP statt WM_USER: Fremde Tools schicken Shell_TrayWnd eigene WM_USER-Nachrichten.
const WM_APPBAR: u32 = WM_APP + 1;
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_TICK: usize = 1;
const TIMER_TIP: usize = 2;
const TIMER_ANIM: usize = 3;
const TIMER_CPU: usize = 5;
const TIMER_SLOW: usize = 6;
const TIMER_REFRESH: usize = 7;
/// Solange eine Leiste wegen Vollbild ausgeblendet ist: prüfen, ob es noch eins ist
/// (Vollbild kann enden, ohne dass das Vordergrundfenster wechselt).
const TIMER_FULL: usize = 8;
/// Kurz nach einer fremden Änderung des Arbeitsbereichs: unseren Platz prüfen.
const TIMER_WORKAREA: usize = 9;
/// Einmal kurz nach der Übernahme: Explorer blendet seine Leiste beim Wechsel
/// auf "automatisch ausblenden" selbst wieder ein.
const TIMER_REHIDE: usize = 10;

/// Explorers Leiste kurz nach der Übernahme noch einmal ausblenden.
pub unsafe fn schedule_rehide() {
    SetTimer(hwnd(), TIMER_REHIDE, 3000, None);
}
pub const WM_BAR_WINX: u32 = WM_APP + 30;
pub const WM_BAR_NUM: u32 = WM_APP + 31;
pub const WM_BAR_QUICK: u32 = WM_APP + 32;
const WM_BAR_USAGE: u32 = WM_APP + 33;
const WM_BAR_INVOKE: u32 = WM_APP + 34;
pub const WM_BAR_FOCUS: u32 = WM_APP + 35;
/// Lautstärke/Netz/Akku geändert (gepostet aus status).
pub const WM_BAR_STATUS: u32 = WM_APP + 36;
const WM_BAR_EVENT: u32 = WM_APP + 37;
const USAGE_W: i32 = 236;
const TASKMGR_W: i32 = 40;
const VK_KEY_D: VIRTUAL_KEY = 0x44;

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    None,
    Start,
    Task(usize),
    Tray(usize),
    Chevron,
    /// "…": Apps, für die in der Leiste kein Platz mehr ist.
    More,
    Status,
    Clock,
    Desktop,
    Usage,
    TaskMgr,
}

#[derive(Clone)]
struct Drag {
    slot: String,
    start_x: i32,
    x: i32,
    /// Erst ab ein paar Pixeln Bewegung wird aus dem Klick ein Ziehen.
    active: bool,
}

/// Ein Button: alle Fenster einer App (wie bei Windows 11 gruppiert).
#[derive(Clone)]
struct Group {
    /// Fester Platz in der Reihenfolge: Anheft-Eintrag bei angehefteten Apps,
    /// sonst der Gruppenschlüssel. Bleibt gleich, wenn eine angeheftete App
    /// ein Fenster öffnet (und ihr Schlüssel z. B. zur AppUserModelID wird).
    slot: String,
    /// "aumid:<AppUserModelID>" oder "exe:<pfad>" – Gruppier- und Anheft-Schlüssel.
    key: String,
    /// Kleingeschriebener exe-Pfad, soweit ermittelbar.
    exe: Option<String>,
    /// Offene Fenster in stabiler Reihenfolge; leer = nur angeheftet.
    windows: Vec<HWND>,
    icon: HICON,
    /// Selbst erzeugt und damit unser – muss mit DestroyIcon freigegeben werden.
    owned: bool,
}

struct State {
    dpi: u32,
    /// Größe, in der die gecachten Icons erzeugt wurden.
    icon_px: i32,
    shell_msg: u32,
    created_msg: u32,
    groups: Vec<Group>,
    /// Je Fenster: (exe, Gruppenschlüssel) – einmal ermittelt, dann gecacht.
    info: HashMap<usize, (Option<String>, String)>,
    pins: Vec<String>,
    /// Vom Nutzer per Ziehen festgelegte Reihenfolge (Platz-Schlüssel).
    order: Vec<String>,
    /// Laufendes Ziehen eines Buttons.
    drag: Option<Drag>,
    /// Tastaturfokus (Win+T) auf einem Element aus ui_elements.
    kb_focus: Option<usize>,
    status: Option<Status>,
    hover: Hit,
    /// Leiste, über der hover gilt (Haupt- oder Zweitleiste).
    hover_hwnd: HWND,
    /// Leiste, deren Mausverlassen gerade verfolgt wird.
    tracking: HWND,
    /// Eine Vollbild-App ist vorne: Leiste ausgeblendet.
    fullscreen: bool,
    anim: Anim,
    /// Systemlast fürs Task-Manager-Symbol (0..100) und letzte Messwerte.
    cpu: f32,
    ram: f32,
    cpu_times: (u64, u64),
    taskmgr_icon: HICON,
    /// Fenster, die blinken (Aufmerksamkeit wollen), bis sie aktiviert werden.
    attention: Vec<HWND>,
    start: Instant,
    font: HFONT,
    icons: HFONT,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Kurzer Zugriff auf den Zustand. Nie über Win32-Aufrufe hinweg halten,
/// die Nachrichten zurück in unsere WndProc schicken können.
fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| {
        let mut g = s.try_borrow_mut().ok()?;
        g.as_mut().map(f)
    })
}

static BAR_HWND: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

pub fn hwnd() -> HWND {
    BAR_HWND.load(Ordering::Relaxed)
}

// ------------------------------------------------------------- Anheften

fn pins_path() -> Option<PathBuf> {
    Some(crate::util::data_dir()?.join("taskbar.txt"))
}

/// Angeheftete Gruppenschlüssel. Ältere Dateien enthalten nackte exe-Pfade.
fn load_pins() -> Vec<String> {
    pins_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| if l.starts_with("exe:") || l.starts_with("aumid:") { l.to_string() } else { exe_key(l) })
                .collect()
        })
        .unwrap_or_default()
}

fn exe_key(exe: &str) -> String {
    format!("exe:{}", normalize_exe(exe))
}

/// Kleinschreibung und Squirrel-Versionsordner entfernen: Discord, Slack & Co.
/// liegen in …\discord\app-1.0.9264\discord.exe; als Schlüssel zählt
/// …\discord\discord.exe, damit Anheften Updates überlebt.
pub fn normalize_exe(exe: &str) -> String {
    let lower = exe.to_lowercase();
    let parts: Vec<&str> = lower.split('\\').collect();
    let is_version_dir = |p: &str| p.strip_prefix("app-").is_some_and(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'));
    parts.iter().filter(|p| !is_version_dir(p)).copied().collect::<Vec<_>>().join("\\")
}

/// Startbare Form eines (normalisierten) exe-Pfads. Bei Squirrel-Apps: der
/// Update.exe-Starter und die aktuellste echte exe (fürs Icon).
enum LaunchTarget {
    Exe(String),
    Squirrel { update: String, exe_name: String, newest: Option<String> },
}

fn resolve_launch(exe: &str) -> Option<LaunchTarget> {
    let path = Path::new(exe);
    let dir = path.parent()?;
    let update = dir.join("Update.exe");
    if update.exists() {
        let name = path.file_name()?.to_string_lossy().into_owned();
        let newest = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().to_lowercase().starts_with("app-"))
            .map(|e| e.path().join(&name))
            .filter(|p| p.exists())
            .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .map(|p| p.to_string_lossy().into_owned());
        return Some(LaunchTarget::Squirrel { update: update.to_string_lossy().into_owned(), exe_name: name, newest });
    }
    path.exists().then(|| LaunchTarget::Exe(exe.to_string()))
}

/// Pfad, aus dem sich ein Icon lesen lässt.
fn icon_source(exe: &str) -> Option<String> {
    match resolve_launch(exe)? {
        LaunchTarget::Exe(p) => Some(p),
        LaunchTarget::Squirrel { newest, .. } => newest,
    }
}

fn save_pins(pins: &[String]) {
    if let Some(p) = pins_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, pins.join("\n"));
    }
}

unsafe fn toggle_key(key: &str) {
    with(|s| {
        if let Some(i) = s.pins.iter().position(|p| p == key) {
            s.pins.remove(i);
        } else {
            s.pins.push(key.to_string());
        }
        save_pins(&s.pins);
    });
    refresh(hwnd(), None);
}

/// Von außen (Startmenü) eine exe an die Taskleiste heften oder lösen.
pub unsafe fn toggle_pin(exe: &str) {
    toggle_key(&exe_key(exe));
}

pub fn is_pinned(exe: &str) -> bool {
    let key = exe_key(exe);
    with(|s| s.pins.contains(&key)).unwrap_or(false)
}

// ---------------------------------------------------------------- Layout

/// Nicht vorhandene Elemente haben ein leeres Rechteck (trifft nie, zeichnet nichts).
struct Layout {
    usage: RECT,
    show_usage: bool,
    taskmgr: RECT,
    start: RECT,
    /// Kann kürzer sein als die Gruppenliste, wenn nicht alle Buttons passen.
    tasks: Vec<RECT>,
    tray: Vec<RECT>,
    chevron: Option<RECT>,
    /// "…"-Knopf, wenn nicht alle App-Buttons Platz haben.
    more: Option<RECT>,
    status: RECT,
    clock: RECT,
    desktop: RECT,
}

struct LayoutInput {
    /// Breite des Fensters.
    width: i32,
    /// Senkrechte Leiste (links/rechts): Länge = Höhe des Fensters.
    vertical: bool,
    length: i32,
    dpi: u32,
    n_tasks: usize,
    /// Angezeigte Tray-Icons (ohne die, die Windows ohnehin ausblendet).
    n_tray: usize,
    /// Gibt es ausgeblendete Tray-Icons (Überlauf-Pfeil nötig)?
    hidden_tray: bool,
    battery: bool,
    /// Hauptleiste: mit Infobereich, Status, Usage und Task-Manager. Zweitleisten
    /// zeigen wie bei Windows nur Start, Apps und Uhr.
    primary: bool,
    usage: bool,
    align_left: bool,
}

/// Kleinste Buttonbreite, bevor Buttons wegfallen (Symbol 24 px + Rand).
const BTN_MIN_W: i32 = 32;

/// Aufteilung der Leiste. Wird es eng (Hochkant-Bildschirm, viele Apps),
/// weicht die Leiste stufenweise aus: Tray-Icons wandern in den Überlauf,
/// dann verschwindet die Usage-Anzeige, dann werden die Buttons schmaler und
/// zuletzt fallen Buttons am Ende weg, statt sich mit dem Rest zu überlappen.
fn layout(i: &LayoutInput) -> Layout {
    if i.vertical {
        return layout_vertical(i);
    }
    let (w, dpi) = (i.width, i.dpi);
    let gap = px(dpi, BTN_GAP);
    let h = px(dpi, BAR_H);
    let top = (h - px(dpi, BTN_H)) / 2;
    let bottom = top + px(dpi, BTN_H);
    let empty = RECT { left: 0, top, right: 0, bottom };

    // Rechte Seite von außen nach innen.
    let desktop = RECT { left: w - px(dpi, DESKTOP_W), top: 0, right: w, bottom: h };
    let clock_r = desktop.left - px(dpi, 4);
    let clock = RECT { left: clock_r - px(dpi, CLOCK_W), top, right: clock_r, bottom };
    let status = if i.primary {
        let glyphs = 2 + i.battery as i32;
        let status_r = clock.left - px(dpi, 4);
        RECT { left: status_r - glyphs * px(dpi, STATUS_GLYPH_W) - px(dpi, 16), top, right: status_r, bottom }
    } else {
        RECT { left: clock.left, right: clock.left, ..empty }
    };
    let tray_r = status.left - px(dpi, 4);
    let tw = px(dpi, TRAY_W);
    let n_tray = if i.primary { i.n_tray } else { 0 };
    let chevron_needed = |shown: usize| i.primary && (i.hidden_tray || shown < n_tray);
    let right_start = |shown: usize| tray_r - (shown as i32 + chevron_needed(shown) as i32) * tw;

    // Linke Seite: Usage, dann Task-Manager.
    let margin = px(dpi, 8);
    let left_end = |usage: bool| match (i.primary, usage) {
        (false, _) => 0,
        (true, true) => margin + px(dpi, USAGE_W) + px(dpi, 4) + px(dpi, TASKMGR_W),
        (true, false) => margin + px(dpi, TASKMGR_W),
    };

    let count = i.n_tasks as i32 + 1;
    let need = |bw: i32| count * bw + (count - 1) * gap;
    let room = |usage: bool, shown: usize| right_start(shown) - left_end(usage) - 2 * gap;
    let (mut usage, mut shown, mut bw) = (i.primary && i.usage, n_tray, px(dpi, BTN_W));
    while need(bw) > room(usage, shown) {
        if shown > 0 {
            shown -= 1;
        } else if usage {
            usage = false;
        } else {
            bw = ((room(usage, shown) + gap) / count - gap).max(px(dpi, BTN_MIN_W));
            break;
        }
    }

    // Apps: mittig (über die ganze Breite) oder links, wie in Windows eingestellt;
    // nie über die Elemente links und rechts hinaus.
    let lo = left_end(usage) + gap;
    let hi = right_start(shown) - gap;
    let fit = (((hi - lo + gap) / (bw + gap)).max(1) as usize).min(count as usize);
    let total = need(bw).min(fit as i32 * (bw + gap) - gap);
    let mut x = if i.align_left { lo } else { ((w - total) / 2).clamp(lo, (hi - total).max(lo)) };
    let mut next = || {
        let r = RECT { left: x, top, right: x + bw, bottom };
        x += bw + gap;
        r
    };
    let start = next();
    // Passen nicht alle, wird der letzte Platz zum "…"-Knopf für den Rest.
    let overflow = fit - 1 < i.n_tasks && fit >= 2;
    let n_shown = if overflow { fit - 2 } else { (fit - 1).min(i.n_tasks) };
    let tasks = (0..n_shown).map(|_| next()).collect();
    let more = overflow.then(|| next());

    let tray_l = tray_r - shown as i32 * tw;
    let tray = (0..shown as i32).map(|n| RECT { left: tray_l + n * tw, top, right: tray_l + (n + 1) * tw, bottom }).collect();
    let chevron = chevron_needed(shown).then(|| RECT { left: tray_l - tw, top, right: tray_l, bottom });
    let usage_r = if usage { RECT { left: margin, top, right: margin + px(dpi, USAGE_W), bottom } } else { empty };
    let taskmgr = if i.primary {
        // Direkt rechts neben der Usage-Anzeige (oder ganz links, wenn die aus ist).
        let tm_left = if usage { usage_r.right + px(dpi, 4) } else { margin };
        RECT { left: tm_left, top, right: tm_left + px(dpi, TASKMGR_W), bottom }
    } else {
        empty
    };
    Layout { usage: usage_r, show_usage: usage, taskmgr, start, tasks, tray, chevron, more, status, clock, desktop }
}

/// Aufteilung einer senkrechten Leiste (links oder rechts): oben der
/// Task-Manager, dann Start und Apps (oben oder mittig, wie in Windows
/// eingestellt), unten Infobereich als Raster, Status, Uhr und die Ecke
/// "Desktop anzeigen". Bei Platzmangel wie waagerecht: Tray-Icons in den
/// Überlauf, flachere Buttons, "…"-Knopf. Die Usage-Anzeige passt nicht in
/// eine Spalte und entfällt.
fn layout_vertical(i: &LayoutInput) -> Layout {
    let (w, len, dpi) = (i.width, i.length, i.dpi);
    let gap = px(dpi, BTN_GAP);
    let margin = px(dpi, 4);
    let (left, right) = (margin, w - margin);
    let row = |top: i32, h: i32| RECT { left, top, right, bottom: top + h };
    let empty = RECT { left, top: 0, right: left, bottom: 0 };

    // Unten von außen nach innen.
    let desktop = RECT { left: 0, top: len - px(dpi, DESKTOP_W), right: w, bottom: len };
    let clock = row(desktop.top - px(dpi, 4) - px(dpi, 44), px(dpi, 44));
    let status = if i.primary { row(clock.top - px(dpi, 4) - px(dpi, 36), px(dpi, 36)) } else { RECT { top: clock.top, bottom: clock.top, ..empty } };
    let n_tray = if i.primary { i.n_tray } else { 0 };
    let cell = px(dpi, TRAY_W);
    let cols = ((right - left) / cell).max(1);
    let chevron_needed = |shown: usize| i.primary && (i.hidden_tray || shown < n_tray);
    let tray_rows = |shown: usize| (shown as i32 + chevron_needed(shown) as i32 + cols - 1) / cols;
    let tray_top = |shown: usize| status.top - px(dpi, 4) - tray_rows(shown) * cell;

    let top_end = if i.primary { px(dpi, 8) + px(dpi, TASKMGR_W) } else { 0 };
    let count = i.n_tasks as i32 + 1;
    let need = |bh: i32| count * bh + (count - 1) * gap;
    let room = |shown: usize| tray_top(shown) - top_end - 2 * gap;
    let (mut shown, mut bh) = (n_tray, px(dpi, BTN_H));
    while need(bh) > room(shown) {
        if shown > 0 {
            shown -= 1;
        } else {
            bh = ((room(shown) + gap) / count - gap).max(px(dpi, BTN_MIN_W));
            break;
        }
    }

    let lo = top_end + gap;
    let hi = tray_top(shown) - gap;
    let fit = (((hi - lo + gap) / (bh + gap)).max(1) as usize).min(count as usize);
    let total = need(bh).min(fit as i32 * (bh + gap) - gap);
    let mut y = if i.align_left { lo } else { ((len - total) / 2).clamp(lo, (hi - total).max(lo)) };
    let mut next = || {
        let r = row(y, bh);
        y += bh + gap;
        r
    };
    let start = next();
    let overflow = fit - 1 < i.n_tasks && fit >= 2;
    let n_shown = if overflow { fit - 2 } else { (fit - 1).min(i.n_tasks) };
    let tasks = (0..n_shown).map(|_| next()).collect();
    let more = overflow.then(|| next());

    // Raster: erst der Überlauf-Pfeil, dann die Icons, zeilenweise.
    let grid_left = left + (right - left - cols * cell) / 2;
    let at = |n: i32| {
        let (c, r) = (n % cols, n / cols);
        let (x, y) = (grid_left + c * cell, tray_top(shown) + r * cell);
        RECT { left: x, top: y, right: x + cell, bottom: y + cell }
    };
    let chevron = chevron_needed(shown).then(|| at(0));
    let first = chevron.is_some() as i32;
    let tray = (0..shown as i32).map(|n| at(first + n)).collect();
    let taskmgr = if i.primary { row(px(dpi, 8), px(dpi, TASKMGR_W)) } else { empty };
    Layout { usage: empty, show_usage: false, taskmgr, start, tasks, tray, chevron, more, status, clock, desktop }
}

/// Layout einer Leiste und die Tray-Icons, die darin Platz haben. Tray-Icons,
/// die nicht mehr passen, zeigt der Überlauf.
unsafe fn current_layout(hwnd: HWND) -> Option<(Layout, Vec<(usize, HICON)>)> {
    let (n, battery) = with(|s| (s.groups.len(), s.status.as_ref().is_some_and(|x| x.battery.is_some())))?;
    let dpi = bar_dpi(hwnd);
    let primary = hwnd == self::hwnd();
    let mut tray = if primary { tray::visible() } else { Vec::new() };
    let mut rc: RECT = zeroed();
    GetClientRect(hwnd, &mut rc);
    let settings = crate::settings::get();
    let l = layout(&LayoutInput {
        width: rc.right,
        vertical: crate::display::edge().vertical(),
        length: rc.bottom,
        dpi,
        n_tasks: n,
        n_tray: tray.len(),
        hidden_tray: tray::has_hidden(),
        battery,
        primary,
        usage: settings.show_usage && !settings.presentation && (settings.usage_claude || settings.usage_codex),
        align_left: crate::display::align_left(),
    });
    if primary {
        tray::set_shown(l.tray.len());
        tray.truncate(l.tray.len());
    }
    Some((l, tray))
}

unsafe fn hit_at(hwnd: HWND, x: i32, y: i32) -> Hit {
    let Some((l, tray)) = current_layout(hwnd) else { return Hit::None };
    if inside(&l.start, x, y) {
        return Hit::Start;
    }
    if let Some(i) = l.tasks.iter().position(|r| inside(r, x, y)) {
        return Hit::Task(i);
    }
    if let Some(i) = l.tray.iter().position(|r| inside(r, x, y)) {
        return Hit::Tray(tray[i].0);
    }
    if l.more.is_some_and(|r| inside(&r, x, y)) {
        return Hit::More;
    }
    if l.chevron.is_some_and(|r| inside(&r, x, y)) {
        return Hit::Chevron;
    }
    if inside(&l.status, x, y) {
        return Hit::Status;
    }
    if inside(&l.clock, x, y) {
        return Hit::Clock;
    }
    if inside(&l.desktop, x, y) {
        return Hit::Desktop;
    }
    if inside(&l.taskmgr, x, y) {
        return Hit::TaskMgr;
    }
    if l.show_usage && inside(&l.usage, x, y) {
        return Hit::Usage;
    }
    Hit::None
}

// ------------------------------------------------------------ Fensterliste

unsafe fn is_task_window(h: HWND) -> bool {
    if IsWindowVisible(h) == 0 {
        return false;
    }
    let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
    if ex & WS_EX_APPWINDOW == 0
        && (ex & (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) != 0 || !GetWindow(h, GW_OWNER).is_null())
    {
        return false;
    }
    // Fenster anderer virtueller Desktops und angehaltene UWP-Apps sind "cloaked".
    let mut cloaked: u32 = 0;
    if DwmGetWindowAttribute(h, DWMWA_CLOAKED as _, &mut cloaked as *mut _ as *mut c_void, 4) == 0
        && cloaked != 0
    {
        return false;
    }
    let mut class = [0u16; 32];
    let len = GetClassNameW(h, class.as_mut_ptr(), class.len() as i32) as usize;
    let class = String::from_utf16_lossy(&class[..len]);
    if class == "Progman" || class == "WorkerW" {
        return false;
    }
    GetWindowTextLengthW(h) > 0
}

unsafe extern "system" fn enum_cb(h: HWND, lp: LPARAM) -> BOOL {
    if is_task_window(h) {
        (*(lp as *mut Vec<HWND>)).push(h);
    }
    1
}

unsafe fn exe_of(h: HWND) -> Option<String> {
    let mut pid = 0u32;
    GetWindowThreadProcessId(h, &mut pid);
    let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if proc.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(proc, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
    CloseHandle(proc);
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]).to_lowercase())
}

/// Icon einer exe in exakt `size` Pixeln (scharf statt hochskaliert).
unsafe fn exe_icon(exe: &str, size: i32) -> HICON {
    let mut icon: HICON = null_mut();
    let hr = SHDefExtractIconW(w16(exe).as_ptr(), 0, 0, &mut icon, null_mut(), size as u32);
    if hr == 0 {
        icon
    } else {
        null_mut()
    }
}

unsafe fn window_icon(h: HWND) -> HICON {
    for which in [ICON_BIG, ICON_SMALL2] {
        let mut r: usize = 0;
        if SendMessageTimeoutW(h, WM_GETICON, which as usize, 0, SMTO_ABORTIFHUNG, 50, &mut r) != 0
            && r != 0
        {
            return r as HICON;
        }
    }
    for idx in [GCLP_HICON, GCLP_HICONSM] {
        let r = GetClassLongPtrW(h, idx);
        if r != 0 {
            return r as HICON;
        }
    }
    LoadIconW(null_mut(), IDI_APPLICATION)
}

/// Gruppenschlüssel eines Fensters: AppUserModelID wie bei Windows, sonst exe.
unsafe fn window_key(h: HWND, exe: Option<&str>) -> String {
    if let Some(id) = crate::apps::window_app_id(h) {
        return format!("aumid:{id}");
    }
    match exe {
        Some(e) => exe_key(e),
        None => format!("hwnd:{}", h as usize),
    }
}

/// Icon einer Gruppe in exakt `size` Pixeln. Rückgabe: (Icon, gehört uns).
/// Reihenfolge: App-Katalog (AUMID, auch Store-Apps) → exe-Ressource → Fenster-Icon.
/// UWP-Fenster gehören dem Frame-Host, dessen exe-Icon wäre falsch.
unsafe fn group_icon(key: &str, exe: Option<&str>, sample: Option<HWND>, size: i32) -> (HICON, bool) {
    if let Some(id) = key.strip_prefix("aumid:") {
        let icon = crate::apps::shell_hicon(&format!("shell:AppsFolder\\{id}"), size);
        if !icon.is_null() {
            return (icon, true);
        }
    }
    if let Some(exe) = exe.filter(|e| !e.ends_with("applicationframehost.exe")) {
        let source = if Path::new(exe).exists() { Some(exe.to_string()) } else { icon_source(exe) };
        let icon = source.map(|p| exe_icon(&p, size)).unwrap_or(null_mut());
        if !icon.is_null() {
            return (icon, true);
        }
    }
    match sample {
        Some(h) => (window_icon(h), false),
        None => (LoadIconW(null_mut(), IDI_APPLICATION), false),
    }
}

/// Fensterliste neu einlesen und nach App gruppieren. Angeheftete Apps zuerst
/// (in Anheft-Reihenfolge), danach die übrigen in stabiler Reihenfolge.
/// Icons werden nur für neue Gruppen, nach DPI-Wechsel oder für `refetch`
/// (Fenster hat sein Icon geändert) neu erzeugt.
unsafe fn refresh(hwnd: HWND, refetch: Option<HWND>) {
    prof(2);
    let mut found: Vec<HWND> = Vec::new();
    EnumWindows(Some(enum_cb), &mut found as *mut _ as LPARAM);
    found.reverse();
    let Some((mut old, size, cached_size, pins, mut info, user_order)) = with(|s| {
        let size = px(s.dpi, ICON);
        let cached = s.icon_px;
        s.icon_px = size;
        // Kopie statt take: Icon-Abfragen können Nachrichten pumpen, ein Paint
        // dazwischen soll weiter die alte Liste zeichnen statt einer leeren.
        (s.groups.clone(), size, cached, s.pins.clone(), s.info.clone(), s.order.clone())
    }) else {
        return;
    };
    // Fingerabdruck vorher: Nur bei echter Änderung neu zeichnen/animieren.
    let before: Vec<(String, Vec<HWND>, usize)> = old.iter().map(|g| (g.key.clone(), g.windows.clone(), g.icon as usize)).collect();

    // Fenster in stabiler Reihenfolge: bekannte wie bisher, neue hinten.
    let mut order: Vec<HWND> = old.iter().flat_map(|g| g.windows.iter().copied()).filter(|h| found.contains(h)).collect();
    for h in &found {
        if !order.contains(h) {
            order.push(*h);
        }
    }
    info.retain(|h, _| found.contains(&(*h as HWND)));
    for &h in &order {
        info.entry(h as usize).or_insert_with(|| {
            let exe = exe_of(h);
            let key = window_key(h, exe.as_deref());
            (exe, key)
        });
    }

    // Nach Schlüssel gruppieren, Gruppen in bisheriger Reihenfolge halten.
    let mut grouped: Vec<(String, Option<String>, Vec<HWND>)> = Vec::new();
    for &h in &order {
        let (exe, key) = info[&(h as usize)].clone();
        match grouped.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.2.push(h),
            None => grouped.push((key, exe, vec![h])),
        }
    }
    grouped.sort_by_key(|g| old.iter().position(|o| o.key == g.0).unwrap_or(usize::MAX));

    // (Platz, Schlüssel, exe, Fenster): angeheftete zuerst, dann der Rest.
    let mut ordered = Vec::with_capacity(grouped.len() + pins.len());
    for pin in &pins {
        match grouped.iter().position(|g| &g.0 == pin || g.1.as_deref().map(exe_key).as_ref() == Some(pin)) {
            Some(i) => {
                let (key, exe, windows) = grouped.remove(i);
                ordered.push((pin.clone(), key, exe, windows));
            }
            None => ordered.push((pin.clone(), pin.clone(), pin.strip_prefix("exe:").map(String::from), Vec::new())),
        }
    }
    ordered.extend(grouped.into_iter().map(|(key, exe, windows)| (key.clone(), key, exe, windows)));
    // Selbst sortierte Reihenfolge hat Vorrang; Neues kommt ans Ende.
    if !user_order.is_empty() {
        ordered.sort_by_key(|g| user_order.iter().position(|s| *s == g.0).unwrap_or(usize::MAX));
    }

    let mut groups = Vec::with_capacity(ordered.len());
    for (slot, key, exe, windows) in ordered {
        let reuse = cached_size == size && !refetch.is_some_and(|r| windows.contains(&r));
        let cached = if reuse { old.iter().position(|g| g.key == key) } else { None };
        let (icon, owned) = match cached {
            Some(i) => {
                let g = old.swap_remove(i);
                (g.icon, g.owned)
            }
            None => group_icon(&key, exe.as_deref(), windows.first().copied(), size),
        };
        groups.push(Group { slot, key, exe, windows, icon, owned });
    }
    // Alles, was nicht übernommen wurde, ist weg oder ersetzt.
    for g in old {
        if g.owned {
            DestroyIcon(g.icon);
        }
    }
    let changed = groups.iter().map(|g| (g.key.clone(), g.windows.clone(), g.icon as usize)).ne(before);
    with(|s| {
        s.groups = groups;
        s.info = info;
    });
    if changed {
        prof(3);
        kick(hwnd);
        InvalidateRect(hwnd, null(), 0);
    }
}

// ---------------------------------------------------------------- Aktionen

unsafe fn activate(h: HWND) {
    if IsIconic(h) != 0 {
        // Asynchron: Ein hängendes Programm darf die Leiste nicht mit einfrieren.
        ShowWindowAsync(h, SW_RESTORE);
    }
    force_foreground(h);
}

/// Fenster einer Gruppe nach Z-Order (oberstes zuerst).
unsafe fn by_z_order(windows: &[HWND]) -> Vec<HWND> {
    let mut all: Vec<HWND> = Vec::new();
    EnumWindows(Some(enum_cb), &mut all as *mut _ as LPARAM);
    all.into_iter().filter(|h| windows.contains(h)).collect()
}

/// Neue Instanz bzw. angeheftete App starten.
unsafe fn launch(g: &Group) {
    // Bekannte exe zuerst: Bei Squirrel-Apps über deren Starter, sonst direkt.
    if let Some(exe) = g.exe.as_deref().filter(|e| !e.ends_with("applicationframehost.exe")) {
        match resolve_launch(exe) {
            Some(LaunchTarget::Squirrel { update, exe_name, .. }) => {
                shell_open(&update, &format!("--processStart \"{exe_name}\""), SW_SHOWNORMAL);
                return;
            }
            Some(LaunchTarget::Exe(p)) => {
                shell_open(&p, "", SW_SHOWNORMAL);
                return;
            }
            None => {}
        }
    }
    if let Some(id) = g.key.strip_prefix("aumid:") {
        crate::apps::launch(id);
    }
}

/// Sprungliste ("Zuletzt verwendet") einer Gruppe: über ihre AppUserModelID
/// oder – bei Desktop-Apps ohne eigene – über die ID im App-Katalog.
/// Im Präsentationsmodus leer (Dateinamen sind privat).
/// Eine Sprunglisten-Kategorie ins Menü: Überschrift, Einträge (IDs ab 200 in
/// der Reihenfolge von `labels`), Trennlinien.
fn push_jump<'a>(items: &mut Vec<Item<'a>>, c: &crate::jumplist::Category, title: &'a str, labels: &'a [String], n: &mut usize) {
    items.push(Item::Label(title));
    for e in &c.entries {
        if e.title.is_empty() {
            items.push(Item::Sep);
        } else {
            items.push(Item::Cmd(200 + *n, &labels[*n]));
        }
        *n += 1;
    }
    items.push(Item::Sep);
}

/// Kennungen, unter denen Windows die Sprunglisten einer App ablegt: ihre
/// AppUserModelID oder – ohne eine – die Startmenü-Kennung bzw. der Pfad der exe.
fn app_ids_of(g: &Group) -> Vec<String> {
    if let Some(id) = g.key.strip_prefix("aumid:") {
        return vec![id.to_string()];
    }
    let exe = g.exe.clone();
    [exe.as_deref().and_then(crate::start::app_id_for_exe), exe].into_iter().flatten().collect()
}

fn jump_list_of(g: &Group) -> Vec<(String, String)> {
    if crate::settings::get().presentation {
        return Vec::new();
    }
    let id = match g.key.strip_prefix("aumid:") {
        Some(id) => Some(id.to_string()),
        None => g.exe.as_deref().and_then(crate::start::app_id_for_exe),
    };
    id.map(|id| crate::apps::jump_list(&id, 8)).unwrap_or_default()
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max - 1).collect::<String>() + "…"
    }
}

/// Anheft-Eintrag, der zu dieser Gruppe gehört (Schlüssel oder exe).
fn pin_of(g: &Group) -> Option<String> {
    let exe = g.exe.as_deref().map(exe_key);
    with(|s| s.pins.iter().find(|p| **p == g.key || exe.as_ref() == Some(*p)).cloned()).flatten()
}

/// Anheften bevorzugt über die exe (stabil, auch ohne laufendes Fenster
/// startbar); nur Store-Apps ohne eigene exe über ihre App-Kennung.
fn pin_key_for(g: &Group) -> String {
    match g.exe.as_deref().filter(|e| !e.ends_with("applicationframehost.exe")) {
        Some(exe) => exe_key(exe),
        None => g.key.clone(),
    }
}

/// Klick auf einen Gruppen-Button wie bei Windows: starten, aktivieren,
/// minimieren – oder bei mehreren Fenstern durch die Fenster blättern.
unsafe fn click_group(g: &Group) {
    match g.windows.len() {
        0 => launch(g),
        1 => {
            let h = g.windows[0];
            if GetForegroundWindow() == h && IsIconic(h) == 0 {
                ShowWindowAsync(h, SW_MINIMIZE);
            } else {
                activate(h);
            }
        }
        _ => {
            let z = by_z_order(&g.windows);
            let fg = GetForegroundWindow();
            // Ist schon eins vorne, das unterste holen – so rotiert man durch alle.
            let next = if z.contains(&fg) { z.last() } else { z.first() };
            if let Some(&h) = next {
                activate(h);
            }
        }
    }
}

fn app_name(exe: &str) -> String {
    let stem = Path::new(exe).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut c = stem.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

unsafe fn window_title(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Anzeigename einer Gruppe: App-Name aus der exe, bei Store-Apps der Fenstertitel.
unsafe fn group_name(g: &Group) -> String {
    if let Some(exe) = g.exe.as_deref().filter(|e| !e.ends_with("applicationframehost.exe")) {
        return app_name(exe);
    }
    if let Some(&h) = g.windows.first() {
        return window_title(h);
    }
    g.key.split_once(':').map(|(_, v)| v.to_string()).unwrap_or_default()
}

unsafe fn task_menu(hwnd: HWND, g: Group) {
    let name = group_name(&g);
    let title = match g.windows.as_slice() {
        [h] => window_title(*h),
        _ => String::new(),
    };
    let pin = pin_of(&g);
    let pinned = pin.is_some();
    let recent = jump_list_of(&g);
    let recent_labels: Vec<String> = recent.iter().map(|(n, _)| shorten(n, 48)).collect();
    // App-eigene Einträge wie bei Windows: eigene Kategorien oben, Aufgaben
    // ("Neues Fenster" …) unten. Im Präsentationsmodus nur die Aufgaben –
    // eigene Kategorien enthalten oft Persönliches ("Zuletzt geschlossen").
    let presenting = crate::settings::get().presentation;
    let custom: Vec<crate::jumplist::Category> = crate::jumplist::load(&app_ids_of(&g)).into_iter().filter(|c| !presenting || c.name.is_empty()).collect();
    let entries: Vec<&crate::jumplist::Entry> = custom.iter().flat_map(|c| c.entries.iter()).collect();
    let entry_labels: Vec<String> = entries.iter().map(|e| shorten(&e.title, 48)).collect();
    let mut items = Vec::new();
    let mut n = 0;
    for c in custom.iter().filter(|c| !c.name.is_empty()) {
        push_jump(&mut items, c, &c.name, &entry_labels, &mut n);
    }
    if !recent.is_empty() {
        items.push(Item::Label("Zuletzt verwendet"));
        for (i, label) in recent_labels.iter().enumerate() {
            items.push(Item::Cmd(100 + i, label));
        }
        items.push(Item::Sep);
    }
    for c in custom.iter().filter(|c| c.name.is_empty()) {
        push_jump(&mut items, c, "Aufgaben", &entry_labels, &mut n);
    }
    if !title.is_empty() && title != name {
        items.push(Item::Label(&title));
        items.push(Item::Sep);
    }
    items.push(Item::Cmd(1, &name));
    if !g.key.starts_with("hwnd:") {
        items.push(Item::Cmd(2, if pinned { "Von Taskleiste lösen" } else { "An Taskleiste anheften" }));
    }
    match g.windows.len() {
        0 => {}
        1 => {
            items.push(Item::Sep);
            items.push(Item::Cmd(3, "Fenster schließen"));
        }
        _ => {
            items.push(Item::Sep);
            items.push(Item::Cmd(3, "Alle Fenster schließen"));
        }
    }
    match menus::popup(hwnd, &items) {
        1 => launch(&g),
        2 => toggle_key(&pin.unwrap_or_else(|| pin_key_for(&g))),
        id @ 100..=199 => {
            if let Some((_, path)) = recent.get(id - 100) {
                shell_open(path, "", SW_SHOWNORMAL);
            }
        }
        id @ 200..=999 => {
            if let Some(e) = entries.get(id - 200) {
                crate::jumplist::launch(e);
            }
        }
        3 => {
            for &w in &g.windows {
                PostMessageW(w, WM_CLOSE, 0, 0);
            }
        }
        _ => {}
    }
}

/// "…"-Knopf: die Apps, für die in der Leiste kein Platz ist, als Liste.
unsafe fn more_menu(hwnd: HWND) {
    let Some((l, _)) = current_layout(hwnd) else { return };
    let hidden: Vec<Group> = with(|s| s.groups.iter().skip(l.tasks.len()).cloned().collect()).unwrap_or_default();
    let names: Vec<String> = hidden.iter().map(|g| shorten(&group_name(g), 60)).collect();
    let items: Vec<Item> = names.iter().enumerate().map(|(n, name)| Item::Cmd(n + 1, name)).collect();
    let id = menus::popup(hwnd, &items);
    if let Some(g) = id.checked_sub(1).and_then(|n| hidden.get(n)) {
        click_group(g);
    }
}

/// Menü-IDs der Bildschirm-Auswahl (eine pro Bildschirm).
const MONITOR_IDS: usize = 50;

unsafe fn bar_menu(hwnd: HWND) {
    use crate::settings::{GlowStyle, StartLogo};
    let s = crate::settings::get();
    let glow = |id, label, style| Item::Check(id, label, s.glow == style);
    // Bildschirme: Hauptbildschirm hat immer eine Leiste; die übrigen einzeln
    // wählbar, dazu die Regel für weitere (anfangs wie in Windows eingestellt).
    let monitors = crate::display::list();
    let mut screens: Vec<Item> = monitors
        .iter()
        .enumerate()
        .map(|(n, m)| if m.primary { Item::Label(&m.label) } else { Item::Check(MONITOR_IDS + n, &m.label, crate::display::wanted(m)) })
        .collect();
    screens.push(Item::Sep);
    screens.push(Item::Check(19, "Auf weiteren Bildschirmen anzeigen", crate::display::new_monitors()));
    let secondary = hwnd != self::hwnd();
    let last = if secondary { Item::Cmd(20, "Auf diesem Bildschirm ausblenden") } else { Item::Cmd(3, "Lightbar beenden") };
    let id = menus::popup(
        hwnd,
        &[
            Item::Cmd(1, "Task-Manager"),
            Item::Cmd(2, "Taskleisteneinstellungen"),
            Item::Sep,
            Item::Sub(
                "Leuchtrand",
                vec![
                    glow(10, "Aurora", GlowStyle::Aurora),
                    glow(11, "Akzentfarbe", GlowStyle::Accent),
                    glow(12, "Neon", GlowStyle::Neon),
                    Item::Sep,
                    glow(13, "Aus", GlowStyle::Off),
                ],
            ),
            Item::Sub(
                "Startsymbol",
                vec![
                    Item::Check(16, "Orb", s.start_logo == StartLogo::Orb),
                    Item::Check(17, "Windows-Logo", s.start_logo == StartLogo::Windows),
                    Item::Check(21, "Tux", s.start_logo == StartLogo::Tux),
                ],
            ),
            Item::Sub("Startmenü", vec![Item::Check(22, "Profilbild anzeigen", s.start_avatar)]),
            Item::Check(14, "Uhr mit Sekunden", s.clock_seconds),
            Item::Check(15, "Claude/Codex-Usage anzeigen", s.show_usage),
            Item::Check(5, "Tray-Icons einfarbig", s.mono_tray),
            Item::Check(18, "Präsentationsmodus", s.presentation),
            Item::Sub("Bildschirme", screens),
            Item::Sep,
            last,
        ],
    );
    match id {
        1 => shell_open("taskmgr.exe", "", SW_SHOWNORMAL),
        // Auch die Auswahl der sichtbaren Tray-Icons: wir lesen dieselbe Einstellung.
        2 => shell_open("ms-settings:taskbar", "", SW_SHOWNORMAL),
        10 => crate::glow::set_style(GlowStyle::Aurora),
        11 => crate::glow::set_style(GlowStyle::Accent),
        12 => crate::glow::set_style(GlowStyle::Neon),
        13 => crate::glow::set_style(GlowStyle::Off),
        14 => {
            crate::settings::update(|s| s.clock_seconds = !s.clock_seconds);
            // Der Uhr-Takt läuft an der Hauptleiste, auch wenn das Menü auf einer Zweitleiste kam.
            schedule_clock(self::hwnd());
        }
        16 => crate::settings::update(|s| s.start_logo = StartLogo::Orb),
        17 => crate::settings::update(|s| s.start_logo = StartLogo::Windows),
        21 => crate::settings::update(|s| s.start_logo = StartLogo::Tux),
        22 => crate::settings::update(|s| s.start_avatar = !s.start_avatar),
        15 => crate::settings::update(|s| {
            s.show_usage = !s.show_usage;
            // Beide Dienste abgewählt hieße: Anzeige bleibt trotzdem unsichtbar.
            if s.show_usage && !s.usage_claude && !s.usage_codex {
                s.usage_claude = true;
                s.usage_codex = true;
            }
        }),
        5 => crate::settings::update(|s| s.mono_tray = !s.mono_tray),
        18 => crate::settings::update(|s| s.presentation = !s.presentation),
        3 => {
            crate::util::log("Beenden über das Menü");
            DestroyWindow(hwnd);
            return;
        }
        19 => {
            crate::display::set_new_monitors(!crate::display::new_monitors());
            sync_monitors();
        }
        20 => {
            let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            if let Some(m) = monitors.iter().find(|m| m.handle == mon) {
                crate::display::set_wanted(&m.id, false);
            }
            // Zerstört diese Leiste – danach nichts mehr mit `hwnd` anfangen.
            sync_monitors();
            return;
        }
        n if (MONITOR_IDS..MONITOR_IDS + monitors.len()).contains(&n) => {
            let m = &monitors[n - MONITOR_IDS];
            crate::display::set_wanted(&m.id, !crate::display::wanted(m));
            let destroys_self = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) == m.handle && secondary;
            sync_monitors();
            if destroys_self {
                return;
            }
        }
        _ => {}
    }
    InvalidateRect(hwnd, null(), 0);
}
/// Text für den Tooltip über einem Element.
unsafe fn tip_text(hit: Hit) -> String {
    match hit {
        Hit::Start => "Start".into(),
        Hit::Task(i) => match with(|s| s.groups.get(i).cloned()).flatten() {
            Some(g) => group_name(&g),
            None => String::new(),
        },
        Hit::Tray(i) => tray::tip(i),
        Hit::Chevron => "Ausgeblendete Symbole anzeigen".into(),
        Hit::More => "Weitere Apps".into(),
        Hit::Status => {
            let Some(s) = with(|s| s.status.clone()).flatten() else { return String::new() };
            let net = match &s.net {
                status::Net::Wifi(_, ssid) => format!("WLAN: {ssid}"),
                status::Net::Wired => "Netzwerk: Ethernet, Internetzugriff".into(),
                status::Net::Offline => "Nicht verbunden".into(),
            };
            let vol = if s.muted { "Lautsprecher: stumm".into() } else { format!("Lautsprecher: {} %", (s.volume * 100.0).round()) };
            let mut lines = vec![net, vol];
            if let Some((p, charging)) = s.battery {
                lines.push(format!("Akku: {p} %{}", if charging { ", wird geladen" } else { "" }));
            }
            lines.join("\n")
        }
        Hit::Clock => {
            let mut st: SYSTEMTIME = zeroed();
            GetLocalTime(&mut st);
            const DAYS: [&str; 7] = ["Sonntag", "Montag", "Dienstag", "Mittwoch", "Donnerstag", "Freitag", "Samstag"];
            const MONTHS: [&str; 12] = ["Januar", "Februar", "März", "April", "Mai", "Juni", "Juli", "August", "September", "Oktober", "November", "Dezember"];
            format!("{}, {}. {} {}", DAYS[st.wDayOfWeek as usize % 7], st.wDay, MONTHS[(st.wMonth as usize + 11) % 12], st.wYear)
        }
        Hit::Desktop => "Desktop anzeigen".into(),
        Hit::TaskMgr => {
            let (cpu, ram) = with(|s| (s.cpu, s.ram)).unwrap_or((0.0, 0.0));
            format!("Task-Manager\nCPU {:.0} %  ·  Arbeitsspeicher {:.0} %", cpu, ram)
        }
        Hit::Usage => {
            let u = crate::usage::current().unwrap_or_default();
            let s = crate::settings::get();
            let mut parts = Vec::new();
            if s.usage_claude {
                parts.push(crate::usage::describe("Claude", &u.claude, u.claude_at));
            }
            if s.usage_codex {
                parts.push(crate::usage::describe("Codex", &u.codex, u.codex_at));
            }
            parts.push("Klicken zum Aktualisieren, Rechtsklick für Anmelden und Auswahl".into());
            parts.join("

")
        }
        Hit::None => String::new(),
    }
}

unsafe fn rect_of(hwnd: HWND, hit: Hit) -> Option<RECT> {
    let (l, tray) = current_layout(hwnd)?;
    match hit {
        Hit::Start => Some(l.start),
        Hit::Task(i) => l.tasks.get(i).copied(),
        Hit::Tray(i) => tray.iter().position(|t| t.0 == i).map(|p| l.tray[p]),
        Hit::Chevron => l.chevron,
        Hit::More => l.more,
        Hit::Status => Some(l.status),
        Hit::Clock => Some(l.clock),
        Hit::Desktop => Some(l.desktop),
        Hit::Usage => Some(l.usage),
        Hit::TaskMgr => Some(l.taskmgr),
        Hit::None => None,
    }
}

/// Offene Fenster hinter einem Button (nicht bei angehefteten ohne Fenster).
fn window_at(hit: Hit) -> Option<(Vec<HWND>, HICON)> {
    let Hit::Task(i) = hit else { return None };
    with(|s| s.groups.get(i).map(|g| (g.windows.clone(), g.icon))).flatten().filter(|g| !g.0.is_empty())
}

/// Anker für Popups zu einem Element der Leiste: das Element, quer zur Leiste
/// auf deren ganze Dicke gestreckt (Tooltips halten so Abstand zur Leiste, nicht
/// zum eingerückten Button), in Bildschirmkoordinaten.
unsafe fn element_anchor(hwnd: HWND, r: RECT) -> RECT {
    let mut c: RECT = zeroed();
    GetClientRect(hwnd, &mut c);
    let r = if crate::display::edge().vertical() { RECT { left: c.left, right: c.right, ..r } } else { RECT { top: c.top, bottom: c.bottom, ..r } };
    rect_to_screen(hwnd, r)
}

/// Nach kurzer Verweildauer: Fenster-Vorschau für offene Apps, sonst Tooltip.
unsafe fn show_tip(hwnd: HWND) {
    let Some(hover) = with(|s| s.hover) else { return };
    let dpi = bar_dpi(hwnd);
    let Some(r) = rect_of(hwnd, hover) else { return };
    let anchor = element_anchor(hwnd, r);
    if let Some((targets, icon)) = window_at(hover) {
        crate::preview::show(&targets, icon, anchor, dpi);
    } else {
        crate::tooltip::show(&tip_text(hover), anchor, dpi);
    }
}

/// Mausrad über Netz/Lautstärke: Lautstärke in 2-%-Schritten, mit Anzeige.
unsafe fn wheel_volume(hwnd: HWND, delta: i32) {
    let current = status::poll();
    let v = (current.volume + delta as f32 / 120.0 * 0.02).clamp(0.0, 1.0);
    status::set_volume(v);
    let s = status::poll();
    let label = format!("Lautstärke: {} %", (s.volume * 100.0).round());
    with(|st| st.status = Some(s));
    InvalidateRect(hwnd, null(), 0);
    if let Some(r) = rect_of(hwnd, Hit::Status) {
        crate::tooltip::show(&label, element_anchor(hwnd, r), bar_dpi(hwnd));
    }
}

/// Mausrad über einer App mit mehreren Fenstern: durchblättern.
unsafe fn cycle_group(g: &Group, forward: bool) {
    if g.windows.len() < 2 {
        return;
    }
    let z = by_z_order(&g.windows);
    let fg = GetForegroundWindow();
    let next = match z.iter().position(|&w| w == fg) {
        // Vorwärts: unterstes holen (rotiert), rückwärts: zweitoberstes.
        Some(_) if forward => z.last(),
        Some(_) => z.get(1),
        None => z.first(),
    };
    if let Some(&h) = next {
        activate(h);
    }
}

// ------------------------------------------------------------- Profil
//
// Mit LIGHTBAR_PROFILE=1 zählt die Leiste, was sie aufweckt, und schreibt die
// Zahlen alle 30 s nach %APPDATA%\Lightbar\profile.txt.

pub static PROF: [std::sync::atomic::AtomicU32; 8] = [const { std::sync::atomic::AtomicU32::new(0) }; 8];
const PROF_NAMES: [&str; 8] = ["shell-hook", "winevent", "refresh", "refresh-geändert", "paint", "timer", "status", "copydata"];

pub fn prof(i: usize) {
    PROF[i].fetch_add(1, Ordering::Relaxed);
}

thread_local! {
    static PROF_DETAIL: RefCell<HashMap<String, u32>> = RefCell::new(HashMap::new());
}

fn dump_profile() {
    if std::env::var("LIGHTBAR_PROFILE").is_err() {
        return;
    }
    let mut detail: Vec<(String, u32)> = PROF_DETAIL.with(|d| d.borrow_mut().drain().collect());
    detail.sort_by(|a, b| b.1.cmp(&a.1));
    let top: Vec<String> = detail.iter().take(6).map(|(k, v)| format!("{k}={v}")).collect();
    let line: Vec<String> = PROF.iter().zip(PROF_NAMES).map(|(c, n)| format!("{n}={}", c.swap(0, Ordering::Relaxed))).collect();
    if let Some(dir) = crate::util::data_dir() {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("profile.txt")) {
            let _ = writeln!(f, "{}", line.join(" "));
            let _ = writeln!(f, "  Code:Klasse:exe → {}", top.join(", "));
        }
    }
}

/// Uhr-Zeitgeber auf den nächsten Sekunden- bzw. Minutenwechsel stellen.
unsafe fn schedule_clock(hwnd: HWND) {
    let mut st: SYSTEMTIME = zeroed();
    GetLocalTime(&mut st);
    let ms = st.wMilliseconds as u32;
    let due = if crate::settings::get().clock_seconds { 1000 - ms } else { (60 - st.wSecond as u32) * 1000 - ms };
    SetTimer(hwnd, TIMER_TICK, due.max(20), None);
}

/// Status neu lesen; nur bei Änderung die Status-Symbole neu zeichnen.
unsafe fn update_status(hwnd: HWND) {
    let s = status::poll();
    let changed = with(|st| {
        let changed = st.status.as_ref() != Some(&s);
        st.status = Some(s);
        changed
    })
    .unwrap_or(false);
    if changed {
        if let Some((l, _)) = current_layout(hwnd) {
            InvalidateRect(hwnd, &l.status, 0);
        }
    }
}

/// WinEvents (außerhalb unseres Prozesses, nur Fenster-Objekte) an die Leiste weiterreichen.
unsafe extern "system" fn win_event(_: windows_sys::Win32::UI::Accessibility::HWINEVENTHOOK, event: u32, h: HWND, object: i32, child: i32, _: u32, _: u32) {
    if h.is_null() || object != OBJID_WINDOW || child != 0 {
        return;
    }
    // Ein-/Ausblenden passiert ständig (Menüs, Tooltips, Konsolen): nur
    // weiterreichen, wenn es ein Leisten-Fenster ist oder wird.
    if event == EVENT_OBJECT_SHOW && !is_task_window(h) {
        return;
    }
    if event == EVENT_OBJECT_HIDE && !is_known(h) {
        return;
    }
    PostMessageW(self::hwnd(), WM_BAR_EVENT, event as usize, 0);
}

/// Steht das Fenster gerade als Button auf der Leiste?
fn is_known(h: HWND) -> bool {
    with(|s| s.groups.iter().any(|g| g.windows.contains(&h))).unwrap_or(false)
}

/// Vordergrundwechsel, Cloaking (virtuelle Desktops) und Ein-/Ausblenden abonnieren.
unsafe fn install_win_events() {
    use windows_sys::Win32::UI::Accessibility::SetWinEventHook;
    let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
    SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, null_mut(), Some(win_event), 0, 0, flags);
    SetWinEventHook(EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED, null_mut(), Some(win_event), 0, 0, flags);
    SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_HIDE, null_mut(), Some(win_event), 0, 0, flags);
}

/// Win+1…9: n-ten Button aktivieren oder starten.
unsafe fn activate_nth(n: usize) {
    if let Some(g) = with(|s| s.groups.get(n).cloned()).flatten() {
        click_group(&g);
    }
}

/// Usage neu abfragen und die Leiste danach neu zeichnen.
unsafe fn refresh_usage(hwnd: HWND) {
    let h = hwnd as isize;
    crate::usage::refresh_now(move || {
        PostMessageW(h as HWND, WM_BAR_USAGE, 0, 0);
    });
}

/// Befehl in einem sichtbaren Terminal ausführen (Windows Terminal, sonst
/// Eingabeaufforderung); das Fenster bleibt danach offen.
unsafe fn run_in_terminal(cmd: &str) {
    if !shell_exec("open", "wt.exe", &format!("cmd /k {cmd}")) {
        shell_exec("open", "cmd.exe", &format!("/k {cmd}"));
    }
}

/// Rechtsklick auf die Usage-Anzeige: neu anmelden, wenn ein Login
/// abgelaufen ist ("nicht verfügbar"), oder sofort aktualisieren.
unsafe fn usage_menu(hwnd: HWND) {
    let s = crate::settings::get();
    let id = menus::popup(
        hwnd,
        &[
            Item::Cmd(1, "Jetzt aktualisieren"),
            Item::Sep,
            Item::Cmd(2, "Bei Claude anmelden …"),
            Item::Cmd(3, "Bei Codex anmelden …"),
            Item::Sep,
            Item::Check(5, "Claude anzeigen", s.usage_claude),
            Item::Check(6, "Codex anzeigen", s.usage_codex),
            Item::Sep,
            Item::Cmd(4, "Usage-Anzeige ausblenden"),
        ],
    );
    match id {
        1 => refresh_usage(hwnd),
        2 => run_in_terminal("claude auth login"),
        3 => run_in_terminal("codex login"),
        4 => {
            crate::settings::update(|s| s.show_usage = false);
            InvalidateRect(hwnd, null(), 0);
        }
        5 | 6 => {
            crate::settings::update(|s| if id == 5 { s.usage_claude = !s.usage_claude } else { s.usage_codex = !s.usage_codex });
            // Frisch eingeschaltet: gleich abfragen.
            refresh_usage(hwnd);
        }
        _ => {}
    }
}

unsafe fn status_menu(hwnd: HWND) {
    let id = menus::popup(
        hwnd,
        &[
            Item::Cmd(1, "Netzwerk- und Interneteinstellungen"),
            Item::Cmd(2, "Soundeinstellungen"),
            Item::Cmd(3, "Lautstärkemixer"),
            Item::Cmd(4, "Energieeinstellungen"),
        ],
    );
    let uri = match id {
        1 => "ms-settings:network-status",
        2 => "ms-settings:sound",
        3 => "ms-settings:apps-volume",
        4 => "ms-settings:powersleep",
        _ => return,
    };
    shell_open(uri, "", SW_SHOWNORMAL);
}

unsafe fn clock_menu(hwnd: HWND) {
    let id = menus::popup(
        hwnd,
        &[Item::Cmd(1, "Datum und Uhrzeit anpassen"), Item::Cmd(2, "Benachrichtigungseinstellungen")],
    );
    match id {
        1 => shell_open("ms-settings:dateandtime", "", SW_SHOWNORMAL),
        2 => shell_open("ms-settings:notifications", "", SW_SHOWNORMAL),
        _ => {}
    }
}

unsafe fn tray_click(hwnd: HWND, index: usize, kind: Click) {
    // Anker = Mitte des Icons, so wie Explorer es meldet.
    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    if let Some((l, tray)) = current_layout(hwnd) {
        if let Some(pos) = tray.iter().position(|t| t.0 == index) {
            let r = l.tray[pos];
            pt = POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
            ClientToScreen(hwnd, &mut pt);
        }
    }
    tray::click(index, kind, pt);
}

// ----------------------------------------------------------------- AppBar

unsafe fn appbar_data(hwnd: HWND) -> APPBARDATA {
    APPBARDATA {
        cbSize: size_of::<APPBARDATA>() as u32,
        hWnd: hwnd,
        uCallbackMessage: WM_APPBAR,
        uEdge: crate::display::edge().abe(),
        rc: zeroed(),
        lParam: 0,
    }
}

/// Platz am unteren Rand reservieren und Fenster dorthin legen.
unsafe fn set_pos(hwnd: HWND) {
    if hwnd != self::hwnd() {
        set_pos_secondary(hwnd);
        return;
    }
    let dpi = GetDpiForWindow(hwnd);
    let (font, icons) = (make_font(dpi, 12, FW_NORMAL), icon_font(dpi, 16));
    with(|s| {
        s.dpi = dpi;
        for f in [s.font, s.icons] {
            if !f.is_null() {
                DeleteObject(f);
            }
        }
        s.font = font;
        s.icons = icons;
    });
    dock(hwnd, dpi);
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    let icon = exe_icon(&format!("{root}\\System32\\Taskmgr.exe"), px(dpi, 20));
    with(|s| {
        if !s.taskmgr_icon.is_null() {
            DestroyIcon(s.taskmgr_icon);
        }
        s.taskmgr_icon = icon;
    });
    crate::glow::sync();
}

// ---------------------------------------------------------------- Zeichnen

// ------------------------------------------------- Barrierefreiheit (UIA)

/// Ein bedienbares Element der Leiste, wie es Screenreader sehen.
pub struct UiElement {
    pub id: String,
    pub name: String,
    pub help: String,
    /// Bildschirmkoordinaten.
    pub rect: RECT,
    hit: Hit,
}

/// Alle Elemente von links nach rechts – dieselbe Quelle wie Layout und Hit-Test.
pub fn ui_elements() -> Vec<UiElement> {
    unsafe {
        let hwnd = hwnd();
        let Some((l, tray)) = current_layout(hwnd) else { return Vec::new() };
        let mut origin = POINT { x: 0, y: 0 };
        ClientToScreen(hwnd, &mut origin);
        let screen = |r: RECT| RECT { left: r.left + origin.x, top: r.top + origin.y, right: r.right + origin.x, bottom: r.bottom + origin.y };
        let groups = with(|s| s.groups.clone()).unwrap_or_default();
        let fg = GetForegroundWindow();
        let mut out = Vec::new();
        let mut push = |hit: Hit, id: String, name: String, help: &str, rect: RECT| {
            out.push(UiElement { id, name, help: help.to_string(), rect: screen(rect), hit });
        };
        if l.show_usage {
            push(Hit::Usage, "usage".into(), tip_text(Hit::Usage).replace('\n', ", "), "Claude- und Codex-Nutzung, klicken zum Aktualisieren", l.usage);
        }
        push(Hit::TaskMgr, "taskmgr".into(), tip_text(Hit::TaskMgr).replace('\n', ", "), "Task-Manager öffnen", l.taskmgr);
        push(Hit::Start, "start".into(), "Start".into(), "Startmenü öffnen", l.start);
        for (i, g) in groups.iter().enumerate() {
            let Some(&r) = l.tasks.get(i) else { continue };
            let state = match g.windows.len() {
                0 => "angeheftet, nicht geöffnet".to_string(),
                1 if g.windows[0] == fg => "aktiv".to_string(),
                1 => window_title(g.windows[0]),
                n => format!("{n} Fenster"),
            };
            push(Hit::Task(i), format!("app:{}", g.slot), group_name(g), &state, r);
        }
        if let Some(r) = l.more {
            push(Hit::More, "more".into(), "Weitere Apps".into(), "Apps, die in der Leiste keinen Platz haben", r);
        }
        if let Some(r) = l.chevron {
            push(Hit::Chevron, "overflow".into(), "Ausgeblendete Symbole anzeigen".into(), "Infobereich", r);
        }
        for (n, &(index, _)) in tray.iter().enumerate() {
            let name = tray::tip(index);
            let name = if name.is_empty() { "Infobereich-Symbol".to_string() } else { name };
            push(Hit::Tray(index), format!("tray:{index}"), name, "Infobereich", l.tray[n]);
        }
        push(Hit::Status, "status".into(), tip_text(Hit::Status).replace('\n', ", "), "Schnelleinstellungen öffnen", l.status);
        push(Hit::Clock, "clock".into(), tip_text(Hit::Clock), "Kalender öffnen", l.clock);
        push(Hit::Desktop, "desktop".into(), "Desktop anzeigen".into(), "Alle Fenster minimieren", l.desktop);
        out
    }
}

pub fn ui_focus() -> Option<usize> {
    with(|s| s.kb_focus).flatten()
}

/// Tastaturfokus auf ein Element setzen (Win+T, Pfeiltasten, Screenreader).
pub fn ui_set_focus(index: usize) {
    unsafe {
        let h = hwnd();
        let n = ui_elements().len();
        if n == 0 {
            return;
        }
        let index = index.min(n - 1);
        with(|s| s.kb_focus = Some(index));
        if GetForegroundWindow() != h {
            force_foreground(h);
        }
        SetFocus(h);
        InvalidateRect(h, null(), 0);
        crate::uia::raise_focus(h as isize, index);
    }
}

/// Element auslösen (Enter/Leertaste oder Screenreader) – wie ein Klick.
pub fn ui_invoke(index: usize) {
    unsafe {
        PostMessageW(hwnd(), WM_BAR_INVOKE, index, 0);
    }
}

/// Klick (links oder rechts) mittig auf ein Element schicken.
unsafe fn click_element(hwnd: HWND, index: usize, msg: u32) {
    let Some(e) = ui_elements().into_iter().nth(index) else { return };
    let mut pt = POINT { x: (e.rect.left + e.rect.right) / 2, y: (e.rect.top + e.rect.bottom) / 2 };
    ScreenToClient(hwnd, &mut pt);
    let lp = (pt.x as u16 as isize) | ((pt.y as u16 as isize) << 16);
    let _ = e.hit;
    wndproc(hwnd, msg, 0, lp);
}

/// Tastatur im Fokusmodus; `true`, wenn die Taste verbraucht wurde.
unsafe fn focus_key(hwnd: HWND, vk: u16) -> bool {
    let Some(cur) = ui_focus() else { return false };
    let n = ui_elements().len();
    let shift = GetKeyState(VK_SHIFT as i32) < 0;
    match vk {
        // Senkrechte Leiste: Hoch/Runter wie Links/Rechts.
        VK_RIGHT | VK_DOWN | VK_TAB if !shift => ui_set_focus((cur + 1) % n.max(1)),
        VK_LEFT | VK_UP | VK_TAB => ui_set_focus((cur + n.max(1) - 1) % n.max(1)),
        VK_HOME => ui_set_focus(0),
        VK_END => ui_set_focus(n.saturating_sub(1)),
        VK_RETURN | VK_SPACE => click_element(hwnd, cur, WM_LBUTTONUP),
        VK_APPS | VK_F10 => click_element(hwnd, cur, WM_RBUTTONUP),
        VK_ESCAPE => {
            with(|s| s.kb_focus = None);
            InvalidateRect(hwnd, null(), 0);
        }
        _ => return false,
    }
    true
}

/// Fokusrahmen in Client-Koordinaten (für das Zeichnen).
unsafe fn focus_rect(hwnd: HWND) -> Option<RECT> {
    let i = ui_focus()?;
    let e = ui_elements().into_iter().nth(i)?;
    let mut tl = POINT { x: e.rect.left, y: e.rect.top };
    let mut br = POINT { x: e.rect.right, y: e.rect.bottom };
    ScreenToClient(hwnd, &mut tl);
    ScreenToClient(hwnd, &mut br);
    Some(RECT { left: tl.x, top: tl.y, right: br.x, bottom: br.y })
}

// ------------------------------------------------------------- Animation

/// Animierte Werte der Leiste. Ziele kommen aus Layout und Zustand,
/// `step_anim` nähert die aktuellen Werte jedes Bild weich an.
#[derive(Default, Clone)]
struct Anim {
    /// Hover-Stärke 0..1 je Element.
    hover: HashMap<String, f32>,
    /// Breite des Laufend-Strichs (Pixel) je Gruppe.
    ind: HashMap<String, f32>,
    /// Einblenden neuer Buttons 0..1.
    appear: HashMap<String, f32>,
    /// Aktuelle linke Kante (Pixel) je Gruppe – Buttons rutschen weich nach.
    x: HashMap<String, f32>,
    /// Zuletzt gedrückter Button (Feder-Effekt).
    press: Option<(String, Instant)>,
}

const PRESS_S: f32 = 0.28;

fn hit_id(hit: Hit, groups: &[Group]) -> Option<String> {
    Some(match hit {
        Hit::Start => "start".into(),
        Hit::Task(i) => format!("g:{}", groups.get(i)?.key),
        Hit::Tray(i) => format!("t:{i}"),
        Hit::Chevron => "chevron".into(),
        Hit::More => "more".into(),
        Hit::Status => "status".into(),
        Hit::Clock => "clock".into(),
        Hit::Usage => "usage".into(),
        Hit::Desktop => "desktop".into(),
        Hit::TaskMgr => "taskmgr".into(),
        Hit::None => return None,
    })
}

fn approach(v: &mut f32, target: f32, speed: f32, eps: f32, moving: &mut bool) {
    let d = target - *v;
    if d.abs() <= eps {
        *v = target;
    } else {
        *v += d * speed;
        *moving = true;
    }
}

/// Animation anstoßen (läuft mit ~60 fps, bis alles ruht).
unsafe fn kick(hwnd: HWND) {
    // Den Animationszustand teilen sich alle Leisten; rechnen darf immer nur
    // eine (die zuletzt angestoßene). Zwei Leisten mit unterschiedlicher
    // Skalierung oder Breite zögen ihn sonst endlos in verschiedene Richtungen.
    let prev = ANIM_OWNER.swap(hwnd, Ordering::Relaxed);
    if !prev.is_null() && prev != hwnd {
        KillTimer(prev, TIMER_ANIM);
    }
    SetTimer(hwnd, TIMER_ANIM, 16, None);
}

/// Leiste, deren Layout die Animation gerade antreibt.
static ANIM_OWNER: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

/// Gleiten (anim.x) gilt nur für die Leiste, die es berechnet; die anderen
/// zeichnen ihre Buttons an der Zielposition.
fn animates_positions(hwnd: HWND) -> bool {
    // Senkrechte Leiste: kein seitliches Gleiten (die Buttons stehen untereinander).
    if crate::display::edge().vertical() {
        return false;
    }
    let owner = ANIM_OWNER.load(Ordering::Relaxed);
    owner == hwnd || (owner.is_null() && hwnd == self::hwnd())
}

/// Während des Ziehens: Button folgt der Maus, die anderen weichen aus.
/// `true`, wenn gerade gezogen wird (dann kein normales Hover).
unsafe fn drag_move(hwnd: HWND, x: i32) -> bool {
    // Umsortieren per Ziehen ist waagerecht gedacht; senkrecht vorerst nicht.
    if crate::display::edge().vertical() {
        // drag bleibt inaktiv stehen: drag_end gibt die Maus wieder frei.
        return false;
    }
    let Some((l, _)) = current_layout(hwnd) else { return false };
    let dpi = bar_dpi(hwnd);
    let moving = with(|s| {
        let d = dpi;
        let drag = s.drag.as_mut()?;
        drag.x = x;
        if !drag.active {
            // Mit Finger/Stift wackelt ein Tipp mehr – erst ab größerer Strecke ziehen.
            let threshold = if from_touch() { 20 } else { 6 };
            if (x - drag.start_x).abs() < px(d, threshold) {
                return Some(false);
            }
            drag.active = true;
            s.anim.press = None;
        }
        let slot = drag.slot.clone();
        let cur = s.groups.iter().position(|g| g.slot == slot)?;
        // Ziel = Anzahl der anderen Buttons, deren Mitte links vom Zeiger liegt.
        let target = l.tasks.iter().enumerate().filter(|(i, r)| *i != cur && (r.left + r.right) / 2 < x).count();
        if target != cur && target < s.groups.len() {
            let g = s.groups.remove(cur);
            s.groups.insert(target, g);
            s.order = s.groups.iter().map(|g| g.slot.clone()).collect();
        }
        Some(true)
    })
    .flatten();
    if moving != Some(true) {
        return false;
    }
    crate::tooltip::hide();
    crate::preview::hide();
    kick(hwnd);
    InvalidateRect(hwnd, null(), 0);
    true
}

/// Ziehen beenden. Angeheftete Apps behalten die neue Reihenfolge dauerhaft.
/// `true`, wenn wirklich gezogen wurde (dann ist es kein Klick).
unsafe fn drag_end(hwnd: HWND) -> bool {
    let Some(drag) = with(|s| s.drag.take()).flatten() else { return false };
    ReleaseCapture();
    if !drag.active {
        return false;
    }
    with(|s| {
        let mut pins: Vec<String> = s.groups.iter().filter(|g| s.pins.contains(&g.slot)).map(|g| g.slot.clone()).collect();
        for p in &s.pins {
            if !pins.contains(p) {
                pins.push(p.clone());
            }
        }
        s.pins = pins;
        save_pins(&s.pins);
    });
    kick(hwnd);
    InvalidateRect(hwnd, null(), 0);
    true
}

/// Ein Animationsschritt; `true`, solange sich noch etwas bewegt.
unsafe fn step_anim(hwnd: HWND) -> bool {
    let Some((l, _)) = current_layout(hwnd) else { return false };
    let dpi = bar_dpi(hwnd);
    let fg = GetForegroundWindow();
    let mut bar_origin = POINT { x: 0, y: 0 };
    ClientToScreen(hwnd, &mut bar_origin);
    let (moving, new_buttons) = with(|s| {
        let d = dpi;
        let mut moving = false;
        let hover_id = hit_id(s.hover, &s.groups);
        if let Some(id) = &hover_id {
            s.anim.hover.entry(id.clone()).or_insert(0.0);
        }
        for (id, v) in s.anim.hover.iter_mut() {
            let target = if Some(id) == hover_id.as_ref() { 1.0 } else { 0.0 };
            approach(v, target, 0.22, 0.01, &mut moving);
        }
        s.anim.hover.retain(|id, v| *v > 0.0 || Some(id) == hover_id.as_ref());

        let had_buttons = !s.anim.x.is_empty();
        let mut new_buttons = Vec::new();
        let mut live = Vec::with_capacity(s.groups.len());
        for (i, g) in s.groups.iter().enumerate() {
            let Some(r) = l.tasks.get(i) else { continue };
            let id = format!("g:{}", g.key);
            let ind = if g.windows.is_empty() {
                0.0
            } else if g.windows.contains(&fg) {
                px(d, 16) as f32
            } else {
                px(d, 6) as f32
            };
            approach(s.anim.ind.entry(id.clone()).or_insert(0.0), ind, 0.22, 0.3, &mut moving);
            approach(s.anim.appear.entry(id.clone()).or_insert(0.0), 1.0, 0.16, 0.01, &mut moving);
            if !s.anim.x.contains_key(&id) && had_buttons {
                new_buttons.push(bar_origin.x + (r.left + r.right) / 2);
            }
            // Relativ zum Startknopf: Haupt- und Zweitleiste haben verschiedene Breiten.
            let rel = (r.left - l.start.left) as f32;
            approach(s.anim.x.entry(id.clone()).or_insert(rel), rel, 0.22, 0.5, &mut moving);
            live.push(id);
        }
        s.anim.ind.retain(|k, _| live.contains(k));
        s.anim.appear.retain(|k, _| live.contains(k));
        s.anim.x.retain(|k, _| live.contains(k));
        if let Some((_, t0)) = &s.anim.press {
            if t0.elapsed().as_secs_f32() < PRESS_S {
                moving = true;
            } else {
                s.anim.press = None;
            }
        }
        // Aufmerksamkeit pulsiert dauerhaft – aber nur für Fenster mit Button
        // (z. B. nicht für ein blinkendes Fenster auf einem anderen Desktop).
        s.attention.retain(|&w| IsWindow(w) != 0);
        if s.groups.iter().any(|g| g.windows.iter().any(|w| s.attention.contains(w))) {
            moving = true;
        }
        (moving, new_buttons)
    })
    .unwrap_or((false, Vec::new()));
    // Neu geöffnete App: Lichtimpuls im Leuchtrand.
    for x in new_buttons {
        crate::glow::flash(x);
    }
    moving
}

/// Leuchtrand mit Hover-Position und Aufmerksamkeits-Buttons versorgen.
unsafe fn sync_glow(hwnd: HWND) {
    if hwnd != self::hwnd() {
        crate::glow::set_hover(None);
        return;
    }
    let Some((l, _)) = current_layout(hwnd) else { return };
    let mut origin = POINT { x: 0, y: 0 };
    ClientToScreen(hwnd, &mut origin);
    let hover_hit = with(|s| if s.hover_hwnd == hwnd { s.hover } else { Hit::None }).unwrap_or(Hit::None);
    let hover = rect_of(hwnd, hover_hit).map(|r| origin.x + (r.left + r.right) / 2);
    let Some(attention) = with(|s| {
        s.attention.retain(|&w| IsWindow(w) != 0);
        let attention = s
            .groups
            .iter()
            .enumerate()
            .filter(|(_, g)| g.windows.iter().any(|w| s.attention.contains(w)))
            .filter_map(|(i, _)| l.tasks.get(i).map(|r| origin.x + (r.left + r.right) / 2))
            .collect::<Vec<i32>>();
        attention
    }) else {
        return;
    };
    crate::glow::set_hover(hover);
    crate::glow::set_attention(attention);
}

// ----------------------------------------------- Vollbild & maximierte Fenster

unsafe fn same_monitor(a: HWND, b: HWND) -> bool {
    MonitorFromWindow(a, MONITOR_DEFAULTTONEAREST) == MonitorFromWindow(b, MONITOR_DEFAULTTONEAREST)
}

/// Echtes Vollbild-Fenster ohne Rahmen (Spiele, Videoplayer): weder
/// Titelleiste noch Größenrahmen. Rahmenlose Electron-Apps (Discord, VS Code)
/// haben einen Größenrahmen und zählen nicht dazu.
unsafe fn borderless(h: HWND) -> bool {
    let style = GetWindowLongW(h, GWL_STYLE) as u32;
    style & WS_CAPTION != WS_CAPTION && style & WS_THICKFRAME == 0
}

/// Bedeckt das Fenster seinen ganzen Monitor (inklusive Platz der Leiste)?
unsafe fn covers_monitor(h: HWND) -> bool {
    let m = monitor_rect(h, false);
    let mut r: RECT = zeroed();
    GetWindowRect(h, &mut r);
    r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
}

/// Maximierte Fenster, die noch nach dem alten Arbeitsbereich ausgerichtet
/// sind (z. B. maximiert, bevor unsere Leiste Platz reserviert hat), ragen
/// unter die Leiste oder lassen nach einem Umzug der Leiste eine Lücke.
/// Windows passt sie nicht von selbst an – also auf den Arbeitsbereich setzen.
pub unsafe fn fit_maximized(bar: HWND) {
    let work = monitor_rect(bar, true);
    let mut windows: Vec<HWND> = Vec::new();
    EnumWindows(Some(enum_cb), &mut windows as *mut _ as LPARAM);
    for h in windows {
        // Rahmenlose "maximierte" Vollbild-Fenster (Spiele, Videoplayer) nicht kürzen.
        if IsZoomed(h) == 0 || !same_monitor(h, bar) || (borderless(h) && covers_monitor(h)) {
            continue;
        }
        let mut r: RECT = zeroed();
        GetWindowRect(h, &mut r);
        // Maximierte Fenster stehen ringsum um ihre Rahmenbreite über. An den
        // Seiten ohne Leiste stimmt der Überstand; an der Seite einer neuen
        // (oder an eine andere Kante gezogenen) Leiste ist er größer bzw.
        // negativ – also der kleinste nicht-negative Überstand.
        let overhang = [work.left - r.left, work.top - r.top, r.right - work.right, r.bottom - work.bottom];
        let border = overhang.iter().copied().filter(|&o| o >= 0).min().unwrap_or(0);
        let target = RECT { left: work.left - border, top: work.top - border, right: work.right + border, bottom: work.bottom + border };
        let off = (r.left - target.left).abs().max((r.top - target.top).abs()).max((r.right - target.right).abs()).max((r.bottom - target.bottom).abs());
        if off > 1 {
            SetWindowPos(h, null_mut(), target.left, target.top, target.right - target.left, target.bottom - target.top, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS);
        }
    }
}

/// Deckt das Vordergrundfenster den ganzen Monitor ab (Spiel, Video, F11)?
/// Maximierte Fenster zählen nicht – die lassen die Leiste frei.
unsafe fn foreground_fullscreen(bar: HWND) -> bool {
    let fg = GetForegroundWindow();
    // Maximierte Fenster lassen die Leiste frei – außer rahmenlose (Spiele und
    // Videoplayer maximieren sich ohne Titelleiste über den ganzen Monitor).
    if fg.is_null() || (IsZoomed(fg) != 0 && !borderless(fg)) || !same_monitor(fg, bar) {
        return false;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(fg, &mut pid);
    if pid == windows_sys::Win32::System::Threading::GetCurrentProcessId() {
        return false;
    }
    let mut class = [0u16; 32];
    let n = GetClassNameW(fg, class.as_mut_ptr(), class.len() as i32) as usize;
    let class = String::from_utf16_lossy(&class[..n]);
    if class == "Progman" || class == "WorkerW" {
        return false;
    }
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST), &mut mi);
    let mut r: RECT = zeroed();
    GetWindowRect(fg, &mut r);
    let m = mi.rcMonitor;
    r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
}

/// Bei Vollbild ausblenden, danach wieder zeigen – wie die Windows-Leiste.
unsafe fn update_fullscreen(hwnd: HWND) {
    let now = foreground_fullscreen(hwnd);
    let changed = with(|s| std::mem::replace(&mut s.fullscreen, now) != now).unwrap_or(false);
    if !changed {
        return;
    }
    if now {
        crate::preview::hide();
        crate::tooltip::hide();
        ShowWindow(hwnd, SW_HIDE);
        SetTimer(self::hwnd(), TIMER_FULL, 1000, None);
    } else {
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
    crate::glow::sync();
}

// ---------------------------------------------------------- Usage-Anzeige

/// Unix-Sekunden → lokale Uhrzeit "HH:MM".
fn clock_of(unix: i64) -> String {
    unsafe {
        use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
        let ticks = (unix as u64 + 11_644_473_600) * 10_000_000;
        let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
        let (mut utc, mut local): (SYSTEMTIME, SYSTEMTIME) = (zeroed(), zeroed());
        if FileTimeToSystemTime(&ft, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local) == 0 {
            return String::new();
        }
        format!("{:02}:{:02}", local.wHour, local.wMinute)
    }
}

fn level_color(used: f32) -> COLORREF {
    if used >= 90.0 {
        rgb(255, 99, 71)
    } else if used >= 70.0 {
        rgb(255, 185, 0)
    } else {
        accent()
    }
}

/// CPU-Last aus der Differenz der Systemzeiten seit dem letzten Tick,
/// RAM-Auslastung direkt.
unsafe fn measure_load() {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetSystemTimes;
    let (mut idle, mut kernel, mut user): (FILETIME, FILETIME, FILETIME) = (zeroed(), zeroed(), zeroed());
    if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
        return;
    }
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    // Kernel-Zeit enthält die Leerlaufzeit bereits.
    let (idle, total) = (ft(idle), ft(kernel) + ft(user));
    let mut mem: MEMORYSTATUSEX = zeroed();
    mem.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    let ram = if GlobalMemoryStatusEx(&mut mem) != 0 { mem.dwMemoryLoad as f32 } else { 0.0 };
    with(|s| {
        let (pi, pt) = s.cpu_times;
        if pt != 0 && total > pt {
            let busy = 1.0 - (idle - pi) as f32 / (total - pt) as f32;
            s.cpu = (busy * 100.0).clamp(0.0, 100.0);
        }
        s.cpu_times = (idle, total);
        s.ram = ram;
    });
}

/// Task-Manager-Knopf: echtes Icon, darunter ein Mini-Balken mit der CPU-Last.
unsafe fn draw_taskmgr(dc: HDC, r: &RECT, dpi: u32, hover: f32, icon: HICON, cpu: f32) {
    if hover > 0.01 {
        round(dc, r, mix(BG, HOVER, hover), px(dpi, 6));
    }
    let is = px(dpi, 20);
    let cx = (r.left + r.right) / 2;
    let iy = r.top + px(dpi, 6);
    if !icon.is_null() {
        DrawIconEx(dc, cx - is / 2, iy, icon, is, is, 0, null_mut(), DI_NORMAL);
    }
    let track = RECT { left: cx - px(dpi, 10), top: r.bottom - px(dpi, 8), right: cx + px(dpi, 10), bottom: r.bottom - px(dpi, 5) };
    round(dc, &track, rgb(64, 64, 64), px(dpi, 3));
    let w = ((track.right - track.left) as f32 * cpu / 100.0).round() as i32;
    if w > 0 {
        round(dc, &RECT { right: track.left + w.max(px(dpi, 3)), ..track }, level_color(cpu), px(dpi, 3));
    }
}

/// Unten links: je Zeile Claude/Codex mit Balken fürs 5-Stunden-Fenster,
/// daneben 5h- und Wochen-Prozent.
unsafe fn draw_usage(dc: HDC, r: &RECT, dpi: u32, font: HFONT, hover: f32) {
    if hover > 0.01 {
        round(dc, r, mix(BG, HOVER, hover), px(dpi, 6));
    }
    let u = crate::usage::current().unwrap_or_default();
    let line = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS;
    // Nur eingeschaltete Dienste; zeilenweise auf die Höhe verteilt.
    let s = crate::settings::get();
    let rows: Vec<_> = [("Claude", &u.claude, u.claude_paused_until, s.usage_claude), ("Codex", &u.codex, u.codex_paused_until, s.usage_codex)]
        .into_iter()
        .filter(|r| r.3)
        .collect();
    let n = rows.len().max(1) as i32;
    let inner = RECT { top: r.top + px(dpi, 1), bottom: r.bottom - px(dpi, 1), ..*r };
    for (i, (name, res, paused, _)) in rows.into_iter().enumerate() {
        let i = i as i32;
        let h = inner.bottom - inner.top;
        let row = RECT { left: r.left + px(dpi, 8), right: r.right - px(dpi, 8), top: inner.top + h * i / n, bottom: inner.top + h * (i + 1) / n };
        text(dc, name, &RECT { right: row.left + px(dpi, 46), ..row }, font, SUBTEXT, line);
        let rest = RECT { left: row.left + px(dpi, 48), ..row };
        match res {
            Some(Ok(l)) => {
                let cy = (row.top + row.bottom) / 2;
                let five = l.five_hour.map(|w| w.used);
                let track = RECT { left: rest.left, top: cy - px(dpi, 2), right: rest.left + px(dpi, 56), bottom: cy + px(dpi, 2) };
                round(dc, &track, rgb(64, 64, 64), px(dpi, 4));
                if let Some(p) = five {
                    let w = ((track.right - track.left) as f32 * (p / 100.0).clamp(0.0, 1.0)) as i32;
                    if w > 0 {
                        round(dc, &RECT { right: track.left + w.max(px(dpi, 4)), ..track }, level_color(p), px(dpi, 4));
                    }
                }
                let pct = |w: Option<f32>| w.map(|p| format!("{:.0} %", p)).unwrap_or_else(|| "–".into());
                let label = match l.week {
                    Some(w) => format!("{} · Wo. {:.0} %", pct(five), w.used),
                    None => pct(five),
                };
                text(dc, &label, &RECT { left: track.right + px(dpi, 8), ..rest }, font, TEXT, line);
            }
            // Ohne Wert, aber vom Server gedrosselt (429): sagen, bis wann – damit es
            // nicht wie ein Fehler von Lightbar aussieht. Details im Tooltip.
            _ if paused > 0 => text(dc, &format!("gedrosselt bis {}", clock_of(paused)), &rest, font, DIM, line),
            Some(Err(_)) => text(dc, "nicht verfügbar", &rest, font, DIM, line),
            None => text(dc, "wird abgefragt …", &rest, font, DIM, line),
        }
    }
}

// ------------------------------------------------------- Mehrere Monitore
//
// Auf jedem weiteren Bildschirm eine eigene Leiste (wie "Taskleiste auf allen
// Bildschirmen anzeigen" bei Windows 11). Sie teilt Zustand, Zeichnen und
// Klicklogik mit der Hauptleiste – dieselbe WndProc, nur eine andere
// Fensterklasse (Tray-Nachrichten gehen weiter nur an `Shell_TrayWnd`) und
// eine eigene Platzreservierung auf ihrem Monitor. Tray-Übernahme,
// Screenreader und Sekundentakt bleiben bei der Hauptleiste.

static SECONDARY: std::sync::Mutex<Vec<isize>> = std::sync::Mutex::new(Vec::new());
/// Leiste, auf der zuletzt geklickt wurde – dort öffnen sich die Popups.
static ANCHOR: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

/// Skalierung einer Leiste. Zweitleisten können auf Monitoren mit anderer
/// Skalierung liegen (Laptop 150 %, externer Monitor 100 %).
unsafe fn bar_dpi(hwnd: HWND) -> u32 {
    if hwnd == self::hwnd() {
        with(|s| s.dpi).unwrap_or(96)
    } else {
        GetDpiForWindow(hwnd).max(96)
    }
}

thread_local! {
    /// Schriften für Zweitleisten mit anderer Skalierung (dpi → Text, Symbole).
    static FONTS: RefCell<HashMap<u32, (HFONT, HFONT)>> = RefCell::new(HashMap::new());
}

/// Text- und Symbolschrift für eine Leiste in ihrer Skalierung.
unsafe fn bar_fonts(hwnd: HWND) -> (u32, HFONT, HFONT) {
    let dpi = bar_dpi(hwnd);
    if let Some((main_dpi, font, icons)) = with(|s| (s.dpi, s.font, s.icons)) {
        if main_dpi == dpi {
            return (dpi, font, icons);
        }
    }
    let (font, icons) = FONTS.with(|f| *f.borrow_mut().entry(dpi).or_insert_with(|| (make_font(dpi, 12, FW_NORMAL), icon_font(dpi, 16))));
    (dpi, font, icons)
}

fn secondaries() -> Vec<HWND> {
    SECONDARY.lock().map(|v| v.iter().map(|&h| h as HWND).collect()).unwrap_or_default()
}

/// Hauptleiste und alle Zweitleisten.
pub fn all_bars() -> Vec<HWND> {
    let mut v = vec![hwnd()];
    v.extend(secondaries());
    v
}

/// Gehört das Fenster zu unseren Leisten (Haupt- oder Zweitleiste)?
pub fn is_bar(h: HWND) -> bool {
    h == hwnd() || secondaries().contains(&h)
}

/// Leiste, über der Popups (Start, Kalender, Schnelleinstellungen) erscheinen:
/// die zuletzt angeklickte, sonst die Hauptleiste.
pub fn anchor() -> HWND {
    let a = ANCHOR.load(Ordering::Relaxed);
    if !a.is_null() && is_bar(a) && unsafe { IsWindow(a) } != 0 {
        a
    } else {
        hwnd()
    }
}

unsafe fn monitor_rect(h: HWND, work: bool) -> RECT {
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi);
    if work {
        mi.rcWork
    } else {
        mi.rcMonitor
    }
}

/// Dicke einer Leiste: Höhe oben/unten, Breite links/rechts.
fn thickness(dpi: u32) -> i32 {
    if crate::display::edge().vertical() {
        px(dpi, BAR_W_V)
    } else {
        px(dpi, BAR_H)
    }
}

/// Leiste an die eingestellte Kante ihres Monitors legen und dort Platz
/// reservieren (AppBar), dann den Arbeitsbereich absichern.
unsafe fn dock(hwnd: HWND, dpi: u32) {
    use crate::display::Edge;
    let edge = crate::display::edge();
    let t = thickness(dpi);
    let m = monitor_rect(hwnd, false);
    let mut abd = appbar_data(hwnd);
    abd.rc = match edge {
        Edge::Bottom => RECT { top: m.bottom - t, ..m },
        Edge::Top => RECT { bottom: m.top + t, ..m },
        Edge::Left => RECT { right: m.left + t, ..m },
        Edge::Right => RECT { left: m.right - t, ..m },
    };
    // Explorer schiebt uns ggf. vor andere AppBars an dieser Kante; danach
    // wieder auf unsere Dicke bringen.
    SHAppBarMessage(ABM_QUERYPOS, &mut abd);
    match edge {
        Edge::Bottom => abd.rc.top = abd.rc.bottom - t,
        Edge::Top => abd.rc.bottom = abd.rc.top + t,
        Edge::Left => abd.rc.right = abd.rc.left + t,
        Edge::Right => abd.rc.left = abd.rc.right - t,
    }
    SHAppBarMessage(ABM_SETPOS, &mut abd);
    let r = abd.rc;
    MoveWindow(hwnd, r.left, r.top, r.right - r.left, r.bottom - r.top, 1);
    ensure_work_area(hwnd);
}

/// Zweitleiste an die eingestellte Kante ihres Monitors legen.
unsafe fn set_pos_secondary(hwnd: HWND) {
    dock(hwnd, bar_dpi(hwnd));
}

/// Seite des Arbeitsbereichs, die eine Leiste an `edge` begrenzt, und die
/// Innenkante der Leiste dort (bis wohin Fenster reichen dürfen).
fn work_side(work: &RECT, edge: crate::display::Edge) -> i32 {
    use crate::display::Edge;
    match edge {
        Edge::Bottom => work.bottom,
        Edge::Top => work.top,
        Edge::Left => work.left,
        Edge::Right => work.right,
    }
}

fn set_work_side(work: &mut RECT, edge: crate::display::Edge, v: i32) {
    use crate::display::Edge;
    match edge {
        Edge::Bottom => work.bottom = v,
        Edge::Top => work.top = v,
        Edge::Left => work.left = v,
        Edge::Right => work.right = v,
    }
}

fn inner_side(bar: &RECT, edge: crate::display::Edge) -> i32 {
    use crate::display::Edge;
    match edge {
        Edge::Bottom => bar.top,
        Edge::Top => bar.bottom,
        Edge::Left => bar.right,
        Edge::Right => bar.left,
    }
}

/// Reicht der Arbeitsbereich an dieser Kante weiter als `v` (Richtung Rand)?
fn beyond(side: i32, v: i32, edge: crate::display::Edge) -> bool {
    use crate::display::Edge;
    match edge {
        Edge::Bottom | Edge::Right => side > v,
        Edge::Top | Edge::Left => side < v,
    }
}

/// Arbeitsbereich-Kante, die wir selbst gesetzt haben, je Monitor (mit der
/// Kante der Leiste). Nur diesen Wert passen wir später auch in die andere
/// Richtung an (z. B. kleinere Skalierung → schmalere Leiste); einen Bereich,
/// den jemand anderes reserviert hat (eine weitere AppBar), lassen wir in Ruhe.
/// Zieht die Leiste an eine andere Kante, geben wir den alten Platz frei.
static SET_BY_US: std::sync::Mutex<Vec<(isize, crate::display::Edge, i32)>> = std::sync::Mutex::new(Vec::new());

fn set_by_us(mon: isize) -> Option<(crate::display::Edge, i32)> {
    SET_BY_US.lock().ok()?.iter().find(|e| e.0 == mon).map(|e| (e.1, e.2))
}

fn remember_set(mon: isize, value: Option<(crate::display::Edge, i32)>) {
    if let Ok(mut v) = SET_BY_US.lock() {
        v.retain(|e| e.0 != mon);
        if let Some((edge, side)) = value {
            v.push((mon, edge, side));
        }
    }
}

/// Die Leiste muss ihren Platz auch wirklich vom Arbeitsbereich abziehen,
/// sonst legen sich maximierte Fenster darunter. Normalerweise erledigt das
/// Explorer über die AppBar-Anmeldung; hat er es nicht getan (gemeldet auf
/// Systemen mit mehreren Bildschirmen), setzen wir den Arbeitsbereich dieses
/// Monitors selbst.
unsafe fn ensure_work_area(bar: HWND) {
    if IsWindowVisible(bar) == 0 {
        return;
    }
    let edge = crate::display::edge();
    let mon = MonitorFromWindow(bar, MONITOR_DEFAULTTONEAREST) as isize;
    // Leiste an eine andere Kante gezogen: unseren alten Platz zuerst freigeben.
    if set_by_us(mon).is_some_and(|(e, _)| e != edge) {
        release_work_area(bar);
    }
    let mut br: RECT = zeroed();
    GetWindowRect(bar, &mut br);
    let mut work = monitor_rect(bar, true);
    let inner = inner_side(&br, edge);
    let side = work_side(&work, edge);
    let too_big = beyond(side, inner, edge);
    let ours_too_small = side != inner && !too_big && set_by_us(mon) == Some((edge, side));
    if too_big || ours_too_small {
        set_work_side(&mut work, edge, inner);
        set_work_area(work);
        remember_set(mon, Some((edge, inner)));
        // Explorer hat den Platz der Leiste nicht reserviert. Höchstens einmal pro
        // Minute protokollieren, falls er immer wieder dagegenhält.
        static LAST_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let t = windows_sys::Win32::System::SystemInformation::GetTickCount64();
        if t.saturating_sub(LAST_LOG.swap(t, Ordering::Relaxed)) > 60_000 {
            crate::util::log(&format!("Arbeitsbereich korrigiert ({edge:?}): reichte bis {side}, Leiste beginnt bei {inner}"));
        }
        fit_maximized(bar);
    }
}

/// Arbeitsbereich setzen, ohne WM_SETTINGCHANGE zu verschicken.
/// Mit SPIF_SENDCHANGE rechnet Explorer daraufhin neu, setzt den Bereich
/// zurück, wir setzen ihn wieder … – er sprang im Halbsekundentakt, und weil
/// die Meldung synchron an alle Fenster geht, hing die Leiste dabei an
/// Explorer bzw. PowerToys FancyZones fest (kein WM_PAINT, unsichtbar).
/// Maximierte Fenster passt `fit_maximized` an.
unsafe fn set_work_area(mut work: RECT) {
    SystemParametersInfoW(SPI_SETWORKAREA, 0, &mut work as *mut RECT as *mut c_void, 0);
}

/// Den Platz, den wir auf dem Monitor dieser Leiste selbst reserviert hatten,
/// wieder freigeben (Zweitleiste entfernt, Leiste an andere Kante gezogen).
unsafe fn release_work_area(bar: HWND) {
    let mut work = monitor_rect(bar, true);
    let m = monitor_rect(bar, false);
    let mon = MonitorFromWindow(bar, MONITOR_DEFAULTTONEAREST) as isize;
    // Nur zurückgeben, was wir selbst gesetzt haben (die Leiste kann beim
    // Abstecken eines Monitors schon woanders hingeschoben worden sein).
    if let Some((edge, side)) = set_by_us(mon) {
        if work_side(&work, edge) == side {
            set_work_side(&mut work, edge, work_side(&m, edge));
            set_work_area(work);
        }
        remember_set(mon, None);
    }
}

/// Leisten an die neue Kante ziehen (Position in Windows geändert).
unsafe fn relocate() {
    crate::util::log(&format!("Taskleistenposition: {:?}", crate::display::edge()));
    crate::preview::hide();
    crate::tooltip::hide();
    tray::hide_overflow();
    set_pos(self::hwnd());
    sync_monitors();
}

/// Taskleisten-Einstellungen von Windows neu lesen und anwenden: Ausrichtung,
/// Bildschirme und Position. Zieht die Leiste an eine andere Kante, legen sich
/// alle Leisten neu an (Platz, Arbeitsbereich, maximierte Fenster, Leuchtrand).
unsafe fn apply_taskbar_settings() {
    if crate::display::reload() {
        relocate();
    }
    sync_monitors();
    for b in all_bars() {
        InvalidateRect(b, null(), 0);
    }
}

/// Zweitleisten an die aktuelle Monitor-Anordnung anpassen: neue Monitore
/// bekommen eine Leiste (wenn gewünscht, siehe `display`), entfernte oder
/// abgewählte verlieren ihre, vorhandene werden neu ausgerichtet.
pub unsafe fn sync_monitors() {
    let mut monitors: Vec<(HMONITOR, RECT)> = crate::display::list()
        .into_iter()
        .filter(|m| !m.primary && crate::display::wanted(m))
        .map(|m| (m.handle, m.rect))
        .collect();
    let existing = secondaries();
    let mut keep = Vec::new();
    for h in existing {
        let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONULL);
        if let Some(i) = monitors.iter().position(|(m, _)| *m == mon) {
            monitors.remove(i);
            set_pos_secondary(h);
            keep.push(h as isize);
        } else {
            DestroyWindow(h);
        }
    }
    let inst = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null());
    for (_, rect) in monitors {
        if let Some(h) = create_secondary(inst, rect) {
            keep.push(h as isize);
        }
    }
    if let Ok(mut v) = SECONDARY.lock() {
        *v = keep;
    }
    crate::glow::sync();
}

unsafe fn create_secondary(inst: HINSTANCE, rect: RECT) -> Option<HWND> {
    let class = w16("LightbarSecondary");
    // Mehrfaches Registrieren schlägt nur fehl, schadet aber nicht.
    register_class(inst, &class, Some(wndproc), CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS);
    // Gleich auf dem Zielmonitor anlegen, damit MonitorFromWindow stimmt.
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        class.as_ptr(),
        w16("Lightbar").as_ptr(),
        WS_POPUP,
        rect.left,
        rect.bottom - 1,
        1,
        1,
        null_mut(),
        null_mut(),
        inst,
        null(),
    );
    if hwnd.is_null() {
        return None;
    }
    let mut abd = appbar_data(hwnd);
    SHAppBarMessage(ABM_NEW, &mut abd);
    set_pos_secondary(hwnd);
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    ensure_work_area(hwnd);
    Some(hwnd)
}

/// Sekundentakt der Hauptleiste für die Zweitleisten: maximierte Fenster
/// kürzen und bei Vollbild auf ihrem Monitor aus-/einblenden.
unsafe fn tick_secondaries() {
    let mut changed = false;
    for h in secondaries() {
        ensure_work_area(h);
        fit_maximized(h);
        let full = foreground_fullscreen(h);
        let visible = IsWindowVisible(h) != 0;
        if full && visible {
            ShowWindow(h, SW_HIDE);
            SetTimer(self::hwnd(), TIMER_FULL, 1000, None);
            changed = true;
        } else if !full && !visible {
            SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
            changed = true;
        }
    }
    if changed {
        crate::glow::sync();
    }
}

/// Startsymbol: standardmäßig ein eigener Orb (Akzentfarbe, versetzter dunkler
/// Kern, Glanzpunkt); auf Wunsch das Windows-Logo (Marke von Microsoft,
/// deshalb nicht voreingestellt).
unsafe fn draw_logo(dc: HDC, r: &RECT, dpi: u32) {
    use crate::settings::StartLogo;
    let logo = crate::settings::get().start_logo;
    if logo == StartLogo::Tux {
        draw_tux(dc, r, dpi);
        return;
    }
    if logo == StartLogo::Windows {
        let (q, gap) = (px(dpi, 8), px(dpi, 1).max(1));
        let size = 2 * q + gap;
        let x = r.left + (r.right - r.left - size) / 2;
        let y = r.top + (r.bottom - r.top - size) / 2;
        for (dx, dy) in [(0, 0), (q + gap, 0), (0, q + gap), (q + gap, q + gap)] {
            fill(dc, &RECT { left: x + dx, top: y + dy, right: x + dx + q, bottom: y + dy + q }, accent());
        }
        return;
    }
    let size = px(dpi, 20);
    let x = r.left + (r.right - r.left - size) / 2;
    let y = r.top + (r.bottom - r.top - size) / 2;
    let orb = RECT { left: x, top: y, right: x + size, bottom: y + size };
    round(dc, &orb, accent(), size);
    let core = px(dpi, 9);
    let (cx, cy) = (x + size / 2 + px(dpi, 2), y + size / 2 + px(dpi, 2));
    round(dc, &RECT { left: cx - core / 2, top: cy - core / 2, right: cx + core / 2, bottom: cy + core / 2 }, BG, core);
    let shine = px(dpi, 4).max(2);
    let (sx, sy) = (x + px(dpi, 5), y + px(dpi, 5));
    round(dc, &RECT { left: sx, top: sy, right: sx + shine, bottom: sy + shine }, mix(accent(), rgb(255, 255, 255), 0.6), shine);
}

/// Der sitzende Tux (Larry Ewing) als Strichzeichnung wie die bekannte
/// einfarbige Fassung, entworfen auf 20 × 22 px bei 100 %: Umriss,
/// Gesichtsmaske mit Schnabel, Bauch und Füße in der Akzentfarbe, der Körper
/// selbst bleibt Hintergrund.
///
/// Bei 22 px würden Augen und Schnabel auf ganze Pixel gerundet und den Kopf
/// zerfressen. Deshalb die Form vierfach groß als Maske zeichnen, je 4 × 4
/// Pixel zu einer Deckung mitteln und damit die Akzentfarbe über den
/// vorhandenen Hintergrund mischen (HALFTONE schwingt über: helle und fast
/// schwarze Säume).
unsafe fn draw_tux(dc: HDC, r: &RECT, dpi: u32) {
    const SS: i32 = 4;
    // Entwurf 20 × 22, gezeichnet 26 px hoch: kleiner verschwimmt das Gesicht.
    let s = dpi as f32 / 96.0 * 26.0 / 22.0;
    let (w, h) = ((20.0 * s).round() as i32, (22.0 * s).round() as i32);
    let (x, y) = (r.left + (r.right - r.left - w) / 2, r.top + (r.bottom - r.top - h) / 2);
    let (bw, bh) = (w * SS, h * SS);
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = bw;
    bi.bmiHeader.biHeight = -bh; // von oben nach unten
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    let mut bits: *mut c_void = null_mut();
    let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if bmp.is_null() {
        return;
    }
    let mem = CreateCompatibleDC(dc);
    let old = SelectObject(mem, bmp);
    // Maske: weiß = Akzentfarbe, schwarz = Hintergrund.
    let (a, bg) = (rgb(255, 255, 255), rgb(0, 0, 0));
    fill(mem, &RECT { left: 0, top: 0, right: bw, bottom: bh }, bg);
    let s = s * SS as f32;
    let at = |vx: f32, vy: f32| POINT { x: (vx * s).round() as i32, y: (vy * s).round() as i32 };
    // Füllen ohne Rand. Ellipse statt `round`: RoundRect mit quadratischem
    // Radius verzerrt nicht quadratische Flächen zu Spitzen.
    let fill_with = |c: COLORREF, draw: &dyn Fn()| {
        let brush = CreateSolidBrush(c);
        let ob = SelectObject(mem, brush);
        let op = SelectObject(mem, GetStockObject(NULL_PEN));
        draw();
        SelectObject(mem, op);
        SelectObject(mem, ob);
        DeleteObject(brush);
    };
    let ellipse = |x0: f32, y0: f32, x1: f32, y1: f32, c: COLORREF| {
        let (a, b) = (at(x0, y0), at(x1, y1));
        // Mit NULL_PEN lässt Ellipse rechts/unten ein Pixel weg.
        fill_with(c, &|| {
            Ellipse(mem, a.x, a.y, b.x + 1, b.y + 1);
        });
    };
    let polygon = |pts: [(f32, f32); 4], c: COLORREF| {
        let pts = pts.map(|(vx, vy)| at(vx, vy));
        fill_with(c, &|| {
            Polygon(mem, pts.as_ptr(), pts.len() as i32);
        });
    };
    // Umriss: Silhouette aus Rumpf, Kopf und Flossen, innen wieder ausgespart.
    const T: f32 = 1.0;
    ellipse(2.5, 6.5, 17.5, 21.5, a);
    ellipse(5.5, 0.0, 14.5, 11.5, a);
    polygon([(4.2, 9.0), (1.0, 16.5), (2.6, 18.2), (5.6, 12.5)], a);
    polygon([(15.8, 9.0), (19.0, 16.5), (17.4, 18.2), (14.4, 12.5)], a);
    ellipse(2.5 + T, 6.5 + T, 17.5 - T, 21.5 - T, bg);
    ellipse(5.5 + T, T, 14.5 - T, 11.5 - T, bg);
    polygon([(4.6, 10.5), (2.0, 16.3), (2.9, 17.2), (5.8, 13.2)], bg);
    polygon([(15.4, 10.5), (18.0, 16.3), (17.1, 17.2), (14.2, 13.2)], bg);
    ellipse(5.5, 9.5, 15.0, 20.5, a); // Bauch
    // Augen mit Pupillen, darunter der breite Schnabel.
    ellipse(7.4, 3.4, 9.9, 6.8, a);
    ellipse(10.1, 3.4, 12.6, 6.8, a);
    ellipse(8.5, 4.4, 9.5, 6.1, bg);
    ellipse(10.5, 4.4, 11.5, 6.1, bg);
    ellipse(7.0, 5.8, 13.0, 8.4, a);
    ellipse(0.2, 17.0, 8.8, 22.0, a); // Füße
    ellipse(11.2, 17.0, 19.8, 22.0, a);
    GdiFlush();
    let mask = std::slice::from_raw_parts(bits as *const u8, (bw * bh * 4) as usize);
    let accent = accent();
    for py in 0..h {
        for px_ in 0..w {
            let mut sum = 0u32;
            for sy in 0..SS {
                let row = ((py * SS + sy) * bw + px_ * SS) as usize * 4;
                for sx in 0..SS as usize {
                    sum += mask[row + sx * 4 + 1] as u32;
                }
            }
            if sum == 0 {
                continue;
            }
            let k = sum as f32 / (255 * SS * SS) as f32;
            let under = GetPixel(dc, x + px_, y + py);
            SetPixelV(dc, x + px_, y + py, mix(under, accent, k));
        }
    }
    SelectObject(mem, old);
    DeleteObject(bmp);
    DeleteDC(mem);
}

/// Uhr samt Hover-Fläche zeichnen (volles Zeichnen und Schnellweg).
unsafe fn draw_clock(dc: HDC, r: &RECT, dpi: u32, font: HFONT, hover: f32) {
    if hover > 0.01 {
        round(dc, r, mix(BG, HOVER, hover), px(dpi, 6));
    }
    let mut st: SYSTEMTIME = zeroed();
    GetLocalTime(&mut st);
    let mid = (r.top + r.bottom) / 2;
    // Schmale Uhr (senkrechte Leiste): mittig und Datum ohne Jahr.
    let narrow = r.right - r.left < px(dpi, CLOCK_W);
    let flags = if narrow { DT_CENTER } else { DT_RIGHT } | DT_SINGLELINE;
    let cr = if narrow { *r } else { RECT { right: r.right - px(dpi, 10), ..*r } };
    let time = if crate::settings::get().clock_seconds {
        format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
    } else {
        format!("{:02}:{:02}", st.wHour, st.wMinute)
    };
    text(dc, &time, &RECT { bottom: mid, ..cr }, font, TEXT, flags | DT_BOTTOM);
    let date = if narrow { format!("{:02}.{:02}.", st.wDay, st.wMonth) } else { format!("{:02}.{:02}.{}", st.wDay, st.wMonth, st.wYear) };
    text(dc, &date, &RECT { top: mid, ..cr }, font, TEXT, flags | DT_TOP);
}

/// Ist nur die Uhr ungültig (Sekunden-/Minutentakt)?
unsafe fn clock_only_update(hwnd: HWND) -> Option<RECT> {
    let mut ur: RECT = zeroed();
    if GetUpdateRect(hwnd, &mut ur, 0) == 0 {
        return None;
    }
    let (l, _) = current_layout(hwnd)?;
    let c = l.clock;
    (ur.left >= c.left && ur.right <= c.right && ur.top >= c.top && ur.bottom <= c.bottom).then_some(c)
}

/// Schnellweg: nur die Uhr in einen kleinen Puffer zeichnen statt der ganzen Leiste.
unsafe fn paint_clock_only(hwnd: HWND, c: RECT) {
    let Some((_, _, k)) = with(|s| {
        let hover = if s.hover_hwnd == hwnd { s.anim.hover.get("clock").copied().unwrap_or(0.0) } else { 0.0 };
        (s.dpi, s.font, if crate::calendar::visible() { 1.0 } else { hover })
    }) else {
        return;
    };
    let (dpi, font, _) = bar_fonts(hwnd);
    let mut ps: PAINTSTRUCT = zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let (w, h) = (c.right - c.left, c.bottom - c.top);
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old = SelectObject(mem, bmp);
    // Leisten-Koordinaten beibehalten: Ursprung des Puffers liegt bei der Uhr.
    SetViewportOrgEx(mem, -c.left, -c.top, null_mut());
    fill(mem, &c, BG);
    draw_clock(mem, &c, dpi, font, k);
    BitBlt(hdc, c.left, c.top, w, h, mem, c.left, c.top, SRCCOPY);
    SelectObject(mem, old);
    DeleteObject(bmp);
    DeleteDC(mem);
    EndPaint(hwnd, &ps);
}

unsafe fn paint(hwnd: HWND) {
    if let Some(c) = clock_only_update(hwnd) {
        paint_clock_only(hwnd, c);
        return;
    }
    let snapshot = with(|s| {
        let groups: Vec<(String, Vec<HWND>, HICON, bool)> = s
            .groups
            .iter()
            .map(|g| (format!("g:{}", g.key), g.windows.clone(), g.icon, g.windows.iter().any(|w| s.attention.contains(w))))
            .collect();
        let dragged = s.drag.as_ref().filter(|d| d.active).and_then(|d| s.groups.iter().find(|g| g.slot == d.slot).map(|g| (format!("g:{}", g.key), d.x)));
        (s.dpi, s.font, s.icons, s.status.clone(), groups, s.anim.clone(), s.start.elapsed().as_secs_f32(), s.taskmgr_icon, s.cpu, dragged, s.hover_hwnd == hwnd)
    });
    let Some((_, _, _, status, groups, anim, t, taskmgr_icon, cpu, dragged, hover_here)) = snapshot else {
        let mut ps: PAINTSTRUCT = zeroed();
        BeginPaint(hwnd, &mut ps);
        EndPaint(hwnd, &ps);
        return;
    };
    let (dpi, font, icons) = bar_fonts(hwnd);
    let slide = animates_positions(hwnd);
    let hv = |id: &str| if hover_here { anim.hover.get(id).copied().unwrap_or(0.0) } else { 0.0 };
    let Some((l, tray)) = current_layout(hwnd) else {
        ValidateRect(hwnd, null());
        return;
    };
    let focus = if hwnd == self::hwnd() { focus_rect(hwnd) } else { None };
    let settings = crate::settings::get();
    let mono_on = settings.mono_tray;
    paint_buffered(hwnd, |dc, w, h| {
        let rc = RECT { left: 0, top: 0, right: w, bottom: h };
        fill(dc, &rc, BG);
        fill(dc, &RECT { left: 0, top: 0, right: w, bottom: px(dpi, 1).max(1) }, BORDER);

        let battery = status.as_ref().and_then(|s| s.battery);
        let radius = px(dpi, 6);
        let hover_bg = |dc: HDC, r: &RECT, k: f32| {
            if k > 0.01 {
                round(dc, r, mix(BG, HOVER, k), radius);
            }
        };
        hover_bg(dc, &l.start, hv("start"));
        draw_logo(dc, &l.start, dpi);

        let fg = GetForegroundWindow();
        let is = px(dpi, ICON);
        for (i, (id, windows, icon, attention)) in groups.iter().enumerate() {
            let Some(base) = l.tasks.get(i) else { continue };
            // Weich nachrutschende Position.
            let dx = if slide { anim.x.get(id).map(|x| x.round() as i32 - (base.left - l.start.left)).unwrap_or(0) } else { 0 };
            let mut r = RECT { left: base.left + dx, right: base.right + dx, ..*base };
            // Gezogener Button klebt am Mauszeiger.
            let is_dragged = dragged.as_ref().is_some_and(|(did, _)| did == id);
            if let Some((_, mx)) = dragged.as_ref().filter(|_| is_dragged) {
                let w = base.right - base.left;
                r = RECT { left: mx - w / 2, right: mx - w / 2 + w, ..*base };
                round(dc, &r, mix(ACTIVE, rgb(72, 72, 72), 0.6), radius);
            }
            let active = windows.iter().any(|w| *w == fg);
            if *attention {
                let pulse = 0.5 + 0.5 * (t * 6.0).sin();
                round(dc, &r, mix(rgb(92, 58, 18), rgb(160, 96, 20), pulse), radius);
            } else if active {
                round(dc, &r, mix(ACTIVE, rgb(66, 66, 66), hv(id)), radius);
            } else {
                hover_bg(dc, &r, hv(id));
            }
            // Einblenden (wächst aus 55 %) und Feder beim Klick.
            let appear = anim::ease_out(anim.appear.get(id).copied().unwrap_or(0.0));
            let press = match &anim.press {
                Some((pid, t0)) if pid == id => {
                    let p = (t0.elapsed().as_secs_f32() / PRESS_S).min(1.0);
                    1.0 - 0.16 * (p * std::f32::consts::PI).sin()
                }
                _ => 1.0,
            };
            let size = ((is as f32) * (0.55 + 0.45 * appear) * press).round() as i32;
            if size > 2 {
                let cx = (r.left + r.right) / 2;
                let cy = (r.top + r.bottom) / 2 - px(dpi, 2);
                DrawIconEx(dc, cx - size / 2, cy - size / 2, *icon, size, size, 0, null_mut(), DI_NORMAL);
            }
            let iw = anim.ind.get(id).copied().unwrap_or(0.0).round() as i32;
            if iw >= 1 {
                let ih = px(dpi, 3).max(2);
                let cx = (r.left + r.right) / 2;
                let ind = RECT { left: cx - iw / 2, top: r.bottom - ih - 1, right: cx + iw - iw / 2, bottom: r.bottom - 1 };
                if windows.len() > 1 && !active && iw <= px(dpi, 8) {
                    // Mehrere Fenster: zwei kurze Striche statt einem.
                    let seg = px(dpi, 4).max(2);
                    let gap = px(dpi, 3).max(2);
                    let left = RECT { left: cx - seg - gap / 2, right: cx - gap / 2, ..ind };
                    let right = RECT { left: cx + gap - gap / 2, right: cx + gap - gap / 2 + seg, ..ind };
                    round(dc, &left, DIM, ih);
                    round(dc, &right, DIM, ih);
                } else {
                    round(dc, &ind, if active { accent() } else { DIM }, ih);
                }
            }
        }

        // "…": Apps ohne Platz in der Leiste.
        if let Some(r) = l.more {
            hover_bg(dc, &r, hv("more"));
            text(dc, "\u{E712}", &r, icons, TEXT, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }

        // Infobereich
        let ts = px(dpi, TRAY_ICON);
        for (n, &(index, icon)) in tray.iter().enumerate() {
            let r = l.tray[n];
            hover_bg(dc, &r, hv(&format!("t:{index}")));
            let icon = if mono_on { tray::mono(icon, ts) } else { icon };
            DrawIconEx(dc, r.left + (r.right - r.left - ts) / 2, r.top + (r.bottom - r.top - ts) / 2, icon, ts, ts, 0, null_mut(), DI_NORMAL);
        }
        if let Some(r) = l.chevron {
            let k = if tray::overflow_visible() { 1.0 } else { hv("chevron") };
            hover_bg(dc, &r, k);
            text(dc, "\u{E70E}", &r, icons, TEXT, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }

        // Status: Netz, Lautstärke, Akku
        hover_bg(dc, &l.status, hv("status"));
        if let Some(s) = status.as_ref().filter(|_| l.status.right > l.status.left) {
            // Symbolbreite nach Platz (schmale senkrechte Leiste), mittig verteilt.
            let n = 2 + battery.is_some() as i32;
            let avail = l.status.right - l.status.left;
            let gw = px(dpi, STATUS_GLYPH_W).min(avail / n);
            let mut gx = l.status.left + (avail - n * gw) / 2;
            let mut glyph = |g: &str| {
                text(dc, g, &RECT { left: gx, right: gx + gw, ..l.status }, icons, TEXT, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                gx += gw;
            };
            glyph(status::net_glyph(&s.net));
            glyph(status::volume_glyph(s.volume, s.muted));
            if let Some((p, charging)) = battery {
                glyph(&status::battery_glyph(p, charging));
            }
        }

        // Uhr
        let k = if crate::calendar::visible() { 1.0 } else { hv("clock") };
        draw_clock(dc, &l.clock, dpi, font, k);

        if l.show_usage {
            draw_usage(dc, &l.usage, dpi, font, hv("usage"));
        }
        if l.taskmgr.right > l.taskmgr.left {
            draw_taskmgr(dc, &l.taskmgr, dpi, hv("taskmgr"), taskmgr_icon, cpu);
        }

        // Tastaturfokus (Win+T): deutlich sichtbarer Rahmen.
        if let Some(r) = focus {
            outline(dc, &r, TEXT, radius);
            outline(dc, &RECT { left: r.left + 1, top: r.top + 1, right: r.right - 1, bottom: r.bottom - 1 }, TEXT, radius);
        }

        // "Desktop anzeigen": dünne Kante ganz rechts, blendet ein.
        let k = hv("desktop");
        if k > 0.01 {
            let line = RECT { left: l.desktop.left + px(dpi, 3), top: l.desktop.top + px(dpi, 12), right: l.desktop.left + px(dpi, 4), bottom: l.desktop.bottom - px(dpi, 12) };
            fill(dc, &line, mix(BG, DIM, k));
        }
    });
}

// --------------------------------------------------------------- WndProc

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (shell_msg, created_msg) = with(|s| (s.shell_msg, s.created_msg)).unwrap_or((0, 0));
    if created_msg != 0 && msg == created_msg {
        // Explorer wurde neu gestartet und hat AppBars vergessen: neu anmelden.
        // (Kein eigenes announce() hier – wir empfangen auch unseren Broadcast.)
        let mut abd = appbar_data(hwnd);
        SHAppBarMessage(ABM_NEW, &mut abd);
        set_pos(hwnd);
        // Nach dem Login kann Explorer später fertig sein als wir: dann seine
        // Leiste jetzt (erneut) ausblenden und auf Auto-Ausblenden stellen.
        crate::shell::takeover();
        schedule_rehide();
        return 0;
    }
    if shell_msg != 0 && msg == shell_msg {
        prof(0);
        // HSHELL_FLASH (0x8006): Fenster will Aufmerksamkeit; Aktivierung beendet das.
        let target = lp as HWND;
        match wp as u32 {
            0x8006 => {
                with(|s| if !s.attention.contains(&target) { s.attention.push(target) });
            }
            // 0x8004 = HSHELL_RUDEAPPACTIVATED (fehlt in windows-sys)
            HSHELL_WINDOWACTIVATED | 0x8004 => {
                with(|s| s.attention.retain(|&w| w != target));
            }
            _ => {}
        }
        // Nur so viel tun wie nötig: REDRAW kommt bei jeder Titeländerung (Browser,
        // Chats, Fortschrittsanzeigen) mehrmals pro Sekunde und ändert die Leiste nicht.
        if std::env::var("LIGHTBAR_PROFILE").is_ok() {
            let mut cls = [0u16; 64];
            let n = GetClassNameW(target, cls.as_mut_ptr(), 64).max(0) as usize;
            let exe = exe_of(target).unwrap_or_default();
            let exe = exe.rsplit('\\').next().unwrap_or("").to_string();
            PROF_DETAIL.with(|d| *d.borrow_mut().entry(format!("{}:{}:{}", wp & 0x7fff, String::from_utf16_lossy(&cls[..n]), exe)).or_insert(0) += 1);
        }
        match (wp & 0x7fff) as u32 {
            // Nur Fenster, die auf die Leiste gehören – Hintergrundprozesse erzeugen
            // ständig unsichtbare Fenster (z. B. Konsolen), die uns nichts angehen.
            HSHELL_WINDOWCREATED if is_task_window(target) => refresh(hwnd, None),
            HSHELL_WINDOWDESTROYED if is_known(target) => refresh(hwnd, None),
            HSHELL_WINDOWREPLACED => refresh(hwnd, Some(target)),
            HSHELL_WINDOWACTIVATED => {
                // Aktiv-Strich wandert, Vollbild sofort erkennen.
                update_fullscreen(hwnd);
                sync_glow(hwnd);
                kick(hwnd);
            }
            HSHELL_REDRAW if wp & 0x8000 != 0 => kick(hwnd), // Blinken (Aufmerksamkeit)
            _ => {}
        }
        return 0;
    }
    match msg {
        WM_COPYDATA => {
            prof(7);
            if let Some(r) = tray::handle_copydata(lp) {
                InvalidateRect(hwnd, null(), 0);
                return r;
            }
            // AppBar & Co. gehören weiter Explorer.
            let ex = tray::explorer_tray(hwnd);
            if ex.is_null() {
                return 0;
            }
            let mut result: usize = 0;
            SendMessageTimeoutW(ex, WM_COPYDATA, wp, lp, SMTO_ABORTIFHUNG, 3000, &mut result);
            result as LRESULT
        }
        WM_PAINT => {
            prof(4);
            let clock_only = clock_only_update(hwnd).is_some();
            paint(hwnd);
            // Zweitleisten zeigen denselben Zustand: mitzeichnen – beim Uhr-Takt nur die Uhr.
            if hwnd == self::hwnd() {
                for h in secondaries() {
                    match current_layout(h).filter(|_| clock_only) {
                        Some((l, _)) => InvalidateRect(h, &l.clock, 0),
                        None => InvalidateRect(h, null(), 0),
                    };
                }
            }
            0
        }
        WM_ERASEBKGND => 1,
        // Klicks dürfen den Fokus nicht klauen, sonst funktioniert Minimieren per Klick nicht.
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_TIMER if wp == TIMER_ANIM => {
            if !step_anim(hwnd) {
                KillTimer(hwnd, TIMER_ANIM);
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_TIMER if wp == TIMER_TIP => {
            KillTimer(hwnd, TIMER_TIP);
            show_tip(hwnd);
            0
        }
        WM_BAR_WINX => {
            menus::winx(hwnd);
            0
        }
        WM_BAR_QUICK => {
            crate::quick::toggle();
            0
        }
        WM_BAR_USAGE => {
            InvalidateRect(hwnd, null(), 0);
            0
        }
        // Screenreader fragen nach unserem UI-Automation-Provider.
        WM_GETOBJECT if hwnd == self::hwnd() && lp as i32 == windows::Win32::UI::Accessibility::UiaRootObjectId => {
            use windows::Win32::Foundation as wf;
            let root = crate::uia::root(hwnd as isize);
            windows::Win32::UI::Accessibility::UiaReturnRawElementProvider(wf::HWND(hwnd as _), wf::WPARAM(wp), wf::LPARAM(lp), &root).0
        }
        WM_BAR_INVOKE => {
            click_element(hwnd, wp, WM_LBUTTONUP);
            0
        }
        WM_BAR_FOCUS => {
            // Win+T: auf die erste App, sonst auf Start.
            let els = ui_elements();
            let first = els.iter().position(|e| matches!(e.hit, Hit::Task(_))).or_else(|| els.iter().position(|e| e.hit == Hit::Start));
            ui_set_focus(first.unwrap_or(0));
            0
        }
        WM_KEYDOWN | WM_SYSKEYDOWN if focus_key(hwnd, wp as u16) => 0,
        WM_KILLFOCUS => {
            if with(|s| s.kb_focus.take().is_some()).unwrap_or(false) {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_BAR_NUM => {
            activate_nth(wp);
            0
        }
        // Im Leerlauf soll Lightbar fast nichts tun: Änderungen kommen als
        // Ereignisse (Shell-Hook, WinEvents, Audio-/Netz-/Akku-Meldungen).
        // Zeitgeber gibt es nur für das, was sich wirklich mit der Zeit ändert.
        WM_TIMER if wp == TIMER_TICK => {
            prof(5);
            // Uhr: genau zum nächsten Minuten- bzw. Sekundenwechsel.
            if let Some((l, _)) = current_layout(hwnd) {
                InvalidateRect(hwnd, &l.clock, 0);
            }
            schedule_clock(hwnd);
            0
        }
        WM_TIMER if wp == TIMER_CPU => {
            // CPU-Balken: messen ist billig, neu zeichnen nur bei sichtbarer Änderung.
            let before = with(|s| s.cpu).unwrap_or(0.0);
            measure_load();
            let after = with(|s| s.cpu).unwrap_or(0.0);
            if (before / 5.0).round() != (after / 5.0).round() {
                if let Some((l, _)) = current_layout(hwnd) {
                    InvalidateRect(hwnd, &l.taskmgr, 0);
                }
            }
            0
        }
        WM_TIMER if wp == TIMER_REHIDE => {
            KillTimer(hwnd, TIMER_REHIDE);
            crate::shell::ensure_hidden();
            0
        }
        WM_TIMER if wp == TIMER_WORKAREA => {
            KillTimer(hwnd, TIMER_WORKAREA);
            // Notbremse: Hält jemand dauerhaft dagegen, nicht endlos nachsetzen.
            static WINDOW: AtomicU64 = AtomicU64::new(0);
            static COUNT: AtomicU64 = AtomicU64::new(0);
            let now = windows_sys::Win32::System::SystemInformation::GetTickCount64() / 60_000;
            if WINDOW.swap(now, Ordering::Relaxed) != now {
                COUNT.store(0, Ordering::Relaxed);
            }
            if COUNT.fetch_add(1, Ordering::Relaxed) < 10 {
                for b in all_bars() {
                    ensure_work_area(b);
                }
            }
            0
        }
        WM_TIMER if wp == TIMER_FULL => {
            update_fullscreen(hwnd);
            tick_secondaries();
            // Die ausgeblendete Hauptleiste bekommt kein WM_PAINT – und nur sie hat
            // bisher die Zweitleisten mitgezeichnet. Solange: hier direkt (Uhr,
            // Fensterliste, aktive App bleiben so auf den anderen Monitoren aktuell).
            if with(|s| s.fullscreen).unwrap_or(false) {
                for h in secondaries() {
                    InvalidateRect(h, null(), 0);
                }
            }
            let still = with(|s| s.fullscreen).unwrap_or(false) || secondaries().iter().any(|&h| IsWindowVisible(h) == 0);
            if !still {
                KillTimer(hwnd, TIMER_FULL);
            }
            0
        }
        WM_TIMER if wp == TIMER_REFRESH => {
            // Gebündelte Fensterlisten-Aktualisierung nach WinEvents.
            KillTimer(hwnd, TIMER_REFRESH);
            refresh(hwnd, None);
            0
        }
        WM_TIMER if wp == TIMER_SLOW => {
            // Sicherheitsnetz alle 30 s für das, was niemand meldet: WLAN-
            // Signalstärke, verwaiste Tray-Icons, Explorer-Leiste wieder sichtbar,
            // Z-Order gegenüber Explorer.
            refresh(hwnd, None);
            update_status(hwnd);
            tray::tick();
            crate::shell::ensure_hidden();
            // Position in Windows geändert, ohne dass eine Meldung kam?
            if crate::display::reload() {
                relocate();
            }
            ensure_work_area(hwnd);
            fit_maximized(hwnd);
            tick_secondaries();
            // Vor Explorers Shell_TrayWnd bleiben, damit neue Tray-Icons zu uns kommen.
            // Nur wenn nötig (Explorer liegt vorne), nie bei offenem Menü und nie
            // über einer Vollbild-App – sonst verdeckt die Leiste Menüs, Popups, Spiele.
            let first = FindWindowW(w16("Shell_TrayWnd").as_ptr(), null());
            let fullscreen = with(|s| s.fullscreen).unwrap_or(false);
            if first != hwnd && !menus::menu_open() && !fullscreen {
                SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                crate::glow::raise();
            }
            sync_glow(hwnd);
            dump_profile();
            0
        }
        // Wächter fragt, ob die Leiste noch Nachrichten verarbeitet.
        crate::watchdog::WM_BAR_PING => {
            crate::watchdog::pong();
            0
        }
        // Lautstärke, Netzwerk oder Akku haben sich geändert (Meldung aus `status`).
        WM_BAR_STATUS | WM_POWERBROADCAST => {
            prof(6);
            if msg == WM_BAR_STATUS {
                status::invalidate_network();
            }
            update_status(hwnd);
            if msg == WM_POWERBROADCAST {
                return 1;
            }
            0
        }
        // WinEvents: Vordergrundwechsel (Vollbild, aktive App) und
        // Ein-/Ausblenden von Fenstern beim Wechsel virtueller Desktops.
        WM_BAR_EVENT => {
            prof(1);
            match wp as u32 {
                EVENT_SYSTEM_FOREGROUND => {
                    update_fullscreen(hwnd);
                    ensure_work_area(hwnd);
                    tick_secondaries();
                    kick(hwnd);
                }
                _ => {
                    SetTimer(hwnd, TIMER_REFRESH, 150, None);
                }
            }
            0
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(lp);
            if drag_move(hwnd, x) {
                return 0;
            }
            let h = hit_at(hwnd, x, y);
            let (changed, start_tracking) = with(|s| {
                let changed = s.hover != h || s.hover_hwnd != hwnd;
                s.hover = h;
                s.hover_hwnd = hwnd;
                (changed, std::mem::replace(&mut s.tracking, hwnd) != hwnd)
            })
            .unwrap_or((false, false));
            if start_tracking {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tme);
            }
            if changed {
                kick(hwnd);
                sync_glow(hwnd);
                if let (Hit::Tray(i), Some(r)) = (h, rect_of(hwnd, h)) {
                    let mut pt = POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
                    ClientToScreen(hwnd, &mut pt);
                    tray::hover(i, pt);
                }
                crate::tooltip::hide();
                let window = window_at(h);
                if window.is_some() && crate::preview::visible() {
                    // Zwischen Buttons wandert die Vorschau sofort mit.
                    show_tip(hwnd);
                } else {
                    if window.is_none() {
                        crate::preview::schedule_hide();
                    }
                    if h != Hit::None {
                        SetTimer(hwnd, TIMER_TIP, 450, None);
                    }
                }
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_MOUSELEAVE => {
            KillTimer(hwnd, TIMER_TIP);
            crate::tooltip::hide();
            crate::preview::schedule_hide();
            with(|s| {
                // Nur die eigene Leiste zurücksetzen – die Maus kann schon auf der anderen sein.
                if s.hover_hwnd == hwnd {
                    s.hover = Hit::None;
                }
                if s.tracking == hwnd {
                    s.tracking = null_mut();
                }
            });
            kick(hwnd);
            crate::glow::set_hover(None);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            ANCHOR.store(hwnd, Ordering::Relaxed);
            KillTimer(hwnd, TIMER_TIP);
            crate::tooltip::hide();
            crate::preview::hide();
            // Der Pfeil schaltet den Überlauf selbst um; jeder andere Klick schließt ihn.
            let (x, y) = xy(lp);
            let hit = hit_at(hwnd, x, y);
            if hit != Hit::Chevron {
                tray::hide_overflow();
            }
            if let (WM_LBUTTONDOWN, Hit::Task(i)) = (msg, hit) {
                // Möglicher Beginn eines Ziehens; zum Ziehen wird es erst ab ein paar Pixeln.
                with(|s| s.drag = s.groups.get(i).map(|g| Drag { slot: g.slot.clone(), start_x: x, x, active: false }));
                SetCapture(hwnd);
            }
            if msg == WM_LBUTTONDOWN && matches!(hit, Hit::Task(_)) {
                with(|s| s.anim.press = hit_id(hit, &s.groups).map(|id| (id, Instant::now())));
                kick(hwnd);
            }
            0
        }
        WM_CAPTURECHANGED => {
            with(|s| s.drag = None);
            0
        }
        WM_LBUTTONUP => {
            if drag_end(hwnd) {
                return 0; // Gezogen, nicht geklickt.
            }
            let (x, y) = xy(lp);
            match hit_at(hwnd, x, y) {
                Hit::Chevron => {
                    if let Some(r) = rect_of(hwnd, Hit::Chevron) {
                        tray::toggle_overflow(element_anchor(hwnd, r), bar_dpi(hwnd));
                    }
                }
                Hit::Start => crate::start::toggle(false),
                Hit::Clock => crate::calendar::toggle(),
                Hit::Status => crate::quick::toggle(),
                Hit::Usage => {
                    let h = hwnd as isize;
                    crate::usage::refresh_now(move || {
                        PostMessageW(h as HWND, WM_BAR_USAGE, 0, 0);
                    });
                }
                Hit::TaskMgr => shell_open("taskmgr.exe", "", SW_SHOWNORMAL),
                Hit::Desktop => send_keys(&[(VK_LWIN, false), (VK_KEY_D, false), (VK_KEY_D, true), (VK_LWIN, true)]),
                Hit::Tray(i) => tray_click(hwnd, i, Click::Left),
                Hit::Task(i) => {
                    if let Some(g) = with(|s| s.groups.get(i).cloned()).flatten() {
                        click_group(&g);
                    }
                }
                Hit::More => more_menu(hwnd),
                Hit::None => {}
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp >> 16) & 0xffff) as i16 as i32;
            let mut pt = POINT { x: (lp & 0xffff) as i16 as i32, y: ((lp >> 16) & 0xffff) as i16 as i32 };
            ScreenToClient(hwnd, &mut pt);
            match hit_at(hwnd, pt.x, pt.y) {
                Hit::Status => wheel_volume(hwnd, delta),
                Hit::Task(i) => {
                    if let Some(g) = with(|s| s.groups.get(i).cloned()).flatten() {
                        cycle_group(&g, delta < 0);
                    }
                }
                _ => {}
            }
            0
        }
        // Akzentfarbe geändert (Personalisierung) → neu einlesen.
        0x0320 | WM_SETTINGCHANGE => {
            reload_accent();
            // Taskleisten-Einstellungen von Windows (Ausrichtung, alle Bildschirme)
            // gelten sofort – nicht bei unserer eigenen Arbeitsbereich-Meldung.
            // Arbeitsbereich von jemand anderem geändert (meist Explorer): kurz danach
            // prüfen, ob unsere Leisten noch Platz haben.
            if msg == WM_SETTINGCHANGE && hwnd == self::hwnd() && wp as u32 == SPI_SETWORKAREA {
                SetTimer(hwnd, TIMER_WORKAREA, 200, None);
            }
            if msg == WM_SETTINGCHANGE && hwnd == self::hwnd() && wp as u32 != SPI_SETWORKAREA {
                apply_taskbar_settings();
            }
            InvalidateRect(hwnd, null(), 0);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = xy(lp);
            if let Hit::Tray(i) = hit_at(hwnd, x, y) {
                tray_click(hwnd, i, Click::Double);
            }
            0
        }
        WM_MBUTTONUP => {
            // Mittelklick: neue Instanz, wie bei Windows.
            let (x, y) = xy(lp);
            if let Hit::Task(i) = hit_at(hwnd, x, y) {
                if let Some(g) = with(|s| s.groups.get(i).cloned()).flatten() {
                    launch(&g);
                }
            }
            0
        }
        WM_RBUTTONUP => {
            let (x, y) = xy(lp);
            match hit_at(hwnd, x, y) {
                Hit::Start => menus::winx(hwnd),
                Hit::Task(i) => {
                    if let Some(g) = with(|s| s.groups.get(i).cloned()).flatten() {
                        task_menu(hwnd, g);
                    }
                }
                Hit::Tray(i) => tray_click(hwnd, i, Click::Right),
                Hit::Chevron => bar_menu(hwnd),
                Hit::More => more_menu(hwnd),
                Hit::Status => status_menu(hwnd),
                Hit::Clock => clock_menu(hwnd),
                Hit::Usage => usage_menu(hwnd),
                Hit::Desktop | Hit::TaskMgr | Hit::None => bar_menu(hwnd),
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_APPBAR => {
            match wp as u32 {
                ABN_POSCHANGED => set_pos(hwnd),
                // Der Vollbild-Merker gehört der Hauptleiste; Zweitleisten prüfen sich selbst.
                ABN_FULLSCREENAPP if hwnd == self::hwnd() => update_fullscreen(hwnd),
                ABN_FULLSCREENAPP => tick_secondaries(),
                _ => {}
            }
            0
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            set_pos(hwnd);
            if hwnd == self::hwnd() {
                refresh(hwnd, None);
                // Monitor an- oder abgesteckt: Zweitleisten anpassen.
                sync_monitors();
            }
            0
        }
        // Abmelden/Herunterfahren: Explorers Originalzustand (kein "automatisch
        // ausblenden") zurück. Der überlebt sonst den Neustart – und startet
        // Lightbar danach nicht, wäre die Windows-Leiste versteckt.
        WM_ENDSESSION if hwnd == self::hwnd() && wp != 0 => {
            crate::util::log("Sitzung endet – Windows-Taskleiste wiederhergestellt");
            crate::shell::restore();
            0
        }
        // Von außen geschlossen (z. B. --install, Installer, Restart Manager):
        // festhalten, damit sich ein unerwartetes Ende nachvollziehen lässt.
        WM_CLOSE if hwnd == self::hwnd() => {
            crate::util::log("WM_CLOSE von außen erhalten");
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY if hwnd != self::hwnd() => {
            // Zweitleiste: nur ihren Platz freigeben.
            let mut abd = appbar_data(hwnd);
            SHAppBarMessage(ABM_REMOVE, &mut abd);
            release_work_area(hwnd);
            // Lag die Zweitleiste zuletzt auf dem Hauptbildschirm (Monitor abgesteckt),
            // braucht die Hauptleiste ihren Platz jetzt wieder selbst.
            ensure_work_area(self::hwnd());
            if let Ok(mut v) = SECONDARY.lock() {
                v.retain(|&h| h != hwnd as isize);
            }
            0
        }
        WM_DESTROY => {
            crate::util::log("Beendet");
            // Zweitleisten zuerst schließen (Liste vorher leeren: ihr WM_DESTROY räumt sonst mit).
            let others = secondaries();
            if let Ok(mut v) = SECONDARY.lock() {
                v.clear();
            }
            for h in others {
                DestroyWindow(h);
            }
            // Vor ABM_REMOVE: Wiederherstellen läuft noch über unsere Weiterleitung.
            crate::shell::restore();
            KillTimer(hwnd, TIMER_TICK);
            DeregisterShellHookWindow(hwnd);
            let mut abd = appbar_data(hwnd);
            SHAppBarMessage(ABM_REMOVE, &mut abd);
            with(|s| {
                DeleteObject(s.font);
                DeleteObject(s.icons);
            });
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Leiste anlegen, als AppBar registrieren und anzeigen.
pub unsafe fn create(inst: HINSTANCE) -> HWND {
    let class = w16("Shell_TrayWnd");
    register_class(inst, &class, Some(wndproc), CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        class.as_ptr(),
        w16("Rust Taskbar").as_ptr(),
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
        return hwnd;
    }
    BAR_HWND.store(hwnd, Ordering::Relaxed);
    let shell_msg = RegisterWindowMessageW(w16("SHELLHOOK").as_ptr());
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            dpi: 96,
            icon_px: 0,
            shell_msg,
            created_msg: RegisterWindowMessageW(w16("TaskbarCreated").as_ptr()),
            groups: Vec::new(),
            info: HashMap::new(),
            pins: load_pins(),
            order: Vec::new(),
            drag: None,
            kb_focus: None,
            status: Some(status::poll()),
            hover: Hit::None,
            hover_hwnd: null_mut(),
            tracking: null_mut(),
            fullscreen: false,
            anim: Anim::default(),
            cpu: 0.0,
            ram: 0.0,
            cpu_times: (0, 0),
            taskmgr_icon: null_mut(),
            attention: Vec::new(),
            start: Instant::now(),
            font: null_mut(),
            icons: null_mut(),
        })
    });
    let mut abd = appbar_data(hwnd);
    SHAppBarMessage(ABM_NEW, &mut abd);
    set_pos(hwnd);
    RegisterShellHookWindow(hwnd);
    schedule_clock(hwnd);
    SetTimer(hwnd, TIMER_CPU, 2000, None);
    SetTimer(hwnd, TIMER_SLOW, 30_000, None);
    status::watch(hwnd);
    install_win_events();
    refresh(hwnd, None);
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    // set_pos lief noch unsichtbar – jetzt den Arbeitsbereich prüfen.
    ensure_work_area(hwnd);
    tray::announce();
    sync_monitors();
    let notify_target = hwnd as isize;
    crate::usage::spawn(move || {
        PostMessageW(notify_target as HWND, WM_BAR_USAGE, 0, 0);
    });
    hwnd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn non_empty(r: &RECT) -> bool {
        r.right > r.left
    }

    fn overlap(a: &RECT, b: &RECT) -> bool {
        non_empty(a) && non_empty(b) && a.left < b.right && b.left < a.right
    }

    /// Breiten in physischen Pixeln (auch hochkant), gängige Skalierungen,
    /// wenige bis sehr viele Apps und Tray-Icons: nichts überlappt, nichts
    /// ragt aus der Leiste, Buttons bleiben bedienbar breit.
    #[test]
    fn layout_fits_every_resolution() {
        let mut dropped = Vec::new();
        for width in [720, 768, 800, 1024, 1080, 1200, 1280, 1366, 1440, 1600, 1920, 2160, 2560, 3440, 3840] {
            for dpi in [96, 120, 144, 168, 192] {
                for n_tasks in [0, 3, 8, 15, 25, 40] {
                    for n_tray in [0, 4, 12] {
                        for (primary, usage, align_left, hidden_tray) in [(true, true, false, true), (true, false, true, false), (true, true, true, false), (false, false, false, false), (false, false, true, false)] {
                            let i = LayoutInput { width, vertical: false, length: px(dpi, BAR_H), dpi, n_tasks, n_tray, hidden_tray, battery: true, primary, usage, align_left };
                            let l = layout(&i);
                            let ctx = format!("w={width} dpi={dpi} tasks={n_tasks} tray={n_tray} primary={primary} usage={usage} left={align_left}");
                            let mut all: Vec<(&str, RECT)> = vec![("start", l.start), ("status", l.status), ("clock", l.clock), ("taskmgr", l.taskmgr)];
                            if l.show_usage {
                                all.push(("usage", l.usage));
                            }
                            all.extend(l.tasks.iter().map(|r| ("task", *r)));
                            all.extend(l.tray.iter().map(|r| ("tray", *r)));
                            all.extend(l.chevron.iter().map(|r| ("chevron", *r)));
                            all.extend(l.more.iter().map(|r| ("more", *r)));
                            if l.tasks.len() < n_tasks {
                                assert!(l.more.is_some(), "Apps ohne Platz und ohne …-Knopf: {ctx}");
                            }
                            for (name, r) in &all {
                                if non_empty(r) {
                                    assert!(r.left >= 0 && r.right <= width, "{name} außerhalb: {ctx}");
                                }
                            }
                            for a in 0..all.len() {
                                for b in a + 1..all.len() {
                                    assert!(!overlap(&all[a].1, &all[b].1), "{} überlappt {}: {ctx}", all[a].0, all[b].0);
                                }
                            }
                            for r in &l.tasks {
                                assert!(r.right - r.left >= px(dpi, BTN_MIN_W), "Button zu schmal: {ctx}");
                            }
                            if !primary {
                                assert!(l.tray.is_empty() && !l.show_usage && !non_empty(&l.taskmgr), "Zweitleiste mit Hauptleisten-Elementen: {ctx}");
                            }
                            if l.tasks.len() < n_tasks {
                                dropped.push(ctx);
                            }
                        }
                    }
                }
            }
        }
        // Zur Einordnung: wo nicht alle Buttons Platz haben.
        let tight: Vec<&String> = dropped.iter().filter(|c| !c.contains("tasks=40") && !c.contains("tasks=25")).collect();
        println!("{} Fälle mit weggelassenen Buttons, davon {} mit höchstens 15 Apps:", dropped.len(), tight.len());
        for c in tight.iter().filter(|c| c.starts_with("w=1080 ") || c.starts_with("w=1366 ")) {
            println!("  {c}");
        }
    }
}

#[cfg(test)]
mod vertical_tests {
    use super::*;

    fn overlap(a: &RECT, b: &RECT) -> bool {
        a.right > a.left && b.right > b.left && a.bottom > a.top && b.bottom > b.top && a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
    }

    /// Senkrechte Leiste (links/rechts) bei vielen Bildschirmhöhen und
    /// Skalierungen: nichts überlappt, alles bleibt in der Leiste, Apps ohne
    /// Platz landen hinter dem "…"-Knopf.
    #[test]
    fn vertical_layout_fits() {
        for length in [600, 768, 900, 1080, 1200, 1440, 1600, 1920, 2160] {
            for dpi in [96, 120, 144, 192] {
                let width = px(dpi, BAR_W_V);
                for n_tasks in [0, 5, 12, 25, 40] {
                    for n_tray in [0, 3, 10] {
                        for (primary, align_left, hidden_tray) in [(true, false, true), (true, true, false), (false, false, false)] {
                            let i = LayoutInput { width, vertical: true, length, dpi, n_tasks, n_tray, hidden_tray, battery: true, primary, usage: true, align_left };
                            let l = layout(&i);
                            let ctx = format!("len={length} dpi={dpi} tasks={n_tasks} tray={n_tray} primary={primary} top={align_left}");
                            let mut all: Vec<(&str, RECT)> = vec![("start", l.start), ("status", l.status), ("clock", l.clock), ("taskmgr", l.taskmgr), ("desktop", l.desktop)];
                            all.extend(l.tasks.iter().map(|r| ("task", *r)));
                            all.extend(l.tray.iter().map(|r| ("tray", *r)));
                            all.extend(l.chevron.iter().map(|r| ("chevron", *r)));
                            all.extend(l.more.iter().map(|r| ("more", *r)));
                            assert!(!l.show_usage, "Usage in senkrechter Leiste: {ctx}");
                            for (name, r) in &all {
                                if r.right > r.left && r.bottom > r.top {
                                    assert!(r.left >= 0 && r.right <= width && r.top >= 0 && r.bottom <= length, "{name} außerhalb: {ctx}");
                                }
                            }
                            for a in 0..all.len() {
                                for b in a + 1..all.len() {
                                    assert!(!overlap(&all[a].1, &all[b].1), "{} überlappt {}: {ctx}", all[a].0, all[b].0);
                                }
                            }
                            if l.tasks.len() < n_tasks {
                                assert!(l.more.is_some(), "Apps ohne Platz und ohne …-Knopf: {ctx}");
                            }
                            if !primary {
                                assert!(l.tray.is_empty() && l.chevron.is_none(), "Zweitleiste mit Tray: {ctx}");
                            }
                        }
                    }
                }
            }
        }
    }
}
