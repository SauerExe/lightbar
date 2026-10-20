//! Startmenü: Suche (Apps, Einstellungen, zuletzt verwendete Dateien, Explorer, Web),
//! angeheftete Apps, Alle Apps, Energie-Menü.
//!
//! Gezeichnet wird pro Frame komplett neu; jede klickbare Fläche wird dabei in
//! `hits` eingetragen, damit Layout und Hit-Testing nie auseinanderlaufen.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::SystemTime;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::apps::{self, App};
use crate::menus::{self, Item};
use crate::util::*;

pub const WM_TOGGLE: u32 = WM_APP + 10;
const WM_RELOAD: u32 = WM_APP + 11;
const WM_FILES: u32 = WM_APP + 12;
const TIMER_FILES: usize = 4;

/// Ergebnis der letzten Index-Suche: (Generation, Anfrage, Treffer).
static FILE_RESULTS: std::sync::Mutex<Option<(u64, String, Vec<(String, String)>)>> = std::sync::Mutex::new(None);
const WM_MOUSELEAVE: u32 = 0x02A3;

// Logische Maße (96 DPI).
const W: i32 = 640;
const H: i32 = 720;
const PAD: i32 = 32;
const FOOT: i32 = 64;
const COLS: usize = 6;
const PIN_ROWS: usize = 3;
const ROW_H: i32 = 40;
const HEAD_H: i32 = 32;

static HWND_START: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

pub fn hwnd() -> HWND {
    HWND_START.load(Ordering::Relaxed)
}

#[derive(Clone)]
enum Target {
    App(usize),
    Path(String),
    Uri(String),
}

#[derive(Clone, Copy)]
enum Glyph {
    App,
    Settings,
    Web,
    Search,
    File,
}

#[derive(Clone)]
struct Entry {
    title: String,
    /// Rechtsbündiges Label (Kategorie) in Listen.
    kind: &'static str,
    /// Zweite Zeile, z. B. "vor 3 Std.".
    sub: String,
    target: Target,
    glyph: Glyph,
}

enum Row {
    Header(String),
    Item(Entry),
}

#[derive(Clone)]
enum Act {
    Open(Target),
    ShowAll,
    ShowHome,
    Power,
    Account,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Home,
    All,
}

#[derive(Clone, Copy)]
struct Fonts {
    normal: HFONT,
    small: HFONT,
    bold: HFONT,
    icons: HFONT,
    icons_small: HFONT,
    icons_big: HFONT,
}

impl Fonts {
    unsafe fn new(dpi: u32) -> Self {
        Fonts {
            normal: make_font(dpi, 13, FW_NORMAL),
            small: make_font(dpi, 12, FW_NORMAL),
            bold: make_font(dpi, 14, FW_SEMIBOLD),
            icons: icon_font(dpi, 16),
            icons_small: icon_font(dpi, 10),
            icons_big: icon_font(dpi, 24),
        }
    }
    unsafe fn delete(&self) {
        for f in [self.normal, self.small, self.bold, self.icons, self.icons_small, self.icons_big] {
            DeleteObject(f);
        }
    }
}

struct State {
    dpi: u32,
    fonts: Option<Fonts>,
    apps: Vec<App>,
    all_rows: Vec<Row>,
    pins: Vec<String>,
    pin_idx: Vec<usize>,
    recent: Vec<Entry>,
    icons: HashMap<String, HBITMAP>,
    query: String,
    results: Vec<Row>,
    /// exe (normalisiert) → App-Kennung im Katalog; für Sprunglisten, lazy gebaut.
    exe_ids: Option<HashMap<String, String>>,
    /// Treffer aus dem Windows-Suchindex für iles_for.
    files: Vec<Entry>,
    files_for: String,
    /// Laufende Nummer der Index-Suche; ältere Antworten werden verworfen.
    files_gen: u64,
    view: View,
    sel: usize,
    scroll: i32,
    max_scroll: i32,
    list_h: i32,
    mouse: (i32, i32),
    tracking: bool,
    hits: Vec<(RECT, Act)>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Zustand nur kurz und nie reentrant leihen; ist er belegt, passiert nichts.
fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| {
        let mut g = s.try_borrow_mut().ok()?;
        g.as_mut().map(f)
    })
}

// --------------------------------------------------------------- Daten

const SETTINGS: &[(&str, &str, &str)] = &[
    ("Einstellungen", "ms-settings:", "settings einstellungen"),
    ("Bluetooth & Geräte", "ms-settings:bluetooth", "bluetooth geräte maus tastatur kopfhörer"),
    ("WLAN", "ms-settings:network-wifi", "wlan wifi netzwerk internet"),
    ("Netzwerk & Internet", "ms-settings:network-status", "netzwerk internet ethernet lan"),
    ("VPN", "ms-settings:network-vpn", "vpn"),
    ("Anzeige", "ms-settings:display", "anzeige bildschirm monitor auflösung skalierung helligkeit"),
    ("Sound", "ms-settings:sound", "sound audio lautstärke lautsprecher mikrofon"),
    ("Benachrichtigungen", "ms-settings:notifications", "benachrichtigungen mitteilungen"),
    ("Energie & Akku", "ms-settings:powersleep", "energie akku batterie strom ruhezustand"),
    ("Speicher", "ms-settings:storagesense", "speicher festplatte platz"),
    ("Hintergrund", "ms-settings:personalization-background", "hintergrund wallpaper personalisierung"),
    ("Farben", "ms-settings:colors", "farben dunkel modus dark theme akzent"),
    ("Taskleiste", "ms-settings:taskbar", "taskleiste"),
    ("Installierte Apps", "ms-settings:appsfeatures", "apps programme deinstallieren"),
    ("Standard-Apps", "ms-settings:defaultapps", "standard apps browser"),
    ("Autostart", "ms-settings:startupapps", "autostart"),
    ("Konten", "ms-settings:yourinfo", "konto account"),
    ("Datum & Uhrzeit", "ms-settings:dateandtime", "datum uhrzeit zeit zone"),
    ("Sprache & Region", "ms-settings:regionlanguage", "sprache region"),
    ("Maus", "ms-settings:mousetouchpad", "maus touchpad zeiger"),
    ("Drucker & Scanner", "ms-settings:printers", "drucker scanner"),
    ("Windows Update", "ms-settings:windowsupdate", "update aktualisierung"),
    ("Datenschutz & Sicherheit", "ms-settings:privacy", "datenschutz sicherheit privacy"),
    ("Windows-Sicherheit", "windowsdefender:", "defender virus sicherheit"),
    ("Info", "ms-settings:about", "info system pc name version"),
    ("Gaming", "ms-settings:gaming-gamebar", "gaming spiele game bar"),
    ("Systemsteuerung", "shell:ControlPanelFolder", "systemsteuerung control panel"),
];

/// Startbelegung, solange der Nutzer nichts angeheftet hat.
const DEFAULT_PINS: &[&str] = &[
    "edge", "explorer", "einstellungen", "store", "fotos", "rechner", "editor", "terminal",
    "paint", "outlook", "word", "excel", "discord", "claude", "spotify", "steam", "visual studio code",
    "firefox", "chrome", "duckduckgo",
];

fn pins_path() -> Option<PathBuf> {
    Some(data_dir()?.join("pins.txt"))
}

fn load_pins(apps: &[App]) -> Vec<String> {
    if let Some(text) = pins_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        return text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    }
    let mut pins = Vec::new();
    for key in DEFAULT_PINS {
        let found = apps.iter().find(|a| {
            a.lower.contains(key) && !a.lower.contains("uninstall") && !a.lower.contains("deinstall")
        });
        if let Some(a) = found {
            if !pins.contains(&a.id) {
                pins.push(a.id.clone());
            }
        }
        if pins.len() >= COLS * PIN_ROWS {
            break;
        }
    }
    pins
}

fn save_pins(pins: &[String]) {
    if let Some(p) = pins_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, pins.join("\n"));
    }
}

fn ago(t: SystemTime) -> String {
    let s = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match s {
        0..=59 => "Gerade eben".into(),
        60..=3599 => format!("vor {} Min.", s / 60),
        3600..=86399 => format!("vor {} Std.", s / 3600),
        86400..=172799 => "Gestern".into(),
        _ => format!("vor {} Tagen", s / 86400),
    }
}

/// Zuletzt verwendete Dateien aus dem Recent-Ordner der Shell.
fn load_recent() -> Vec<Entry> {
    if crate::settings::get().presentation {
        return Vec::new();
    }
    let Ok(appdata) = std::env::var("APPDATA") else { return Vec::new() };
    let dir = Path::new(&appdata).join(r"Microsoft\Windows\Recent");
    let mut files: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let is_lnk = p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk"));
            // Protokoll-Aktivierungen (ms-gamingoverlay:…, ms-screenclip:…) sind keine Dateien.
            let stem = p.file_stem()?.to_string_lossy().to_lowercase();
            let is_protocol = stem.starts_with("ms-") || stem.contains("---");
            (is_lnk && !is_protocol).then_some(())?;
            Some((e.metadata().ok()?.modified().ok()?, p))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.truncate(60);
    files
        .into_iter()
        .map(|(t, p)| Entry {
            title: p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            kind: "Datei",
            sub: ago(t),
            target: Target::Path(p.to_string_lossy().into_owned()),
            glyph: Glyph::File,
        })
        .collect()
}

fn rebuild(st: &mut State) {
    let mut rows = Vec::with_capacity(st.apps.len() + 30);
    let mut last = None;
    for (i, a) in st.apps.iter().enumerate() {
        let c = a.name.chars().next().map(|c| c.to_uppercase().next().unwrap_or(c)).unwrap_or('#');
        let letter = if c.is_alphabetic() { c } else { '#' };
        if last != Some(letter) {
            rows.push(Row::Header(letter.to_string()));
            last = Some(letter);
        }
        rows.push(Row::Item(Entry {
            title: a.name.clone(),
            kind: "",
            sub: String::new(),
            target: Target::App(i),
            glyph: Glyph::App,
        }));
    }
    st.all_rows = rows;
    st.exe_ids = None;
    st.pin_idx = st.pins.iter().filter_map(|id| st.apps.iter().position(|a| &a.id == id)).collect();
}

/// App-Kennung (AppsFolder-ID) zu einer exe – unter dieser ID führt Windows
/// die Sprungliste von Desktop-Apps ohne eigene AppUserModelID.
pub fn app_id_for_exe(exe: &str) -> Option<String> {
    let want = crate::bar::normalize_exe(exe);
    with(|st| {
        if st.exe_ids.is_none() {
            let map = st
                .apps
                .iter()
                .filter_map(|a| unsafe { apps::target_path(a) }.map(|t| (crate::bar::normalize_exe(&t), a.id.clone())))
                .collect();
            st.exe_ids = Some(map);
        }
        st.exe_ids.as_ref()?.get(&want).cloned()
    })
    .flatten()
}

fn search(st: &State) -> Vec<Row> {
    let q = st.query.trim().to_lowercase();
    let score = |hay: &str| -> Option<u32> {
        if hay.starts_with(&q) {
            Some(0)
        } else if hay.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&q)) {
            Some(1)
        } else if hay.contains(&q) {
            Some(2)
        } else {
            None
        }
    };
    // Rang = Trefferqualität * 3 + Kategorie (Apps vor Einstellungen vor Dateien).
    let mut hits: Vec<(u32, Entry)> = Vec::new();
    for (i, a) in st.apps.iter().enumerate() {
        if let Some(s) = score(&a.lower) {
            let e = Entry { title: a.name.clone(), kind: "App", sub: String::new(), target: Target::App(i), glyph: Glyph::App };
            hits.push((s * 3, e));
        }
    }
    for (name, uri, keys) in SETTINGS {
        let s = [score(&name.to_lowercase()), score(keys)].into_iter().flatten().min();
        if let Some(s) = s {
            let e = Entry { title: name.to_string(), kind: "Einstellungen", sub: String::new(), target: Target::Uri(uri.to_string()), glyph: Glyph::Settings };
            hits.push((s * 3 + 1, e));
        }
    }
    for e in &st.recent {
        if let Some(s) = score(&e.title.to_lowercase()) {
            hits.push((s * 3 + 2, e.clone()));
        }
    }
    hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.title.len().cmp(&b.1.title.len())));
    let mut rows: Vec<Row> = hits.into_iter().take(40).map(|(_, e)| Row::Item(e)).collect();

    // Dateien aus dem Suchindex (sobald da und noch zur Anfrage passend),
    // ohne Dopplungen mit "Zuletzt verwendet".
    if st.files_for == st.query {
        let recent_titles: Vec<String> = st.recent.iter().map(|e| e.title.to_lowercase()).collect();
        for e in st.files.iter().filter(|e| !recent_titles.contains(&e.title.to_lowercase())).take(10) {
            rows.push(Row::Item(e.clone()));
        }
    }

    let raw = st.query.trim();
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    rows.push(Row::Item(Entry {
        title: format!("„{raw}“ in Dateien suchen"),
        kind: "Explorer",
        sub: String::new(),
        target: Target::Uri(format!("search-ms:query={}&crumb=location:{}", url_encode(raw), url_encode(&home))),
        glyph: Glyph::Search,
    }));
    rows.push(Row::Item(Entry {
        title: format!("„{raw}“ im Web suchen"),
        kind: "Web",
        sub: String::new(),
        target: Target::Uri(format!("https://duckduckgo.com/?q={}", url_encode(raw))),
        glyph: Glyph::Web,
    }));
    rows
}

// ------------------------------------------------------------- Zeichnen

unsafe fn icon_bitmap(st: &mut State, t: &Target, size: i32) -> HBITMAP {
    let key = match t {
        Target::App(i) => format!("a{size}:{}", st.apps[*i].id),
        Target::Path(p) => format!("p{size}:{p}"),
        Target::Uri(_) => return null_mut(),
    };
    if let Some(&b) = st.icons.get(&key) {
        return b;
    }
    let b = match t {
        Target::App(i) => apps::app_icon(&st.apps[*i], size),
        Target::Path(p) => apps::path_icon(p, size),
        Target::Uri(_) => null_mut(),
    };
    st.icons.insert(key, b);
    b
}

unsafe fn draw_icon(st: &mut State, dc: HDC, f: &Fonts, e: &Entry, x: i32, y: i32, size: i32) {
    let b = icon_bitmap(st, &e.target, size);
    if !b.is_null() {
        draw_alpha(dc, b, x, y, size);
        return;
    }
    let g = match e.glyph {
        Glyph::Settings => "\u{E713}",
        Glyph::Web => "\u{E774}",
        Glyph::Search => "\u{E721}",
        Glyph::File => "\u{E8A5}",
        Glyph::App => "\u{E71D}",
    };
    let font = if size > 24 { f.icons_big } else { f.icons };
    let r = RECT { left: x, top: y, right: x + size, bottom: y + size };
    text(dc, g, &r, font, accent(), DT_CENTER | DT_VCENTER | DT_SINGLELINE);
}

const LINE: u32 = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS;

/// Kleiner Pill-Button rechtsbündig an `right`.
unsafe fn pill(st: &mut State, dc: HDC, f: &Fonts, label: &str, glyph: &str, glyph_left: bool, right: i32, top: i32, act: Act) {
    let d = st.dpi;
    let tw = text_width(dc, label, f.small);
    let r = RECT { left: right - tw - px(d, 40), top, right, bottom: top + px(d, 28) };
    let hov = inside(&r, st.mouse.0, st.mouse.1);
    round(dc, &r, if hov { ACTIVE } else { HOVER }, px(d, 8));
    if glyph_left {
        text(dc, glyph, &RECT { left: r.left + px(d, 10), ..r }, f.icons_small, TEXT, LINE);
        text(dc, label, &RECT { left: r.left + px(d, 28), ..r }, f.small, TEXT, LINE);
    } else {
        text(dc, label, &RECT { left: r.left + px(d, 12), ..r }, f.small, TEXT, LINE);
        text(dc, glyph, &RECT { right: r.right - px(d, 10), ..r }, f.icons_small, TEXT, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
    }
    st.hits.push((r, act));
}

unsafe fn draw_list(st: &mut State, dc: HDC, f: &Fonts, rows: &[Row], area: RECT, sel: Option<usize>) {
    let d = st.dpi;
    let saved = SaveDC(dc);
    IntersectClipRect(dc, area.left, area.top, area.right, area.bottom);
    let mut y = area.top - st.scroll;
    let mut item = 0usize;
    for row in rows {
        match row {
            Row::Header(t) => {
                let h = px(d, HEAD_H);
                if y + h > area.top && y < area.bottom {
                    let r = RECT { left: area.left + px(d, PAD + 12), top: y, right: area.right - px(d, PAD), bottom: y + h };
                    text(dc, t, &r, f.bold, TEXT, LINE);
                }
                y += h;
            }
            Row::Item(e) => {
                let h = px(d, ROW_H);
                if y + h > area.top && y < area.bottom {
                    let r = RECT { left: area.left + px(d, PAD), top: y, right: area.right - px(d, PAD), bottom: y + h };
                    let vis = RECT { top: r.top.max(area.top), bottom: r.bottom.min(area.bottom), ..r };
                    if sel == Some(item) {
                        round(dc, &r, ACTIVE, px(d, 6));
                    } else if inside(&vis, st.mouse.0, st.mouse.1) {
                        round(dc, &r, HOVER, px(d, 6));
                    }
                    let is = px(d, 24);
                    draw_icon(st, dc, f, e, r.left + px(d, 12), r.top + (h - is) / 2, is);
                    let label_w = if e.kind.is_empty() { 0 } else { text_width(dc, e.kind, f.small) + px(d, 16) };
                    let tr = RECT { left: r.left + px(d, 48), right: r.right - label_w - px(d, 8), ..r };
                    if e.sub.is_empty() {
                        text(dc, &e.title, &tr, f.normal, TEXT, LINE);
                    } else {
                        let mid = (r.top + r.bottom) / 2;
                        text(dc, &e.title, &RECT { bottom: mid + px(d, 2), ..tr }, f.normal, TEXT, LINE);
                        text(dc, &e.sub, &RECT { top: mid, ..tr }, f.small, SUBTEXT, LINE);
                    }
                    if !e.kind.is_empty() {
                        let kr = RECT { right: r.right - px(d, 12), ..r };
                        text(dc, e.kind, &kr, f.small, SUBTEXT, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
                    }
                    st.hits.push((vis, Act::Open(e.target.clone())));
                }
                y += h;
                item += 1;
            }
        }
    }
    st.max_scroll = (y + st.scroll - area.top - (area.bottom - area.top)).max(0);
    st.list_h = area.bottom - area.top;
    RestoreDC(dc, saved);
}

unsafe fn draw_home(st: &mut State, dc: HDC, f: &Fonts, w: i32, top: i32, bottom: i32) {
    let d = st.dpi;
    let head = RECT { left: px(d, PAD + 12), top, right: w / 2, bottom: top + px(d, 28) };
    text(dc, "Angeheftet", &head, f.bold, TEXT, LINE);
    pill(st, dc, f, "Alle Apps", "\u{E76C}", false, w - px(d, PAD), top, Act::ShowAll);

    let grid_top = top + px(d, 40);
    let cell_w = (w - 2 * px(d, PAD)) / COLS as i32;
    let cell_h = px(d, 84);
    let pins: Vec<usize> = st.pin_idx.iter().copied().take(COLS * PIN_ROWS).collect();
    for (n, &i) in pins.iter().enumerate() {
        let (c, r) = ((n % COLS) as i32, (n / COLS) as i32);
        let cell = RECT {
            left: px(d, PAD) + c * cell_w,
            top: grid_top + r * cell_h,
            right: px(d, PAD) + (c + 1) * cell_w,
            bottom: grid_top + (r + 1) * cell_h,
        };
        if inside(&cell, st.mouse.0, st.mouse.1) {
            round(dc, &cell, HOVER, px(d, 6));
        }
        let e = Entry {
            title: st.apps[i].name.clone(),
            kind: "",
            sub: String::new(),
            target: Target::App(i),
            glyph: Glyph::App,
        };
        let is = px(d, 32);
        draw_icon(st, dc, f, &e, cell.left + (cell_w - is) / 2, cell.top + px(d, 14), is);
        let tr = RECT { left: cell.left + px(d, 4), top: cell.top + px(d, 52), right: cell.right - px(d, 4), bottom: cell.bottom - px(d, 6) };
        text(dc, &e.title, &tr, f.small, TEXT, DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        st.hits.push((cell, Act::Open(e.target)));
    }

    // Empfohlen: zuletzt verwendete Dateien, zwei Spalten.
    let rec_top = grid_top + PIN_ROWS as i32 * cell_h + px(d, 20);
    if rec_top + px(d, 40) >= bottom {
        return;
    }
    text(dc, "Empfohlen", &RECT { left: px(d, PAD + 12), top: rec_top, right: w, bottom: rec_top + px(d, 28) }, f.bold, TEXT, LINE);
    let item_top = rec_top + px(d, 40);
    let col_w = (w - 2 * px(d, PAD)) / 2;
    let row_h = px(d, 56);
    let rows = ((bottom - item_top) / row_h).max(0) as usize;
    let recent: Vec<Entry> = st.recent.iter().take(rows * 2).cloned().collect();
    for (n, e) in recent.iter().enumerate() {
        let (c, r) = ((n % 2) as i32, (n / 2) as i32);
        let cell = RECT {
            left: px(d, PAD) + c * col_w,
            top: item_top + r * row_h,
            right: px(d, PAD) + (c + 1) * col_w - px(d, 8),
            bottom: item_top + (r + 1) * row_h - px(d, 4),
        };
        if inside(&cell, st.mouse.0, st.mouse.1) {
            round(dc, &cell, HOVER, px(d, 6));
        }
        let is = px(d, 32);
        draw_icon(st, dc, f, e, cell.left + px(d, 12), cell.top + (cell.bottom - cell.top - is) / 2, is);
        let tr = RECT { left: cell.left + px(d, 56), right: cell.right - px(d, 8), ..cell };
        let mid = (cell.top + cell.bottom) / 2;
        text(dc, &e.title, &RECT { bottom: mid + px(d, 1), ..tr }, f.normal, TEXT, LINE);
        text(dc, &e.sub, &RECT { top: mid + px(d, 1), ..tr }, f.small, SUBTEXT, LINE);
        st.hits.push((cell, Act::Open(e.target.clone())));
    }
}

unsafe fn draw(st: &mut State, dc: HDC, w: i32, h: i32) {
    let Some(f) = st.fonts else { return };
    let d = st.dpi;
    st.hits.clear();
    fill(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, BG);

    // Suchfeld
    let field = RECT { left: px(d, PAD), top: px(d, 24), right: w - px(d, PAD), bottom: px(d, 24 + 36) };
    round(dc, &field, FIELD, px(d, 36));
    outline(dc, &field, BORDER, px(d, 36));
    // Win11 markiert das fokussierte Feld mit einer Akzentlinie unten.
    fill(dc, &RECT { left: field.left + px(d, 18), top: field.bottom - px(d, 2), right: field.right - px(d, 18), bottom: field.bottom }, accent());
    text(dc, "\u{E721}", &RECT { left: field.left + px(d, 16), ..field }, f.icons, SUBTEXT, LINE);
    let tx = field.left + px(d, 46);
    let tr = RECT { left: tx, right: field.right - px(d, 16), ..field };
    if st.query.is_empty() {
        text(dc, "Nach Apps, Einstellungen und Dateien suchen", &tr, f.normal, DIM, LINE);
    } else {
        text(dc, &st.query, &tr, f.normal, TEXT, LINE);
    }
    let cx = (tx + text_width(dc, &st.query, f.normal)).min(tr.right);
    fill(dc, &RECT { left: cx, top: field.top + px(d, 10), right: cx + px(d, 1).max(1), bottom: field.bottom - px(d, 10) }, TEXT);

    let body_top = px(d, 80);
    let body_bottom = h - px(d, FOOT);
    if !st.query.is_empty() {
        let rows = std::mem::take(&mut st.results);
        let area = RECT { left: 0, top: body_top, right: w, bottom: body_bottom - px(d, 8) };
        let sel = Some(st.sel);
        draw_list(st, dc, &f, &rows, area, sel);
        st.results = rows;
    } else if st.view == View::All {
        text(dc, "Alle Apps", &RECT { left: px(d, PAD + 12), top: body_top, right: w / 2, bottom: body_top + px(d, 28) }, f.bold, TEXT, LINE);
        pill(st, dc, &f, "Zurück", "\u{E76B}", true, w - px(d, PAD), body_top, Act::ShowHome);
        let rows = std::mem::take(&mut st.all_rows);
        let area = RECT { left: 0, top: body_top + px(d, 40), right: w, bottom: body_bottom - px(d, 8) };
        draw_list(st, dc, &f, &rows, area, None);
        st.all_rows = rows;
    } else {
        draw_home(st, dc, &f, w, body_top, body_bottom - px(d, 8));
    }

    // Fußleiste: Nutzer + Ein/Aus
    let foot = RECT { left: 0, top: body_bottom, right: w, bottom: h };
    fill(dc, &foot, PANEL);
    fill(dc, &RECT { bottom: foot.top + 1, ..foot }, BORDER);
    let user = if crate::settings::get().presentation { "Benutzer".to_string() } else { std::env::var("USERNAME").unwrap_or_default() };
    let av = px(d, 32);
    let ar = RECT { left: px(d, PAD + 16), top: foot.top + (px(d, FOOT) - av) / 2, right: px(d, PAD + 16) + av, bottom: foot.top + (px(d, FOOT) + av) / 2 };
    let account = RECT { left: px(d, PAD), top: foot.top + px(d, 12), right: ar.right + px(d, 24) + text_width(dc, &user, f.normal), bottom: h - px(d, 12) };
    if inside(&account, st.mouse.0, st.mouse.1) {
        round(dc, &account, HOVER, px(d, 6));
    }
    st.hits.push((account, Act::Account));
    round(dc, &ar, ACTIVE, av);
    let initial: String = user.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    text(dc, &initial, &ar, f.bold, TEXT, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    text(dc, &user, &RECT { left: ar.right + px(d, 12), right: w / 2, ..foot }, f.normal, TEXT, LINE);
    let pb = RECT { left: w - px(d, PAD + 40), top: foot.top + px(d, 12), right: w - px(d, PAD), bottom: h - px(d, 12) };
    if inside(&pb, st.mouse.0, st.mouse.1) {
        round(dc, &pb, HOVER, px(d, 6));
    }
    text(dc, "\u{E7E8}", &pb, f.icons, TEXT, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    st.hits.push((pb, Act::Power));
}

// ------------------------------------------------------------- Aktionen

pub unsafe fn toggle(_search: bool) {
    let h = hwnd();
    if h.is_null() {
        return;
    }
    if IsWindowVisible(h) != 0 {
        hide();
    } else {
        show();
    }
}

unsafe fn show() {
    let h = hwnd();
    let bar = crate::bar::anchor();
    let dpi = GetDpiForWindow(bar);
    let recent = load_recent();
    with(|st| {
        if st.dpi != dpi || st.fonts.is_none() {
            if let Some(f) = st.fonts.take() {
                f.delete();
            }
            st.fonts = Some(Fonts::new(dpi));
            st.dpi = dpi;
        }
        st.recent = recent;
        st.query.clear();
        st.results.clear();
        st.view = View::Home;
        st.sel = 0;
        st.scroll = 0;
    });
    let mut br: RECT = zeroed();
    GetWindowRect(bar, &mut br);
    let (w, hh) = (px(dpi, W), px(dpi, H));
    // Wie Windows: bei linksbündiger Taskleiste links, sonst mittig.
    let x = if crate::util::taskbar_left() { br.left + px(dpi, 12) } else { br.left + (br.right - br.left - w) / 2 };
    let y = br.top - px(dpi, 12) - hh;
    crate::anim::pop_in(h, x, y, w, hh, px(dpi, 18));
    force_foreground(h);
    InvalidateRect(h, null(), 0);
}

unsafe fn hide() {
    let h = hwnd();
    if IsWindowVisible(h) != 0 {
        ShowWindow(h, SW_HIDE);
        // Katalog im Hintergrund auffrischen, solange keiner hinschaut.
        PostMessageW(h, WM_RELOAD, 0, 0);
    }
}

unsafe fn open(t: Target) {
    let app_id = with(|st| match &t {
        Target::App(i) => st.apps.get(*i).map(|a| a.id.clone()),
        _ => None,
    })
    .flatten();
    hide();
    match t {
        Target::App(_) => {
            if let Some(id) = app_id {
                apps::launch(&id);
            }
        }
        Target::Path(p) => shell_open(&p, "", SW_SHOWNORMAL),
        Target::Uri(u) => shell_open(&u, "", SW_SHOWNORMAL),
    }
}

/// Rechtsklick auf eine App: anheften (Start/Taskleiste), als Admin starten,
/// Speicherort öffnen, deinstallieren.
unsafe fn app_menu(hwnd: HWND, app: usize) {
    let Some((id, pinned, exe)) = with(|st| {
        let a = st.apps.get(app)?;
        Some((a.id.clone(), st.pins.contains(&a.id), apps::target_path(a)))
    })
    .flatten() else {
        return;
    };
    let mut items = vec![Item::Cmd(1, if pinned { "Von Start lösen" } else { "An Start anheften" })];
    if let Some(exe) = &exe {
        let on_bar = crate::bar::is_pinned(exe);
        items.push(Item::Cmd(2, if on_bar { "Von Taskleiste lösen" } else { "An Taskleiste anheften" }));
        items.push(Item::Sep);
        items.push(Item::Cmd(3, "Als Administrator ausführen"));
        items.push(Item::Cmd(4, "Dateispeicherort öffnen"));
    }
    items.push(Item::Sep);
    items.push(Item::Cmd(5, "Deinstallieren"));
    let cmd = menus::popup(hwnd, &items);
    let exe = exe.unwrap_or_default();
    match cmd {
        2 => crate::bar::toggle_pin(&exe),
        3 => {
            hide();
            shell_exec("runas", &exe, "");
        }
        4 => {
            hide();
            shell_open("explorer.exe", &format!("/select,\"{exe}\""), SW_SHOWNORMAL);
        }
        5 => {
            hide();
            shell_open("ms-settings:appsfeatures", "", SW_SHOWNORMAL);
        }
        _ => {}
    }
    if cmd == 1 {
        with(|st| {
            if pinned {
                st.pins.retain(|p| p != &id);
            } else {
                st.pins.push(id);
            }
            save_pins(&st.pins);
            st.pin_idx = st.pins.iter().filter_map(|id| st.apps.iter().position(|a| &a.id == id)).collect();
        });
        InvalidateRect(hwnd, null(), 0);
    }
}

fn hit(st: &State, x: i32, y: i32) -> Option<Act> {
    st.hits.iter().rev().find(|(r, _)| inside(r, x, y)).map(|(_, a)| a.clone())
}

/// Index-Suche für die aktuelle Anfrage im Hintergrund starten.
unsafe fn start_file_search(hwnd: HWND) {
    if crate::settings::get().presentation {
        return; // Dateinamen und Pfade sind privat.
    }
    let Some((query, gen)) = with(|st| {
        st.files_gen += 1;
        (st.query.trim().to_string(), st.files_gen)
    }) else {
        return;
    };
    if query.chars().count() < 2 {
        return; // Ein Zeichen findet zu viel, um nützlich zu sein.
    }
    let target = hwnd as isize;
    std::thread::spawn(move || unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        let found = apps::search_index(&query, 12);
        if let Ok(mut r) = FILE_RESULTS.lock() {
            *r = Some((gen, query, found));
        }
        PostMessageW(target as HWND, WM_FILES, gen as usize, 0);
    });
}

/// Index-Treffer übernehmen, wenn sie noch zur aktuellen Eingabe gehören.
unsafe fn apply_file_results(hwnd: HWND, gen: u64) {
    let Some((rgen, query, found)) = FILE_RESULTS.lock().ok().and_then(|mut r| r.take()) else { return };
    let applied = with(|st| {
        if rgen != gen || gen != st.files_gen || query != st.query.trim() {
            return false;
        }
        st.files = found
            .into_iter()
            .map(|(name, path)| {
                let folder = Path::new(&path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                Entry { title: name, kind: "Datei", sub: folder, target: Target::Path(path), glyph: Glyph::File }
            })
            .collect();
        st.files_for = st.query.clone();
        let sel = st.sel;
        st.results = search(st);
        st.sel = sel.min(st.results.len().saturating_sub(1));
        true
    })
    .unwrap_or(false);
    if applied {
        InvalidateRect(hwnd, null(), 0);
    }
}

unsafe fn on_key(hwnd: HWND, vk: u16) -> bool {
    let mut open_target = None;
    let mut close = false;
    let mut schedule_files = false;
    let handled = with(|st| {
        let count = st.results.len();
        match vk {
            VK_ESCAPE => {
                if st.query.is_empty() && st.view == View::Home {
                    close = true;
                } else {
                    st.query.clear();
                    st.view = View::Home;
                    st.scroll = 0;
                }
            }
            VK_BACK => {
                st.query.pop();
                st.results = if st.query.is_empty() { Vec::new() } else { search(st) };
                schedule_files = true;
                st.sel = 0;
                st.scroll = 0;
            }
            VK_RETURN => {
                if let Some(Row::Item(e)) = st.results.get(st.sel) {
                    open_target = Some(e.target.clone());
                }
            }
            VK_DOWN | VK_UP if count > 0 => {
                st.sel = if vk == VK_DOWN { (st.sel + 1).min(count - 1) } else { st.sel.saturating_sub(1) };
                // Auswahl sichtbar halten.
                let rh = px(st.dpi, ROW_H);
                let top = st.sel as i32 * rh;
                if top < st.scroll {
                    st.scroll = top;
                } else if top + rh > st.scroll + st.list_h {
                    st.scroll = top + rh - st.list_h;
                }
            }
            _ => return false,
        }
        true
    })
    .unwrap_or(false);
    if schedule_files {
        SetTimer(hwnd, TIMER_FILES, 160, None);
    }
    if close {
        hide();
    } else if let Some(t) = open_target {
        open(t);
    } else if handled {
        InvalidateRect(hwnd, null(), 0);
    }
    handled
}

// -------------------------------------------------------------- WndProc

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_buffered(hwnd, |dc, w, h| {
                with(|st| draw(st, dc, w, h));
            });
            0
        }
        WM_ERASEBKGND => 1,
        WM_ACTIVATE => {
            if (wp & 0xffff) as u32 == WA_INACTIVE {
                hide();
            }
            0
        }
        WM_TOGGLE => {
            toggle(wp == 1);
            0
        }
        WM_TIMER if wp == TIMER_FILES => {
            KillTimer(hwnd, TIMER_FILES);
            start_file_search(hwnd);
            0
        }
        WM_FILES => {
            apply_file_results(hwnd, wp as u64);
            0
        }
        WM_RELOAD => {
            let apps = apps::enumerate();
            with(|st| {
                if !apps.is_empty() {
                    st.apps = apps;
                    rebuild(st);
                }
            });
            0
        }
        WM_MOUSEMOVE => {
            let p = xy(lp);
            let start = with(|st| {
                st.mouse = p;
                !std::mem::replace(&mut st.tracking, true)
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
            with(|st| {
                st.mouse = (-1, -1);
                st.tracking = false;
            });
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp >> 16) & 0xffff) as i16 as i32;
            with(|st| {
                st.scroll = (st.scroll - delta * px(st.dpi, ROW_H * 3) / 120).clamp(0, st.max_scroll);
            });
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(lp);
            match with(|st| hit(st, x, y)).flatten() {
                Some(Act::Open(t)) => open(t),
                Some(Act::ShowAll) => {
                    with(|st| {
                        st.view = View::All;
                        st.scroll = 0;
                    });
                }
                Some(Act::ShowHome) => {
                    with(|st| {
                        st.view = View::Home;
                        st.scroll = 0;
                    });
                }
                Some(Act::Power) => {
                    if menus::power_menu(hwnd) {
                        hide();
                    }
                }
                Some(Act::Account) => {
                    if menus::account_menu(hwnd) {
                        hide();
                    }
                }
                None => {}
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_RBUTTONUP => {
            let (x, y) = xy(lp);
            if let Some(Act::Open(Target::App(i))) = with(|st| hit(st, x, y)).flatten() {
                app_menu(hwnd, i);
            }
            0
        }
        WM_KEYDOWN => {
            if on_key(hwnd, wp as u16) {
                0
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        WM_CHAR => {
            if let Some(c) = char::from_u32(wp as u32).filter(|c| !c.is_control()) {
                with(|st| {
                    st.query.push(c);
                    st.results = search(st);
                    st.sel = 0;
                    st.scroll = 0;
                });
                // Index-Suche erst nach kurzer Tipp-Pause.
                SetTimer(hwnd, TIMER_FILES, 160, None);
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub unsafe fn create(inst: HINSTANCE) {
    let class = w16("LightbarStart");
    register_class(inst, &class, Some(wndproc), 0);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
        class.as_ptr(),
        w16("Start").as_ptr(),
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
    // Abgerundete Ecken und dezenter Rand wie bei Windows-11-Flyouts.
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWA_BORDER_COLOR: u32 = 34;
    let round_pref: u32 = 2;
    let border = BORDER;
    DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round_pref as *const _ as _, 4);
    DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const _ as _, 4);

    let apps = apps::enumerate();
    let pins = load_pins(&apps);
    let mut st = State {
        dpi: 0,
        fonts: None,
        apps,
        all_rows: Vec::new(),
        pins,
        pin_idx: Vec::new(),
        recent: Vec::new(),
        icons: HashMap::new(),
        query: String::new(),
        results: Vec::new(),
        exe_ids: None,
        files: Vec::new(),
        files_for: String::new(),
        files_gen: 0,
        view: View::Home,
        sel: 0,
        scroll: 0,
        max_scroll: 0,
        list_h: 0,
        mouse: (-1, -1),
        tracking: false,
        hits: Vec::new(),
    };
    rebuild(&mut st);
    STATE.with(|s| *s.borrow_mut() = Some(st));
    HWND_START.store(hwnd, Ordering::Relaxed);
}
