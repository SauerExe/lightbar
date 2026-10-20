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
use std::sync::atomic::{AtomicPtr, Ordering};
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
pub const WM_BAR_WINX: u32 = WM_APP + 30;
pub const WM_BAR_NUM: u32 = WM_APP + 31;
pub const WM_BAR_QUICK: u32 = WM_APP + 32;
const WM_BAR_USAGE: u32 = WM_APP + 33;
const WM_BAR_INVOKE: u32 = WM_APP + 34;
pub const WM_BAR_FOCUS: u32 = WM_APP + 35;
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
    tracking: bool,
    /// Eine Vollbild-App ist vorne: Leiste ausgeblendet.
    fullscreen: bool,
    anim: Anim,
    /// Sekundenzähler für seltenere Aufgaben.
    tick: u32,
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

struct Layout {
    usage: RECT,
    taskmgr: RECT,
    start: RECT,
    tasks: Vec<RECT>,
    tray: Vec<RECT>,
    chevron: Option<RECT>,
    status: RECT,
    clock: RECT,
    desktop: RECT,
}

fn layout(width: i32, dpi: u32, n_tasks: usize, n_tray: usize, overflow: bool, battery: bool) -> Layout {
    let (bw, gap) = (px(dpi, BTN_W), px(dpi, BTN_GAP));
    let h = px(dpi, BAR_H);
    let top = (h - px(dpi, BTN_H)) / 2;
    let bottom = top + px(dpi, BTN_H);
    let count = n_tasks as i32 + 1;
    let mut x = (width - (count * bw + (count - 1) * gap)) / 2;
    let mut next = || {
        let r = RECT { left: x, top, right: x + bw, bottom };
        x += bw + gap;
        r
    };
    let start = next();
    let tasks = (0..n_tasks).map(|_| next()).collect();

    // Rechte Seite von außen nach innen.
    let desktop = RECT { left: width - px(dpi, DESKTOP_W), top: 0, right: width, bottom: h };
    let clock_r = desktop.left - px(dpi, 4);
    let clock = RECT { left: clock_r - px(dpi, CLOCK_W), top, right: clock_r, bottom };
    let glyphs = 2 + battery as i32;
    let status_r = clock.left - px(dpi, 4);
    let status = RECT { left: status_r - glyphs * px(dpi, STATUS_GLYPH_W) - px(dpi, 16), top, right: status_r, bottom };
    let tw = px(dpi, TRAY_W);
    let tray_r = status.left - px(dpi, 4);
    let tray_l = tray_r - n_tray as i32 * tw;
    let tray = (0..n_tray as i32)
        .map(|i| RECT { left: tray_l + i * tw, top, right: tray_l + (i + 1) * tw, bottom })
        .collect();
    let chevron = overflow.then(|| RECT { left: tray_l - tw, top, right: tray_l, bottom });
    let usage = RECT { left: px(dpi, 8), top, right: px(dpi, 8 + USAGE_W), bottom };
    // Task-Manager direkt rechts neben der Usage-Anzeige (oder ganz links, wenn die aus ist).
    let tm_left = if { let s = crate::settings::get(); s.show_usage && !s.presentation } { usage.right + px(dpi, 4) } else { px(dpi, 8) };
    let taskmgr = RECT { left: tm_left, top, right: tm_left + px(dpi, TASKMGR_W), bottom };
    Layout { usage, taskmgr, start, tasks, tray, chevron, status, clock, desktop }
}

unsafe fn current_layout(hwnd: HWND) -> Option<(Layout, Vec<(usize, HICON)>)> {
    let (dpi, n, battery) = with(|s| (s.dpi, s.groups.len(), s.status.as_ref().is_some_and(|x| x.battery.is_some())))?;
    let tray = tray::visible();
    let overflow = !tray::overflow().is_empty();
    let mut rc: RECT = zeroed();
    GetClientRect(hwnd, &mut rc);
    Some((layout(rc.right, dpi, n, tray.len(), overflow, battery), tray))
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
    if inside(&l.usage, x, y) && crate::settings::get().show_usage && !crate::settings::get().presentation {
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
        kick(hwnd);
        InvalidateRect(hwnd, null(), 0);
    }
}

// ---------------------------------------------------------------- Aktionen

unsafe fn activate(h: HWND) {
    if IsIconic(h) != 0 {
        ShowWindow(h, SW_RESTORE);
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
                ShowWindow(h, SW_MINIMIZE);
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
    let mut items = Vec::new();
    if !recent.is_empty() {
        items.push(Item::Label("Zuletzt verwendet"));
        for (i, label) in recent_labels.iter().enumerate() {
            items.push(Item::Cmd(100 + i, label));
        }
        items.push(Item::Sep);
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
        3 => {
            for &w in &g.windows {
                PostMessageW(w, WM_CLOSE, 0, 0);
            }
        }
        _ => {}
    }
}

unsafe fn bar_menu(hwnd: HWND) {
    use crate::settings::GlowStyle;
    let s = crate::settings::get();
    let glow = |id, label, style| Item::Check(id, label, s.glow == style);
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
            Item::Sub("Startsymbol", vec![Item::Check(16, "Orb", !s.windows_logo), Item::Check(17, "Windows-Logo", s.windows_logo)]),
            Item::Check(14, "Uhr mit Sekunden", s.clock_seconds),
            Item::Check(15, "Claude/Codex-Usage anzeigen", s.show_usage),
            Item::Check(5, "Tray-Icons einfarbig", s.mono_tray),
            Item::Check(18, "Präsentationsmodus", s.presentation),
            Item::Sep,
            Item::Cmd(3, "Taskleiste beenden"),
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
        14 => crate::settings::update(|s| s.clock_seconds = !s.clock_seconds),
        16 => crate::settings::update(|s| s.windows_logo = false),
        17 => crate::settings::update(|s| s.windows_logo = true),
        15 => crate::settings::update(|s| s.show_usage = !s.show_usage),
        5 => crate::settings::update(|s| s.mono_tray = !s.mono_tray),
        18 => crate::settings::update(|s| s.presentation = !s.presentation),
        3 => {
            DestroyWindow(hwnd);
            return;
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
            format!("{}\n\n{}\n\nKlicken zum Aktualisieren", crate::usage::describe("Claude", &u.claude, u.claude_at), crate::usage::describe("Codex", &u.codex, u.codex_at))
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

/// Nach kurzer Verweildauer: Fenster-Vorschau für offene Apps, sonst Tooltip.
unsafe fn show_tip(hwnd: HWND) {
    let Some((hover, dpi)) = with(|s| (s.hover, s.dpi)) else { return };
    let Some(r) = rect_of(hwnd, hover) else { return };
    let mut pt = POINT { x: (r.left + r.right) / 2, y: 0 };
    ClientToScreen(hwnd, &mut pt);
    if let Some((targets, icon)) = window_at(hover) {
        crate::preview::show(&targets, icon, pt.x, pt.y, dpi);
    } else {
        crate::tooltip::show(&tip_text(hover), pt.x, pt.y, dpi);
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
        let mut pt = POINT { x: (r.left + r.right) / 2, y: 0 };
        ClientToScreen(hwnd, &mut pt);
        let dpi = with(|s| s.dpi).unwrap_or(96);
        crate::tooltip::show(&label, pt.x, pt.y, dpi);
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

/// Win+1…9: n-ten Button aktivieren oder starten.
unsafe fn activate_nth(n: usize) {
    if let Some(g) = with(|s| s.groups.get(n).cloned()).flatten() {
        click_group(&g);
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
        uEdge: ABE_BOTTOM,
        rc: zeroed(),
        lParam: 0,
    }
}

/// Platz am unteren Rand reservieren und Fenster dorthin legen.
unsafe fn set_pos(hwnd: HWND) {
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
    let h = px(dpi, BAR_H);
    let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
    let mut abd = appbar_data(hwnd);
    abd.rc = RECT { left: 0, top: sh - h, right: sw, bottom: sh };
    SHAppBarMessage(ABM_QUERYPOS, &mut abd);
    abd.rc.top = abd.rc.bottom - h;
    SHAppBarMessage(ABM_SETPOS, &mut abd);
    MoveWindow(hwnd, abd.rc.left, abd.rc.top, abd.rc.right - abd.rc.left, h, 1);
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
        let settings = crate::settings::get();
        let groups = with(|s| s.groups.clone()).unwrap_or_default();
        let fg = GetForegroundWindow();
        let mut out = Vec::new();
        let mut push = |hit: Hit, id: String, name: String, help: &str, rect: RECT| {
            out.push(UiElement { id, name, help: help.to_string(), rect: screen(rect), hit });
        };
        if settings.show_usage && !settings.presentation {
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
        VK_RIGHT | VK_TAB if !shift => ui_set_focus((cur + 1) % n.max(1)),
        VK_LEFT | VK_TAB => ui_set_focus((cur + n.max(1) - 1) % n.max(1)),
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
    SetTimer(hwnd, TIMER_ANIM, 16, None);
}

/// Während des Ziehens: Button folgt der Maus, die anderen weichen aus.
/// 	rue, wenn gerade gezogen wird (dann kein normales Hover).
unsafe fn drag_move(hwnd: HWND, x: i32) -> bool {
    let Some((l, _)) = current_layout(hwnd) else { return false };
    let moving = with(|s| {
        let d = s.dpi;
        let drag = s.drag.as_mut()?;
        drag.x = x;
        if !drag.active {
            if (x - drag.start_x).abs() < px(d, 6) {
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
/// 	rue, wenn wirklich gezogen wurde (dann ist es kein Klick).
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
    let fg = GetForegroundWindow();
    let mut bar_origin = POINT { x: 0, y: 0 };
    ClientToScreen(hwnd, &mut bar_origin);
    let (moving, new_buttons) = with(|s| {
        let d = s.dpi;
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
            approach(s.anim.x.entry(id.clone()).or_insert(r.left as f32), r.left as f32, 0.22, 0.5, &mut moving);
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
        // Aufmerksamkeit pulsiert dauerhaft.
        if !s.attention.is_empty() {
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
    let Some((l, _)) = current_layout(hwnd) else { return };
    let mut origin = POINT { x: 0, y: 0 };
    ClientToScreen(hwnd, &mut origin);
    let hover_hit = with(|s| s.hover).unwrap_or(Hit::None);
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

/// Maximierte Fenster, die noch nach dem alten Arbeitsbereich ausgerichtet
/// sind (z. B. maximiert, bevor unsere Leiste Platz reserviert hat), ragen
/// unter die Leiste. Windows passt sie nicht von selbst an – also kürzen.
pub unsafe fn fit_maximized(bar: HWND) {
    let mut work: RECT = zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut _ as _, 0);
    let mut windows: Vec<HWND> = Vec::new();
    EnumWindows(Some(enum_cb), &mut windows as *mut _ as LPARAM);
    for h in windows {
        if IsZoomed(h) == 0 || !same_monitor(h, bar) {
            continue;
        }
        let mut r: RECT = zeroed();
        GetWindowRect(h, &mut r);
        // Maximierte Fenster mit Rahmen stehen ringsum um die Rahmenbreite über.
        let border = (work.left - r.left).max(0);
        let bottom = work.bottom + border;
        if r.bottom > bottom + 1 {
            SetWindowPos(h, null_mut(), r.left, r.top, r.right - r.left, bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
        }
    }
}

/// Deckt das Vordergrundfenster den ganzen Monitor ab (Spiel, Video, F11)?
/// Maximierte Fenster zählen nicht – die lassen die Leiste frei.
unsafe fn foreground_fullscreen(bar: HWND) -> bool {
    let fg = GetForegroundWindow();
    if fg.is_null() || IsZoomed(fg) != 0 || !same_monitor(fg, bar) {
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
    } else {
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
    crate::glow::sync();
}

// ---------------------------------------------------------- Usage-Anzeige

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
    let mid = (r.top + r.bottom) / 2;
    let line = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS;
    for (i, (name, res)) in [("Claude", &u.claude), ("Codex", &u.codex)].into_iter().enumerate() {
        let row = RECT {
            left: r.left + px(dpi, 8),
            right: r.right - px(dpi, 8),
            top: if i == 0 { r.top + px(dpi, 1) } else { mid },
            bottom: if i == 0 { mid } else { r.bottom - px(dpi, 1) },
        };
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
            Some(Err(_)) => text(dc, "nicht verfügbar", &rest, font, DIM, line),
            None => text(dc, "…", &rest, font, DIM, line),
        }
    }
}

/// Startsymbol: standardmäßig ein eigener Orb (Akzentfarbe, versetzter dunkler
/// Kern, Glanzpunkt); auf Wunsch das Windows-Logo (Marke von Microsoft,
/// deshalb nicht voreingestellt).
unsafe fn draw_logo(dc: HDC, r: &RECT, dpi: u32) {
    if crate::settings::get().windows_logo {
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

unsafe fn paint(hwnd: HWND) {
    let snapshot = with(|s| {
        let groups: Vec<(String, Vec<HWND>, HICON, bool)> = s
            .groups
            .iter()
            .map(|g| (format!("g:{}", g.key), g.windows.clone(), g.icon, g.windows.iter().any(|w| s.attention.contains(w))))
            .collect();
        let dragged = s.drag.as_ref().filter(|d| d.active).and_then(|d| s.groups.iter().find(|g| g.slot == d.slot).map(|g| (format!("g:{}", g.key), d.x)));
        (s.dpi, s.font, s.icons, s.status.clone(), groups, s.anim.clone(), s.start.elapsed().as_secs_f32(), s.taskmgr_icon, s.cpu, dragged)
    });
    let Some((dpi, font, icons, status, groups, anim, t, taskmgr_icon, cpu, dragged)) = snapshot else {
        let mut ps: PAINTSTRUCT = zeroed();
        BeginPaint(hwnd, &mut ps);
        EndPaint(hwnd, &ps);
        return;
    };
    let hv = |id: &str| anim.hover.get(id).copied().unwrap_or(0.0);
    let tray = tray::visible();
    let overflow = !tray::overflow().is_empty();
    let focus = focus_rect(hwnd);
    let settings = crate::settings::get();
    let mono_on = settings.mono_tray;
    paint_buffered(hwnd, |dc, w, h| {
        let rc = RECT { left: 0, top: 0, right: w, bottom: h };
        fill(dc, &rc, BG);
        fill(dc, &RECT { left: 0, top: 0, right: w, bottom: px(dpi, 1).max(1) }, BORDER);

        let battery = status.as_ref().and_then(|s| s.battery);
        let l = layout(w, dpi, groups.len(), tray.len(), overflow, battery.is_some());
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
            let dx = anim.x.get(id).map(|x| x.round() as i32 - base.left).unwrap_or(0);
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
        if let Some(s) = &status {
            let gw = px(dpi, STATUS_GLYPH_W);
            let mut gx = l.status.left + px(dpi, 8);
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
        hover_bg(dc, &l.clock, k);
        let mut st: SYSTEMTIME = zeroed();
        GetLocalTime(&mut st);
        let mid = (l.clock.top + l.clock.bottom) / 2;
        let flags = DT_RIGHT | DT_SINGLELINE;
        let cr = RECT { right: l.clock.right - px(dpi, 10), ..l.clock };
        let time = if settings.clock_seconds {
            format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
        } else {
            format!("{:02}:{:02}", st.wHour, st.wMinute)
        };
        text(dc, &time, &RECT { bottom: mid, ..cr }, font, TEXT, flags | DT_BOTTOM);
        text(dc, &format!("{:02}.{:02}.{}", st.wDay, st.wMonth, st.wYear), &RECT { top: mid, ..cr }, font, TEXT, flags | DT_TOP);

        if settings.show_usage && !settings.presentation {
            draw_usage(dc, &l.usage, dpi, font, hv("usage"));
        }
        draw_taskmgr(dc, &l.taskmgr, dpi, hv("taskmgr"), taskmgr_icon, cpu);

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
        return 0;
    }
    if shell_msg != 0 && msg == shell_msg {
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
        let code = (wp & 0x7fff) as u32;
        let refetch = (code == HSHELL_REDRAW || code == HSHELL_WINDOWREPLACED).then_some(lp as HWND);
        refresh(hwnd, refetch);
        // Aktivierungswechsel: Vollbild sofort erkennen, nicht erst im Sekundentakt.
        update_fullscreen(hwnd);
        sync_glow(hwnd);
        kick(hwnd);
        return 0;
    }
    match msg {
        WM_COPYDATA => {
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
            paint(hwnd);
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
        WM_GETOBJECT if lp as i32 == windows::Win32::UI::Accessibility::UiaRootObjectId => {
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
        WM_TIMER => {
            let tick = with(|s| {
                s.tick = s.tick.wrapping_add(1);
                s.tick
            })
            .unwrap_or(0);
            // Jede Sekunde nur das Billige: Last messen, Vollbild prüfen, Status
            // (Audio/WLAN sind in `status` gecacht) – und nur Uhr, Last-Balken
            // und Status-Symbole neu zeichnen statt der ganzen Leiste.
            update_fullscreen(hwnd);
            measure_load();
            let s = status::poll();
            let status_changed = with(|st| {
                let changed = st.status.as_ref() != Some(&s);
                st.status = Some(s);
                changed
            })
            .unwrap_or(false);
            if let Some((l, _)) = current_layout(hwnd) {
                for r in [l.clock, l.taskmgr] {
                    InvalidateRect(hwnd, &r, 0);
                }
                if status_changed {
                    InvalidateRect(hwnd, &l.status, 0);
                }
            }
            // Alle 3 s der Rest. Neue/geschlossene Fenster meldet der Shell-Hook
            // sofort; das hier fängt nur ab, was er nicht meldet (Desktop-Wechsel).
            if tick % 3 == 0 {
                refresh(hwnd, None);
                tray::tick();
                crate::shell::ensure_hidden();
                fit_maximized(hwnd);
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
            }
            if with(|s| !s.attention.is_empty()).unwrap_or(false) {
                kick(hwnd);
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
                let changed = s.hover != h;
                s.hover = h;
                (changed, !std::mem::replace(&mut s.tracking, true))
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
                s.hover = Hit::None;
                s.tracking = false;
            });
            kick(hwnd);
            crate::glow::set_hover(None);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
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
                        let mut pt = POINT { x: (r.left + r.right) / 2, y: 0 };
                        ClientToScreen(hwnd, &mut pt);
                        let dpi = with(|s| s.dpi).unwrap_or(96);
                        tray::toggle_overflow(pt.x, pt.y, dpi);
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
                Hit::Status => status_menu(hwnd),
                Hit::Clock => clock_menu(hwnd),
                Hit::Desktop | Hit::Usage | Hit::TaskMgr | Hit::None => bar_menu(hwnd),
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_APPBAR => {
            match wp as u32 {
                ABN_POSCHANGED => set_pos(hwnd),
                ABN_FULLSCREENAPP => update_fullscreen(hwnd),
                _ => {}
            }
            0
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            set_pos(hwnd);
            refresh(hwnd, None);
            0
        }
        WM_DESTROY => {
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
            tracking: false,
            fullscreen: false,
            anim: Anim::default(),
            tick: 0,
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
    SetTimer(hwnd, TIMER_TICK, 1000, None);
    refresh(hwnd, None);
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    tray::announce();
    let notify_target = hwnd as isize;
    crate::usage::spawn(move || {
        PostMessageW(notify_target as HWND, WM_BAR_USAGE, 0, 0);
    });
    hwnd
}
