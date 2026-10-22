//! Startmenü: Suche (Apps, Einstellungen, zuletzt verwendete Dateien, Explorer, Web),
//! angeheftete Apps, Alle Apps, Energie-Menü.
//!
//! Gezeichnet wird pro Frame komplett neu; jede klickbare Fläche wird dabei in
//! `hits` eingetragen, damit Layout und Hit-Testing nie auseinanderlaufen.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::{Duration, Instant, SystemTime};

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
/// Neues Stück einer KI-Antwort.
const WM_AI: u32 = WM_APP + 13;
const TIMER_FILES: usize = 4;
/// Blendet das „Kopiert“ nach dem Kopieren wieder aus.
const TIMER_COPIED: usize = 5;
/// Hintergrundprüfung des App-Katalogs fertig (wp = 1: hat sich geändert).
const WM_CATALOG: u32 = WM_APP + 14;
/// Shell meldet Änderungen in den Startmenü-Ordnern.
const WM_SHELL_CHANGE: u32 = WM_APP + 15;
/// Element per Screenreader auslösen (wp = `Focus::code`).
const WM_UIA_INVOKE: u32 = WM_APP + 16;
/// Wartet nach Änderungen in den Startmenü-Ordnern, bis die Installation fertig ist.
const TIMER_APPS: usize = 6;
/// Älter als das wird der Katalog beim Öffnen im Hintergrund geprüft.
const CATALOG_MAX_AGE: Duration = Duration::from_secs(30);
/// So lange bleibt ein Chat nach dem Ausblenden erhalten.
const CHAT_KEEP: Duration = Duration::from_secs(10 * 60);

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
    /// Frage an die KI (siehe `ai`).
    Ask(String),
}

#[derive(Clone, Copy)]
enum Glyph {
    App,
    Settings,
    Web,
    Search,
    File,
    Ai,
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
    /// Text in die Zwischenablage (Antwort oder Codeblock).
    CopyText(String),
    /// http(s)-Link aus einer Antwort öffnen.
    OpenLink(String),
    /// Chat verwerfen und neu anfangen.
    NewChat,
    /// Zum Ende der Antwort springen und ihr wieder folgen.
    ScrollEnd,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Home,
    All,
    /// Antwort der KI.
    Answer,
}

#[derive(Clone, Copy)]
struct Fonts {
    normal: HFONT,
    small: HFONT,
    bold: HFONT,
    /// Fett in Textgröße (`**…**` im Chat).
    strong: HFONT,
    /// Code im Chat.
    mono: HFONT,
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
            strong: make_font(dpi, 13, FW_SEMIBOLD),
            mono: mono_font(dpi, 12),
            icons: icon_font(dpi, 16),
            icons_small: icon_font(dpi, 10),
            icons_big: icon_font(dpi, 24),
        }
    }
    unsafe fn delete(&self) {
        for f in [self.normal, self.small, self.bold, self.strong, self.mono, self.icons, self.icons_small, self.icons_big] {
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
    /// Kontobild für die Fußleiste: (Größe, Bitmap); Größe 0 = noch nicht geladen.
    avatar: (i32, HBITMAP),
    query: String,
    /// Strg+A: ganze Suche markiert – Tippen oder Einfügen ersetzt sie.
    select_all: bool,
    results: Vec<Row>,
    /// exe (normalisiert) → App-Kennung im Katalog; für Sprunglisten, lazy gebaut.
    exe_ids: Option<HashMap<String, String>>,
    /// Treffer aus dem Windows-Suchindex für `files_for`.
    files: Vec<Entry>,
    files_for: String,
    /// Laufende Nummer der Index-Suche; ältere Antworten werden verworfen.
    files_gen: u64,
    view: View,
    sel: usize,
    scroll: i32,
    max_scroll: i32,
    list_h: i32,
    /// KI-Antwort scrollt beim Streamen mit.
    follow: bool,
    /// Eingabe für Folgefragen im Chat (statt `query`).
    input: String,
    /// Zuletzt ausgeblendet – ein Chat bleibt eine Weile erhalten.
    hidden_at: Option<Instant>,
    /// Gesetzter Chat zu (Inhalt+Breite als Hash), damit Scrollen nicht neu misst.
    chat_layout: Option<(u64, crate::chat::Layout)>,
    /// Zuletzt kopierter Text (Hash) für das kurze „Kopiert“.
    copied: Option<(u64, Instant)>,
    mouse: (i32, i32),
    tracking: bool,
    hits: Vec<(RECT, Act)>,
    /// Zusatz-Suchschlüssel je App: (exe-Name, Kern der Kennung); leer = neu bauen.
    app_keys: Vec<(Option<String>, String)>,
    /// Starts je App-Kennung (launches.txt).
    launches: HashMap<String, u32>,
    /// Tastaturfokus (Suchfeld, Kachel, Listenzeile, Fußleiste).
    focus: Focus,
    /// Fokusrahmen zeigen – erst nach Tastaturbedienung, wie bei Windows.
    kb: bool,
    /// Fokussierbare Elemente des letzten Frames mit ihrer Fläche (Client).
    /// Grundlage für Tastaturnavigation und Screenreader.
    nodes: Vec<(Focus, RECT)>,
    /// Zuletzt fokussierte Zeile in „Alle Apps“ (für den Weg zurück von unten).
    last_list: usize,
    /// Wann der App-Katalog zuletzt gelesen bzw. als aktuell bestätigt wurde.
    loaded_at: Instant,
    /// Hintergrundprüfung des Katalogs läuft.
    checking: bool,
}

impl State {
    fn new(apps: Vec<App>, pins: Vec<String>, launches: HashMap<String, u32>) -> Self {
        State {
            dpi: 0,
            fonts: None,
            apps,
            all_rows: Vec::new(),
            pins,
            pin_idx: Vec::new(),
            recent: Vec::new(),
            icons: HashMap::new(),
            avatar: (0, null_mut()),
            query: String::new(),
            select_all: false,
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
            follow: true,
            input: String::new(),
            hidden_at: None,
            chat_layout: None,
            copied: None,
            mouse: (-1, -1),
            tracking: false,
            hits: Vec::new(),
            app_keys: Vec::new(),
            launches,
            focus: Focus::Search,
            kb: false,
            nodes: Vec::new(),
            last_list: 0,
            loaded_at: Instant::now(),
            checking: false,
        }
    }
}

/// Ein fokussierbares Element im Startmenü.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Search,
    /// „Alle Apps“ auf der Startseite bzw. „Zurück“ in „Alle Apps“.
    Pill,
    /// Kachel im Raster der angehefteten Apps.
    Pinned(usize),
    /// Eintrag unter „Empfohlen“.
    Rec(usize),
    /// Zeile in „Alle Apps“ (gezählt ohne Buchstaben-Überschriften).
    List(usize),
    /// Zeile der Suchtreffer.
    Result(usize),
    Account,
    Power,
}

impl Focus {
    /// Als Zahl (für UI-Automation-Kennungen und Fensternachrichten).
    pub fn code(self) -> usize {
        let (kind, i) = match self {
            Focus::Search => (0, 0),
            Focus::Pill => (1, 0),
            Focus::Pinned(i) => (2, i),
            Focus::Rec(i) => (3, i),
            Focus::List(i) => (4, i),
            Focus::Result(i) => (5, i),
            Focus::Account => (6, 0),
            Focus::Power => (7, 0),
        };
        (kind << 24) | (i & 0xFF_FFFF)
    }

    pub fn from_code(c: usize) -> Option<Focus> {
        let i = c & 0xFF_FFFF;
        Some(match c >> 24 {
            0 => Focus::Search,
            1 => Focus::Pill,
            2 => Focus::Pinned(i),
            3 => Focus::Rec(i),
            4 => Focus::List(i),
            5 => Focus::Result(i),
            6 => Focus::Account,
            7 => Focus::Power,
            _ => return None,
        })
    }
}

/// Richtung einer Pfeiltaste.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Schritt in einem Raster aus `n` Elementen mit `cols` Spalten, wie im
/// Windows-Startmenü: links/rechts laufen über das Zeilenende hinaus weiter,
/// runter in eine kürzere letzte Zeile landet auf deren letztem Element.
/// `None` = das Raster wird in diese Richtung verlassen.
fn grid_move(i: usize, n: usize, cols: usize, dir: Dir) -> Option<usize> {
    if n == 0 || cols == 0 || i >= n {
        return None;
    }
    match dir {
        Dir::Left => i.checked_sub(1),
        Dir::Right => (i + 1 < n).then_some(i + 1),
        Dir::Up => i.checked_sub(cols),
        Dir::Down => {
            if i + cols < n {
                Some(i + cols)
            } else if i / cols < (n - 1) / cols {
                Some(n - 1)
            } else {
                None
            }
        }
    }
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

/// Einstellungsseiten: (Name deutsch, Name englisch, Adresse, Suchwörter).
/// Die Suchwörter enthalten beide Sprachen, damit die Suche unabhängig von
/// der Oberflächensprache trifft.
pub(crate) const SETTINGS: &[(&str, &str, &str, &str)] = &[
    ("Einstellungen", "Settings", "ms-settings:", "settings einstellungen"),
    ("Bluetooth & Geräte", "Bluetooth & devices", "ms-settings:bluetooth", "bluetooth geräte maus tastatur kopfhörer devices mouse keyboard headphones"),
    ("WLAN", "Wi-Fi", "ms-settings:network-wifi", "wlan wifi netzwerk internet network wireless"),
    ("Netzwerk & Internet", "Network & internet", "ms-settings:network-status", "netzwerk internet ethernet lan network"),
    ("VPN", "VPN", "ms-settings:network-vpn", "vpn"),
    ("Anzeige", "Display", "ms-settings:display", "anzeige bildschirm monitor auflösung skalierung helligkeit display screen resolution scaling brightness"),
    ("Sound", "Sound", "ms-settings:sound", "sound audio lautstärke lautsprecher mikrofon volume speakers microphone"),
    ("Benachrichtigungen", "Notifications", "ms-settings:notifications", "benachrichtigungen mitteilungen notifications"),
    ("Energie & Akku", "Power & battery", "ms-settings:powersleep", "energie akku batterie strom ruhezustand power battery sleep"),
    ("Speicher", "Storage", "ms-settings:storagesense", "speicher festplatte platz storage disk space"),
    ("Hintergrund", "Background", "ms-settings:personalization-background", "hintergrund wallpaper personalisierung background personalization"),
    ("Farben", "Colors", "ms-settings:colors", "farben dunkel modus dark theme akzent colors colours mode accent light"),
    ("Taskleiste", "Taskbar", "ms-settings:taskbar", "taskleiste taskbar"),
    ("Installierte Apps", "Installed apps", "ms-settings:appsfeatures", "apps programme deinstallieren programs uninstall installed"),
    ("Standard-Apps", "Default apps", "ms-settings:defaultapps", "standard apps browser default"),
    ("Autostart", "Startup apps", "ms-settings:startupapps", "autostart startup"),
    ("Konten", "Accounts", "ms-settings:yourinfo", "konto account accounts"),
    ("Datum & Uhrzeit", "Date & time", "ms-settings:dateandtime", "datum uhrzeit zeit zone date time timezone clock"),
    ("Sprache & Region", "Language & region", "ms-settings:regionlanguage", "sprache region language"),
    ("Maus", "Mouse", "ms-settings:mousetouchpad", "maus touchpad zeiger mouse pointer cursor"),
    ("Drucker & Scanner", "Printers & scanners", "ms-settings:printers", "drucker scanner printer printers scanners"),
    ("Windows Update", "Windows Update", "ms-settings:windowsupdate", "update aktualisierung updates"),
    ("Datenschutz & Sicherheit", "Privacy & security", "ms-settings:privacy", "datenschutz sicherheit privacy security"),
    ("Windows-Sicherheit", "Windows Security", "windowsdefender:", "defender virus sicherheit security antivirus"),
    ("Info", "About", "ms-settings:about", "info system pc name version about"),
    ("Gaming", "Gaming", "ms-settings:gaming-gamebar", "gaming spiele game bar games"),
    ("Systemsteuerung", "Control Panel", "shell:ControlPanelFolder", "systemsteuerung control panel"),
];

/// Startbelegung, solange der Nutzer nichts angeheftet hat. Je Platz die
/// Namen, unter denen die App auf deutschen und englischen Systemen heißt.
const DEFAULT_PINS: &[&[&str]] = &[
    &["microsoft edge", "edge"],
    &["datei-explorer", "file explorer", "explorer"],
    &["einstellungen", "settings"],
    &["microsoft store", "store"],
    &["fotos", "photos", "microsoft fotos", "microsoft photos"],
    &["rechner", "calculator", "windows-rechner"],
    &["editor", "notepad", "windows-editor"],
    &["terminal", "windows terminal"],
    &["paint"],
    &["outlook", "microsoft outlook"],
    &["word", "microsoft word"],
    &["excel", "microsoft excel"],
    &["discord"],
    &["claude"],
    &["spotify"],
    &["steam"],
    &["visual studio code"],
    &["firefox", "mozilla firefox"],
    &["google chrome", "chrome"],
    &["duckduckgo"],
];

/// Wörter eines Namens: klein, an allem getrennt, was kein Buchstabe und
/// keine Ziffer ist ("Datei-Explorer" → ["datei", "explorer"]).
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// Wie gut passt ein App-Name zu einem Standardnamen? Nur ganze Wörter:
/// 0 = genau der Name, 1 = der Name beginnt damit ("Outlook (new)").
/// Alles andere zählt nicht – sonst landete "Registrierungs-Editor" auf dem
/// Platz des Editors und "1Password" auf dem von Word.
fn pin_match(name: &str, key: &str) -> Option<u32> {
    let (n, k) = (words(name), words(key));
    if k.is_empty() || n.len() < k.len() || n[..k.len()] != k[..] {
        return None;
    }
    if n.iter().any(|w| matches!(w.as_str(), "uninstall" | "deinstallieren" | "deinstallation")) {
        return None;
    }
    Some(if n.len() == k.len() { 0 } else { 1 })
}

/// Standardbelegung aus (Name, Kennung) aller Apps: je Platz der beste Treffer
/// (genauer Name vor Namensanfang, dann der kürzere Name).
fn default_pins<'a>(apps: impl Iterator<Item = (&'a str, &'a str)> + Clone) -> Vec<String> {
    let mut pins: Vec<String> = Vec::new();
    for keys in DEFAULT_PINS {
        let best = apps
            .clone()
            .filter_map(|(name, id)| keys.iter().filter_map(|k| pin_match(name, k)).min().map(|s| (s, name.len(), id)))
            .min_by_key(|&(s, len, _)| (s, len));
        if let Some((_, _, id)) = best {
            if !pins.iter().any(|p| p == id) {
                pins.push(id.to_string());
            }
        }
        if pins.len() >= COLS * PIN_ROWS {
            break;
        }
    }
    pins
}

fn pins_path() -> Option<PathBuf> {
    Some(data_dir()?.join("pins.txt"))
}

fn load_pins(apps: &[App]) -> Vec<String> {
    if let Some(text) = pins_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        return text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    }
    default_pins(apps.iter().map(|a| (a.name.as_str(), a.id.as_str())))
}

// ------------------------------------------------------ Startzähler

/// Wie oft eine App aus dem Startmenü gestartet wurde – entscheidet bei
/// gleich guten Suchtreffern. Eine Zeile je App: `Kennung<Tab>Anzahl`.
fn launches_path() -> Option<PathBuf> {
    Some(data_dir()?.join("launches.txt"))
}

fn parse_launches(text: &str) -> HashMap<String, u32> {
    text.lines()
        .filter_map(|l| {
            let (id, n) = l.rsplit_once('\t')?;
            Some((id.to_string(), n.trim().parse().ok()?))
        })
        .filter(|(id, _): &(String, u32)| !id.is_empty())
        .collect()
}

fn format_launches(map: &HashMap<String, u32>) -> String {
    let mut v: Vec<_> = map.iter().collect();
    // Häufigste zuerst, damit die Datei lesbar bleibt.
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    v.iter().map(|(id, n)| format!("{id}\t{n}\n")).collect()
}

fn load_launches() -> HashMap<String, u32> {
    launches_path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse_launches(&t)).unwrap_or_default()
}

/// Start einer App mitzählen und sofort speichern (klein, selten).
fn count_launch(st: &mut State, id: &str) {
    let n = st.launches.entry(id.to_string()).or_insert(0);
    *n = n.saturating_add(1);
    if let Some(p) = launches_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, format_launches(&st.launches));
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

fn ago(t: SystemTime) -> String {
    let s = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match s {
        0..=59 => t!("Gerade eben", "Just now").into(),
        60..=3599 => tf!("vor {} Min.", "{} min ago", s / 60),
        3600..=86399 => tf!("vor {} Std.", "{} h ago", s / 3600),
        86400..=172799 => t!("Gestern", "Yesterday").into(),
        _ => tf!("vor {} Tagen", "{} days ago", s / 86400),
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
            kind: t!("Datei", "File"),
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
    st.app_keys.clear();
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

/// Trefferqualität eines Textes (klein geschrieben) für die Anfrage `q`
/// (klein, getrimmt): 0 = beginnt damit, 1 = ein Wort beginnt damit,
/// 2 = kommt irgendwo vor.
fn quality(hay: &str, q: &str) -> Option<u32> {
    if hay.starts_with(q) {
        Some(0)
    } else if hay.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(q)) {
        Some(1)
    } else if hay.contains(q) {
        Some(2)
    } else {
        None
    }
}

/// Trefferqualität einer App: Anzeigename, dazu Dateiname der exe ("notepad"
/// findet "Editor", "cmd" die Eingabeaufforderung) und der Kern der
/// App-Kennung ("calc" findet den Store-Rechner). Die Zusatzschlüssel greifen
/// erst ab zwei bzw. drei Zeichen, sonst passt fast alles.
fn app_quality(q: &str, name: &str, exe: Option<&str>, id: &str) -> Option<u32> {
    let n = q.chars().count();
    let by_name = quality(name, q);
    let by_exe = exe.filter(|_| n >= 2).and_then(|e| {
        if e == q {
            Some(0)
        } else if e.starts_with(q) {
            Some(1)
        } else {
            (n >= 3 && e.contains(q)).then_some(2)
        }
    });
    let by_id = (n >= 3 && id.contains(q)).then_some(2);
    [by_name, by_exe, by_id].into_iter().flatten().min()
}

/// Trefferqualität einer Einstellungsseite – beide Sprachen und die
/// Suchwörter, unabhängig von der Oberflächensprache.
fn settings_quality(q: &str, de: &str, en: &str, keys: &str) -> Option<u32> {
    [quality(&de.to_lowercase(), q), quality(&en.to_lowercase(), q), quality(keys, q)].into_iter().flatten().min()
}

/// Ein Suchtreffer vor dem Sortieren.
struct Hit {
    quality: u32,
    /// Apps vor Einstellungen vor Dateien.
    category: u32,
    /// Starts aus dem Startmenü; bei gleichem Rang zuerst die häufigeren.
    launches: u32,
    entry: Entry,
}

/// Rang: Trefferqualität, dann Kategorie, dann Häufigkeit, dann der kürzere Titel.
fn sort_hits(hits: &mut [Hit]) {
    hits.sort_by(|a, b| {
        (a.quality, a.category)
            .cmp(&(b.quality, b.category))
            .then(b.launches.cmp(&a.launches))
            .then(a.entry.title.len().cmp(&b.entry.title.len()))
    });
}

/// Zusätzliche Suchschlüssel je App (exe-Name, Kern der Kennung), beim
/// ersten Bedarf nach dem (Neu-)Laden des Katalogs gebaut.
fn ensure_keys(st: &mut State) {
    if st.app_keys.len() == st.apps.len() {
        return;
    }
    st.app_keys = st
        .apps
        .iter()
        .map(|a| {
            let exe = unsafe { apps::target_path(a) };
            (exe.as_deref().map(apps::exe_stem), apps::id_key(&a.id))
        })
        .collect();
}

fn search(st: &mut State) -> Vec<Row> {
    ensure_keys(st);
    let q = st.query.trim().to_lowercase();
    let mut hits: Vec<Hit> = Vec::new();
    for (i, a) in st.apps.iter().enumerate() {
        let (exe, id) = &st.app_keys[i];
        if let Some(s) = app_quality(&q, &a.lower, exe.as_deref(), id) {
            let e = Entry { title: a.name.clone(), kind: t!("App", "App"), sub: String::new(), target: Target::App(i), glyph: Glyph::App };
            let launches = st.launches.get(&a.id).copied().unwrap_or(0);
            hits.push(Hit { quality: s, category: 0, launches, entry: e });
        }
    }
    for (de, en, uri, keys) in SETTINGS {
        if let Some(s) = settings_quality(&q, de, en, keys) {
            let name = if crate::i18n::en() { en } else { de };
            let e = Entry { title: name.to_string(), kind: t!("Einstellungen", "Settings"), sub: String::new(), target: Target::Uri(uri.to_string()), glyph: Glyph::Settings };
            hits.push(Hit { quality: s, category: 1, launches: 0, entry: e });
        }
    }
    for e in &st.recent {
        if let Some(s) = quality(&e.title.to_lowercase(), &q) {
            hits.push(Hit { quality: s, category: 2, launches: 0, entry: e.clone() });
        }
    }
    sort_hits(&mut hits);
    let mut rows: Vec<Row> = hits.into_iter().take(40).map(|h| Row::Item(h.entry)).collect();

    // Dateien aus dem Suchindex (sobald da und noch zur Anfrage passend),
    // ohne Dopplungen mit "Zuletzt verwendet".
    if st.files_for == st.query {
        let recent_titles: Vec<String> = st.recent.iter().map(|e| e.title.to_lowercase()).collect();
        for e in st.files.iter().filter(|e| !recent_titles.contains(&e.title.to_lowercase())).take(10) {
            rows.push(Row::Item(e.clone()));
        }
    }

    let raw = st.query.trim();
    // "?" am Anfang fragt direkt die KI – dann steht die Frage ganz oben.
    if crate::ai::enabled() {
        let question = raw.trim_start_matches('?').trim();
        if !question.is_empty() {
            let e = Entry { title: tf!("„{question}“ fragen", "Ask \"{question}\""), kind: t!("KI", "AI"), sub: String::new(), target: Target::Ask(question.to_string()), glyph: Glyph::Ai };
            if raw.starts_with('?') {
                rows.insert(0, Row::Item(e));
            } else {
                rows.push(Row::Item(e));
            }
        }
    }
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    rows.push(Row::Item(Entry {
        title: tf!("„{raw}“ in Dateien suchen", "Search files for \"{raw}\""),
        kind: "Explorer",
        sub: String::new(),
        target: Target::Uri(format!("search-ms:query={}&crumb=location:{}", url_encode(raw), url_encode(&home))),
        glyph: Glyph::Search,
    }));
    rows.push(Row::Item(Entry {
        title: tf!("„{raw}“ im Web suchen", "Search the web for \"{raw}\""),
        kind: "Web",
        sub: String::new(),
        target: Target::Uri(format!("https://duckduckgo.com/?q={}", url_encode(raw))),
        glyph: Glyph::Web,
    }));
    rows
}

// ------------------------------------------------------------- Zeichnen

/// Zwischengespeicherte Icons freigeben: Datei-Icons oder (all) alle.
unsafe fn purge_icons(st: &mut State, all: bool) {
    st.icons.retain(|k, b| {
        let keep = !all && !k.starts_with('p');
        if !keep && !b.is_null() {
            DeleteObject(*b);
        }
        keep
    });
}

unsafe fn icon_bitmap(st: &mut State, t: &Target, size: i32) -> HBITMAP {
    // Index kann nach einem Neuladen der Apps veraltet sein – dann kein Symbol.
    let key = match t {
        Target::App(i) => match st.apps.get(*i) {
            Some(a) => format!("a{size}:{}", a.id),
            None => return null_mut(),
        },
        Target::Path(p) => format!("p{size}:{p}"),
        Target::Uri(_) | Target::Ask(_) => return null_mut(),
    };
    if let Some(&b) = st.icons.get(&key) {
        return b;
    }
    let b = match t {
        Target::App(i) => st.apps.get(*i).map_or(null_mut(), |a| apps::app_icon(a, size)),
        Target::Path(p) => apps::path_icon(p, size),
        Target::Uri(_) | Target::Ask(_) => null_mut(),
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
        Glyph::Ai => "\u{E8BD}",
    };
    let font = if size > 24 { f.icons_big } else { f.icons };
    let r = RECT { left: x, top: y, right: x + size, bottom: y + size };
    text(dc, g, &r, font, accent(), DT_CENTER | DT_VCENTER | DT_SINGLELINE);
}

const LINE: u32 = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS;

/// Kleiner Pill-Button rechtsbündig an `right`; gibt seine linke Kante zurück.
unsafe fn pill(st: &mut State, dc: HDC, f: &Fonts, label: &str, glyph: &str, glyph_left: bool, right: i32, top: i32, act: Act) -> i32 {
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
    if matches!(act, Act::ShowAll) || (matches!(act, Act::ShowHome) && st.view == View::All) {
        st.nodes.push((Focus::Pill, r));
    }
    st.hits.push((r, act));
    r.left
}

/// Oberkante (relativ zum Listenanfang, ohne Bildlauf) und Höhe der
/// `item`-ten Zeile einer Liste mit Überschriften.
fn item_span(rows: &[Row], dpi: u32, item: usize) -> Option<(i32, i32)> {
    let mut y = 0;
    let mut n = 0;
    for row in rows {
        match row {
            Row::Header(_) => y += px(dpi, HEAD_H),
            Row::Item(_) => {
                if n == item {
                    return Some((y, px(dpi, ROW_H)));
                }
                y += px(dpi, ROW_H);
                n += 1;
            }
        }
    }
    None
}

/// Die `n`-te Zeile (ohne Überschriften) einer Liste.
fn nth_item(rows: &[Row], n: usize) -> Option<&Entry> {
    rows.iter()
        .filter_map(|r| match r {
            Row::Item(e) => Some(e),
            Row::Header(_) => None,
        })
        .nth(n)
}

fn item_count(rows: &[Row]) -> usize {
    rows.iter().filter(|r| matches!(r, Row::Item(_))).count()
}

/// Bildlauf so, dass die `item`-te Zeile ganz sichtbar ist.
fn scroll_into_view(st: &mut State, rows_all: bool, item: usize) {
    let rows = if rows_all { &st.all_rows } else { &st.results };
    let Some((top, h)) = item_span(rows, st.dpi, item) else { return };
    // Kopfzeile mit dabei, wenn die erste Zeile unter einer Überschrift steht.
    let top = if item == 0 { 0 } else { top };
    if top < st.scroll {
        st.scroll = top;
    } else if st.list_h > 0 && top + h > st.scroll + st.list_h {
        st.scroll = top + h - st.list_h;
    }
}

unsafe fn draw_list(st: &mut State, dc: HDC, f: &Fonts, rows: &[Row], area: RECT, sel: Option<usize>, node: fn(usize) -> Focus) {
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
                    st.nodes.push((node(item), vis));
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
    text(dc, t!("Angeheftet", "Pinned"), &head, f.bold, TEXT, LINE);
    pill(st, dc, f, t!("Alle Apps", "All apps"), "\u{E76C}", false, w - px(d, PAD), top, Act::ShowAll);

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
        st.nodes.push((Focus::Pinned(n), cell));
    }

    // Empfohlen: zuletzt verwendete Dateien, zwei Spalten.
    let rec_top = grid_top + PIN_ROWS as i32 * cell_h + px(d, 20);
    if rec_top + px(d, 40) >= bottom {
        return;
    }
    text(dc, t!("Empfohlen", "Recommended"), &RECT { left: px(d, PAD + 12), top: rec_top, right: w, bottom: rec_top + px(d, 28) }, f.bold, TEXT, LINE);
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
        st.nodes.push((Focus::Rec(n), cell));
    }
}

/// Fokusrahmen wie bei Windows 11: zwei Pixel hell außen, dunkle Kante innen.
unsafe fn draw_focus(dc: HDC, r: &RECT, dpi: u32) {
    let radius = px(dpi, 8);
    let w = px(dpi, 2).max(2);
    for k in 0..w {
        let rr = RECT { left: r.left + k, top: r.top + k, right: r.right - k, bottom: r.bottom - k };
        outline(dc, &rr, TEXT, radius);
    }
    let inner = RECT { left: r.left + w, top: r.top + w, right: r.right - w, bottom: r.bottom - w };
    outline(dc, &inner, BG, radius);
}

unsafe fn draw(st: &mut State, dc: HDC, w: i32, h: i32) {
    let Some(f) = st.fonts else { return };
    let d = st.dpi;
    st.hits.clear();
    st.nodes.clear();
    fill(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, BG);

    // Suchfeld
    let field = RECT { left: px(d, PAD), top: px(d, 24), right: w - px(d, PAD), bottom: px(d, 24 + 36) };
    st.nodes.push((Focus::Search, field));
    round(dc, &field, FIELD, px(d, 36));
    outline(dc, &field, BORDER, px(d, 36));
    // Win11 markiert das fokussierte Feld mit einer Akzentlinie unten.
    // Nur solange das Feld den Tastaturfokus hat (wie Akzentlinie und Schreibmarke).
    let field_focused = matches!(st.focus, Focus::Search | Focus::Result(_)) || st.view == View::Answer;
    if field_focused {
        fill(dc, &RECT { left: field.left + px(d, 18), top: field.bottom - px(d, 2), right: field.right - px(d, 18), bottom: field.bottom }, accent());
    }
    // Im Chat ist das Feld die Eingabe für Folgefragen.
    let chat = chat_input(st);
    let glyph = if chat { "\u{E8BD}" } else { "\u{E721}" };
    text(dc, glyph, &RECT { left: field.left + px(d, 16), ..field }, f.icons, SUBTEXT, LINE);
    let tx = field.left + px(d, 46);
    let tr = RECT { left: tx, right: field.right - px(d, 16), ..field };
    let shown = if chat { st.input.clone() } else { st.query.clone() };
    if shown.is_empty() {
        let hint = if chat {
            if crate::ai::has_current() { t!("Nachfragen …", "Ask a follow-up …") } else { t!("Frag etwas …", "Ask something …") }
        } else if crate::ai::enabled() {
            t!("Suchen – mit ? am Anfang die KI fragen", "Search – start with ? to ask the AI")
        } else {
            t!("Nach Apps, Einstellungen und Dateien suchen", "Search for apps, settings and files")
        };
        text(dc, hint, &tr, f.normal, DIM, LINE);
    } else {
        if st.select_all {
            // Markierung wie in einem Textfeld hinter dem Text.
            let sw = text_width(dc, &shown, f.normal).min(tr.right - tx);
            fill(dc, &RECT { left: tx - px(d, 1), top: field.top + px(d, 9), right: tx + sw + px(d, 1), bottom: field.bottom - px(d, 9) }, mix(FIELD, accent(), 0.55));
        }
        // Lange Folgefragen: das Ende bleibt sichtbar (dort wird getippt).
        let flags = if chat && text_width(dc, &shown, f.normal) > tr.right - tx { DT_RIGHT | DT_VCENTER | DT_SINGLELINE } else { LINE };
        text(dc, &shown, &tr, f.normal, TEXT, flags);
    }
    let cx = (tx + text_width(dc, &shown, f.normal)).min(tr.right);
    if field_focused {
        fill(dc, &RECT { left: cx, top: field.top + px(d, 10), right: cx + px(d, 1).max(1), bottom: field.bottom - px(d, 10) }, TEXT);
    }

    let body_top = px(d, 80);
    let body_bottom = h - px(d, FOOT);
    if !st.query.is_empty() {
        let rows = std::mem::take(&mut st.results);
        let area = RECT { left: 0, top: body_top, right: w, bottom: body_bottom - px(d, 8) };
        let sel = Some(st.sel);
        draw_list(st, dc, &f, &rows, area, sel, Focus::Result);
        st.results = rows;
    } else if st.view == View::Answer {
        draw_answer(st, dc, &f, w, body_top, body_bottom - px(d, 8));
    } else if st.view == View::All {
        text(dc, t!("Alle Apps", "All apps"), &RECT { left: px(d, PAD + 12), top: body_top, right: w / 2, bottom: body_top + px(d, 28) }, f.bold, TEXT, LINE);
        pill(st, dc, &f, t!("Zurück", "Back"), "\u{E76B}", true, w - px(d, PAD), body_top, Act::ShowHome);
        let rows = std::mem::take(&mut st.all_rows);
        let area = RECT { left: 0, top: body_top + px(d, 40), right: w, bottom: body_bottom - px(d, 8) };
        draw_list(st, dc, &f, &rows, area, None, Focus::List);
        st.all_rows = rows;
    } else {
        draw_home(st, dc, &f, w, body_top, body_bottom - px(d, 8));
    }

    // Fußleiste: Nutzer + Ein/Aus
    let foot = RECT { left: 0, top: body_bottom, right: w, bottom: h };
    fill(dc, &foot, PANEL);
    fill(dc, &RECT { bottom: foot.top + 1, ..foot }, BORDER);
    let user = if crate::settings::get().presentation { t!("Benutzer", "User").to_string() } else { std::env::var("USERNAME").unwrap_or_default() };
    let av = px(d, 32);
    let ar = RECT { left: px(d, PAD + 16), top: foot.top + (px(d, FOOT) - av) / 2, right: px(d, PAD + 16) + av, bottom: foot.top + (px(d, FOOT) + av) / 2 };
    let account = RECT { left: px(d, PAD), top: foot.top + px(d, 12), right: ar.right + px(d, 24) + text_width(dc, &user, f.normal), bottom: h - px(d, 12) };
    if inside(&account, st.mouse.0, st.mouse.1) {
        round(dc, &account, HOVER, px(d, 6));
    }
    st.hits.push((account, Act::Account));
    st.nodes.push((Focus::Account, account));
    let s = crate::settings::get();
    let picture = if s.start_avatar && !s.presentation { avatar(st, av) } else { null_mut() };
    if picture.is_null() {
        round(dc, &ar, ACTIVE, av);
        let initial: String = user.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
        text(dc, &initial, &ar, f.bold, TEXT, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    } else {
        draw_alpha(dc, picture, ar.left, ar.top, av);
    }
    text(dc, &user, &RECT { left: ar.right + px(d, 12), right: w / 2, ..foot }, f.normal, TEXT, LINE);
    let pb = RECT { left: w - px(d, PAD + 40), top: foot.top + px(d, 12), right: w - px(d, PAD), bottom: h - px(d, 12) };
    if inside(&pb, st.mouse.0, st.mouse.1) {
        round(dc, &pb, HOVER, px(d, 6));
    }
    text(dc, "\u{E7E8}", &pb, f.icons, TEXT, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    st.hits.push((pb, Act::Power));
    st.nodes.push((Focus::Power, pb));
    if st.kb && st.focus != Focus::Search {
        if let Some((_, r)) = st.nodes.iter().find(|(n, _)| *n == st.focus) {
            draw_focus(dc, r, d);
        }
    }
}

/// Kontobild in `size`, beim ersten Bedarf geladen. Null, wenn es keins gibt.
unsafe fn avatar(st: &mut State, size: i32) -> HBITMAP {
    if st.avatar.0 != size {
        drop_avatar(st);
        st.avatar = (size, apps::account_picture(size));
    }
    st.avatar.1
}

/// Verwirft das Kontobild; beim nächsten Zeichnen wird es neu gelesen.
unsafe fn drop_avatar(st: &mut State) {
    if !st.avatar.1.is_null() {
        DeleteObject(st.avatar.1);
    }
    st.avatar = (0, null_mut());
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
            // Andere Skalierung: alle Icons werden in neuer Größe gebraucht.
            purge_icons(st, true);
            st.dpi = dpi;
        }
        st.recent = recent;
        st.query.clear();
        st.select_all = false;
        st.results.clear();
        st.sel = 0;
        // Fokus im Suchfeld, Rahmen erst nach Tastaturbedienung.
        st.focus = Focus::Search;
        st.kb = false;
        st.last_list = 0;
        // Kurz weg gewesen: zurück in den Chat, sonst neu auf der Startseite.
        let keep_chat = st.view == View::Answer && st.hidden_at.is_some_and(|t| t.elapsed() < CHAT_KEEP);
        if !keep_chat {
            if st.view == View::Answer {
                crate::ai::cancel();
            }
            st.view = View::Home;
            st.scroll = 0;
            st.input.clear();
            st.chat_layout = None;
        }
    });
    // Angedockte Lage – beim automatischen Ausblenden kann die Leiste gerade weggeschoben sein.
    let br = crate::bar::docked_rect(bar);
    // An der Leiste, wie in Windows eingestellt: links oder mittig (bei einer
    // seitlichen Leiste oben). Auf kleinen oder hochkant gestellten
    // Bildschirmen kürzer bzw. eingerückt.
    use crate::display::{Along, place_near, room_near};
    let gap = px(dpi, 12);
    let (room_w, room_h) = room_near(br, gap);
    let (w, hh) = (px(dpi, W).min(room_w), px(dpi, H).min(room_h));
    let along = if crate::display::edge().vertical() || crate::display::align_left() { Along::Start } else { Along::Center };
    let (x, y) = place_near(br, w, hh, gap, along);
    crate::anim::pop_in(h, x, y, w, hh, crate::display::pop_offset(px(dpi, 18)));
    force_foreground(h);
    InvalidateRect(h, null(), 0);
    // Neu installierte Apps: Ist der Katalog schon eine Weile alt, im
    // Hintergrund nachsehen und die Ansicht auffrischen, falls sich etwas getan hat.
    if with(|st| st.loaded_at.elapsed() > CATALOG_MAX_AGE).unwrap_or(false) {
        check_catalog(h);
    }
    raise_focus(h, Focus::Search);
}

/// Prüfsumme über (Name, Kennung) aller Apps, unabhängig von der Reihenfolge.
fn catalog_sig<'a>(items: impl Iterator<Item = (&'a str, &'a str)>) -> u64 {
    let mut v: Vec<(&str, &str)> = items.collect();
    v.sort_unstable();
    v.dedup_by(|a, b| a.1 == b.1);
    let mut s = String::new();
    for (n, id) in v {
        s.push_str(n);
        s.push('\u{1}');
        s.push_str(id);
        s.push('\u{2}');
    }
    hash_of(&s)
}

/// Katalog in einem Hintergrund-Thread lesen und mit dem geladenen
/// vergleichen. Meldet sich mit WM_CATALOG zurück; nur bei einer Änderung
/// wird (im UI-Thread) neu geladen.
unsafe fn check_catalog(hwnd: HWND) {
    let Some(sig) = with(|st| {
        if std::mem::replace(&mut st.checking, true) {
            return None;
        }
        Some(catalog_sig(st.apps.iter().map(|a| (a.name.as_str(), a.id.as_str()))))
    })
    .flatten() else {
        return;
    };
    let target = hwnd as isize;
    std::thread::spawn(move || unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        let now = apps::enumerate_ids();
        let changed = !now.is_empty() && catalog_sig(now.iter().map(|(n, i)| (n.as_str(), i.as_str()))) != sig;
        PostMessageW(target as HWND, WM_CATALOG, changed as usize, 0);
    });
}

/// App-Katalog neu laden und alles, was per Index darauf zeigt, nachziehen.
unsafe fn reload(hwnd: HWND) {
    let apps = apps::enumerate();
    with(|st| {
        // Kontobild kann sich geändert haben.
        drop_avatar(st);
        st.loaded_at = Instant::now();
        if !apps.is_empty() {
            st.apps = apps;
            rebuild(st);
            // Treffer zeigen per Index auf die alte App-Liste.
            if !st.query.is_empty() {
                let sel = st.sel;
                st.results = search(st);
                st.sel = sel.min(st.results.len().saturating_sub(1));
            }
            clamp_focus(st);
        }
    });
    InvalidateRect(hwnd, null(), 0);
}

/// Startmenü-Ordner (Benutzer und alle Benutzer) bei der Shell beobachten:
/// Neue oder entfernte Verknüpfungen melden sich als WM_SHELL_CHANGE.
/// Store-Apps legen dort nichts ab – für die greift die Prüfung beim Öffnen.
unsafe fn watch_start_folders(hwnd: HWND) {
    use windows::Win32::UI::Shell::*;
    let mut entries = Vec::new();
    for csidl in [CSIDL_PROGRAMS, CSIDL_COMMON_PROGRAMS] {
        if let Ok(pidl) = SHGetSpecialFolderLocation(None, csidl as i32) {
            entries.push(SHChangeNotifyEntry { pidl, fRecursive: true.into() });
        }
    }
    if entries.is_empty() {
        return;
    }
    let events = SHCNE_CREATE.0 | SHCNE_DELETE.0 | SHCNE_RENAMEITEM.0 | SHCNE_MKDIR.0 | SHCNE_RMDIR.0 | SHCNE_UPDATEDIR.0;
    let sources = SHCNRF_ShellLevel | SHCNRF_InterruptLevel | SHCNRF_RecursiveInterrupt;
    SHChangeNotifyRegister(windows::Win32::Foundation::HWND(hwnd as _), sources, events as i32, WM_SHELL_CHANGE, entries.len() as i32, entries.as_ptr());
    // Die PIDLs bleiben für die Laufzeit des Prozesses bestehen (einmalig, klein).
}

unsafe fn hide() {
    let h = hwnd();
    if IsWindowVisible(h) != 0 {
        ShowWindow(h, SW_HIDE);
        with(|st| st.hidden_at = Some(Instant::now()));
        // Datei-Icons (Suche, Empfohlen) freigeben – sonst wüchse der Cache mit
        // jeder je angezeigten Datei. App-Icons bleiben (begrenzt, oft gebraucht).
        with(|st| purge_icons(st, false));
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
    if let Target::Ask(q) = t {
        // Aus der Suche heraus beginnt immer ein neuer Chat.
        crate::ai::cancel();
        ask(q);
        return;
    }
    hide();
    match t {
        Target::App(_) => {
            if let Some(id) = app_id {
                with(|st| count_launch(st, &id));
                apps::launch(&id);
            }
        }
        Target::Path(p) => shell_open(&p, "", SW_SHOWNORMAL),
        Target::Uri(u) => shell_open(&u, "", SW_SHOWNORMAL),
        Target::Ask(_) => {}
    }
}

/// Frage stellen – neu oder als Folgefrage im laufenden Chat; die Antwort
/// erscheint im Startmenü, Stück für Stück.
unsafe fn ask(question: String) {
    with(|st| {
        st.query.clear();
        st.input.clear();
        st.select_all = false;
        st.results.clear();
        st.view = View::Answer;
        st.sel = 0;
        st.follow = true;
    });
    let h = hwnd() as usize;
    crate::ai::ask(question, move || {
        PostMessageW(h as HWND, WM_AI, 0, 0);
    });
    InvalidateRect(hwnd(), null(), 0);
}

/// Tippen geht im Chat in die Eingabe für Folgefragen, sonst in die Suche.
fn chat_input(st: &State) -> bool {
    st.view == View::Answer && st.query.is_empty()
}

/// Das gerade bearbeitete Eingabefeld (Suche oder Folgefrage).
fn field(st: &mut State) -> &mut String {
    if chat_input(st) {
        &mut st.input
    } else {
        &mut st.query
    }
}

/// Neuen Chat beginnen: alter Verlauf weg, leere Eingabe.
fn new_chat(st: &mut State) {
    crate::ai::cancel();
    st.view = View::Answer;
    st.input.clear();
    st.select_all = false;
    st.scroll = 0;
    st.follow = true;
    st.chat_layout = None;
}

fn hash_of(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Zwischenschritte einer Antwort (Werkzeugaufrufe).
fn steps(a: &crate::ai::Answer) -> &[String] {
    &a.steps
}

/// Textmaße über GDI für das Chat-Layout (`chat::Measure`).
struct GdiMeasure {
    dc: HDC,
    fonts: [HFONT; 5],
    /// (Zeilenhöhe, Oberlänge) je Schrift.
    metrics: [(i32, i32); 5],
    old: HFONT,
    cur: std::cell::Cell<usize>,
}

impl GdiMeasure {
    unsafe fn new(dc: HDC, f: &Fonts, dpi: u32) -> Self {
        let fonts = [f.normal, f.strong, f.bold, f.mono, f.small];
        let old = SelectObject(dc, fonts[0]);
        // Etwas Durchschuss zwischen den Zeilen, gleichmäßig verteilt.
        let lead = px(dpi, 4);
        let mut metrics = [(0, 0); 5];
        for (i, &font) in fonts.iter().enumerate() {
            SelectObject(dc, font);
            let mut tm: TEXTMETRICW = std::mem::zeroed();
            GetTextMetricsW(dc, &mut tm);
            metrics[i] = (tm.tmHeight + lead, tm.tmAscent + lead / 2);
        }
        SelectObject(dc, fonts[0]);
        GdiMeasure { dc, fonts, metrics, old, cur: std::cell::Cell::new(0) }
    }

    fn index(font: crate::chat::Font) -> usize {
        use crate::chat::Font;
        match font {
            Font::Normal => 0,
            Font::Bold => 1,
            Font::Heading => 2,
            Font::Mono => 3,
            Font::Small => 4,
        }
    }
}

impl crate::chat::Measure for GdiMeasure {
    fn width(&self, font: crate::chat::Font, s: &str) -> i32 {
        let i = Self::index(font);
        let w: Vec<u16> = s.encode_utf16().collect();
        let mut size = SIZE { cx: 0, cy: 0 };
        unsafe {
            if self.cur.replace(i) != i {
                SelectObject(self.dc, self.fonts[i]);
            }
            GetTextExtentPoint32W(self.dc, w.as_ptr(), w.len() as i32, &mut size);
        }
        size.cx
    }
    fn height(&self, font: crate::chat::Font) -> i32 {
        self.metrics[Self::index(font)].0
    }
    fn ascent(&self, font: crate::chat::Font) -> i32 {
        self.metrics[Self::index(font)].1
    }
}

impl Drop for GdiMeasure {
    fn drop(&mut self) {
        unsafe { SelectObject(self.dc, self.old) };
    }
}

/// Schnittmenge zweier Rechtecke (leer, wenn sie sich nicht berühren).
fn clip(r: &RECT, area: &RECT) -> RECT {
    RECT { left: r.left.max(area.left), top: r.top.max(area.top), right: r.right.min(area.right), bottom: r.bottom.min(area.bottom) }
}

/// Der Chat: eigene Fragen rechts als Blasen, Antworten links mit leichtem
/// Markdown. Scrollbar; folgt beim Streamen dem Ende, bis man selbst scrollt.
unsafe fn draw_answer(st: &mut State, dc: HDC, f: &Fonts, w: i32, top: i32, bottom: i32) {
    use crate::chat::{Ink, Item};
    let d = st.dpi;
    let has_turns = crate::ai::has_turns();
    let head = RECT { left: px(d, PAD + 12), top, right: w / 2, bottom: top + px(d, 28) };
    text(dc, t!("KI-Chat", "AI chat"), &head, f.bold, TEXT, LINE);
    let left = pill(st, dc, f, t!("Zurück", "Back"), "\u{E76B}", true, w - px(d, PAD), top, Act::ShowHome);
    if has_turns {
        pill(st, dc, f, t!("Neuer Chat", "New chat"), "\u{E710}", true, left - px(d, 8), top, Act::NewChat);
    }
    let area = RECT { left: px(d, PAD + 12), top: top + px(d, 44), right: w - px(d, PAD + 12), bottom };
    let view_h = area.bottom - area.top;
    st.list_h = view_h;
    if !has_turns {
        st.max_scroll = 0;
        st.scroll = 0;
        let r = RECT { bottom: area.top + px(d, 60), ..area };
        text(dc, t!("Stell oben im Feld eine Frage – Folgefragen kennen den bisherigen Verlauf.", "Ask a question in the field above – follow-ups know the conversation so far."), &r, f.normal, SUBTEXT, DT_CENTER | DT_WORDBREAK);
        return;
    }

    // Setzen nur, wenn sich Inhalt oder Breite geändert haben.
    let width = area.right - area.left;
    // Der Chat zählt jede Änderung mit – so muss nicht bei jedem Zeichnen
    // (z. B. Mausbewegung) der ganze Verlauf kopiert und gehasht werden.
    let key = hash_of(&format!("{width}:{d}:{}", crate::ai::revision()));
    let layout = match st.chat_layout.take() {
        Some((k, l)) if k == key => l,
        _ => {
            let answers = crate::ai::turns();
            let m = GdiMeasure::new(dc, f, d);
            let turns: Vec<crate::chat::Turn> = answers
                .iter()
                .map(|a| crate::chat::Turn { question: &a.question, steps: steps(a), text: &a.text, thinking: a.thinking, done: a.done, error: a.error.as_deref() })
                .collect();
            crate::chat::layout(&m, &turns, width, d)
        }
    };
    let lead = px(d, 4);

    st.max_scroll = (layout.height + px(d, 8) - view_h).max(0);
    if st.follow {
        st.scroll = st.max_scroll;
    }
    st.scroll = st.scroll.clamp(0, st.max_scroll);
    let (ox, oy) = (area.left, area.top - st.scroll);
    let now = Instant::now();
    let copied = st.copied.filter(|(_, t)| now.duration_since(*t) < Duration::from_millis(1500)).map(|(h, _)| h);

    let saved = SaveDC(dc);
    IntersectClipRect(dc, area.left, area.top, area.right, area.bottom);
    let panel = mix(BG, TEXT, 0.07);
    for item in &layout.items {
        let at = |r: &RECT| RECT { left: r.left + ox, top: r.top + oy, right: r.right + ox, bottom: r.bottom + oy };
        let r = match item {
            Item::Run(run) => at(&run.rect),
            Item::Bubble(r) | Item::CodePanel(r) | Item::Rule(r) | Item::Copy { rect: r, .. } | Item::Marker { rect: r, .. } => at(r),
        };
        if r.bottom < area.top || r.top > area.bottom {
            continue;
        }
        match item {
            Item::Bubble(_) => round(dc, &r, mix(BG, accent(), 0.28), px(d, 16)),
            Item::CodePanel(_) => {
                round(dc, &r, panel, px(d, 8));
                outline(dc, &r, BORDER, px(d, 8));
            }
            Item::Rule(_) => fill(dc, &r, if r.bottom - r.top > 1 { mix(BG, TEXT, 0.3) } else { BORDER }),
            Item::Marker { text: t, .. } => text(dc, t, &r, f.normal, SUBTEXT, DT_RIGHT | DT_VCENTER | DT_SINGLELINE),
            Item::Run(run) => {
                let m_font = match run.font {
                    crate::chat::Font::Normal => f.normal,
                    crate::chat::Font::Bold => f.strong,
                    crate::chat::Font::Heading => f.bold,
                    crate::chat::Font::Mono => f.mono,
                    crate::chat::Font::Small => f.small,
                };
                if run.code {
                    let bg = RECT { left: r.left - px(d, 4), top: r.top + lead / 2 - px(d, 1), right: r.right + px(d, 4), bottom: r.bottom - lead / 2 + px(d, 1) };
                    round(dc, &bg, mix(BG, TEXT, 0.12), px(d, 4));
                }
                let color = match &run.ink {
                    Ink::Text => TEXT,
                    Ink::Sub => SUBTEXT,
                    Ink::Error => rgb(255, 153, 164),
                    Ink::Link(_) => accent(),
                };
                text(dc, &run.text, &r, m_font, color, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP);
                if let Ink::Link(url) = &run.ink {
                    let vis = clip(&r, &area);
                    if inside(&vis, st.mouse.0, st.mouse.1) {
                        let base = r.bottom - lead / 2 - px(d, 2);
                        fill(dc, &RECT { left: r.left, top: base, right: r.right, bottom: base + px(d, 1).max(1) }, accent());
                    }
                    st.hits.push((vis, Act::OpenLink(url.clone())));
                }
            }
            Item::Copy { text: t, label, .. } => {
                let vis = clip(&r, &area);
                let done = copied == Some(hash_of(t));
                if inside(&vis, st.mouse.0, st.mouse.1) {
                    round(dc, &r, if *label { HOVER } else { ACTIVE }, px(d, 6));
                }
                let glyph = if done { "\u{E73E}" } else { "\u{E8C8}" };
                if *label {
                    text(dc, glyph, &RECT { left: r.left + px(d, 8), ..r }, f.icons_small, SUBTEXT, LINE);
                    text(dc, if done { t!("Kopiert", "Copied") } else { t!("Kopieren", "Copy") }, &RECT { left: r.left + px(d, 26), ..r }, f.small, SUBTEXT, LINE);
                } else {
                    text(dc, glyph, &r, f.icons_small, SUBTEXT, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                }
                st.hits.push((vis, Act::CopyText(t.clone())));
            }
        }
    }
    RestoreDC(dc, saved);

    // Schmale Bildlaufleiste, wenn der Chat länger ist als der Platz.
    if st.max_scroll > 0 {
        let content = view_h + st.max_scroll;
        let th = (view_h * view_h / content).max(px(d, 24));
        let ty = area.top + (view_h - th) * st.scroll / st.max_scroll;
        let x = area.right + px(d, 6);
        round(dc, &RECT { left: x, top: ty, right: x + px(d, 3), bottom: ty + th }, mix(BG, TEXT, 0.25), px(d, 3));
    }
    // Weggescrollt, während die Antwort noch läuft oder darunter mehr steht:
    // Knopf zurück ans Ende.
    if !st.follow && st.max_scroll - st.scroll > px(d, 40) {
        let s = px(d, 32);
        let r = RECT { left: (area.left + area.right - s) / 2, top: area.bottom - s - px(d, 4), right: (area.left + area.right + s) / 2, bottom: area.bottom - px(d, 4) };
        let hov = inside(&r, st.mouse.0, st.mouse.1);
        round(dc, &r, if hov { ACTIVE } else { HOVER }, s);
        outline(dc, &r, BORDER, s);
        text(dc, "\u{E74B}", &r, f.icons_small, TEXT, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        st.hits.push((r, Act::ScrollEnd));
    }
    st.chat_layout = Some((key, layout));
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
    let mut items = vec![Item::Cmd(1, if pinned { t!("Von Start lösen", "Unpin from Start") } else { t!("An Start anheften", "Pin to Start") })];
    if let Some(exe) = &exe {
        let on_bar = crate::bar::is_pinned(exe);
        items.push(Item::Cmd(2, if on_bar { t!("Von Taskleiste lösen", "Unpin from taskbar") } else { t!("An Taskleiste anheften", "Pin to taskbar") }));
        items.push(Item::Sep);
        items.push(Item::Cmd(3, t!("Als Administrator ausführen", "Run as administrator")));
        items.push(Item::Cmd(4, t!("Dateispeicherort öffnen", "Open file location")));
    }
    items.push(Item::Sep);
    items.push(Item::Cmd(5, t!("Deinstallieren", "Uninstall")));
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
            // Ein langsamer älterer Lauf überschreibt kein neueres Ergebnis.
            if r.as_ref().map_or(true, |(g, _, _)| *g < gen) {
                *r = Some((gen, query, found));
            }
        }
        PostMessageW(target as HWND, WM_FILES, gen as usize, 0);
    });
}

/// Index-Treffer übernehmen, wenn sie noch zur aktuellen Eingabe gehören.
unsafe fn apply_file_results(hwnd: HWND, gen: u64) {
    // Nur das Ergebnis zu dieser Nachricht abholen – ein neueres bleibt für
    // seine eigene Nachricht liegen.
    let Some((rgen, query, found)) = FILE_RESULTS.lock().ok().and_then(|mut r| if r.as_ref().is_some_and(|(g, _, _)| *g == gen) { r.take() } else { None }) else { return };
    let applied = with(|st| {
        if rgen != gen || gen != st.files_gen || query != st.query.trim() {
            return false;
        }
        st.files = found
            .into_iter()
            .map(|(name, path)| {
                let folder = Path::new(&path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                Entry { title: name, kind: t!("Datei", "File"), sub: folder, target: Target::Path(path), glyph: Glyph::File }
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

/// Suchtext bzw. Folgefrage ändern (Einfügen, Ausschneiden, Wort löschen)
/// und – in der Suche – neu suchen. `f` bekommt den Text und ob er komplett
/// markiert ist (Strg+A).
unsafe fn edit_query(hwnd: HWND, f: impl FnOnce(&mut String, bool)) {
    let chat = with(|st| {
        let all = st.select_all;
        f(field(st), all);
        st.select_all = false;
        if chat_input(st) {
            return true;
        }
        st.focus = Focus::Search;
        st.results = if st.query.is_empty() { Vec::new() } else { search(st) };
        st.sel = 0;
        st.scroll = 0;
        false
    })
    .unwrap_or(false);
    if !chat {
        SetTimer(hwnd, TIMER_FILES, 160, None);
    }
    InvalidateRect(hwnd, null(), 0);
}

thread_local! {
    /// Hohes Surrogat aus WM_CHAR, das auf sein Gegenstück wartet.
    static HIGH_SURROGATE: std::cell::Cell<Option<u16>> = const { std::cell::Cell::new(None) };
}

/// Eine UTF-16-Einheit aus WM_CHAR zum Zeichen machen. Ein hohes Surrogat
/// wird gemerkt und mit dem folgenden niedrigen verbunden; einzelne
/// Hälften fallen weg.
fn utf16_char(pending: &mut Option<u16>, unit: u16) -> Option<char> {
    if (0xD800..0xDC00).contains(&unit) {
        *pending = Some(unit);
        return None;
    }
    match pending.take() {
        Some(high) if (0xDC00..0xE000).contains(&unit) => char::decode_utf16([high, unit]).next()?.ok(),
        _ => char::decode_utf16([unit]).next()?.ok(),
    }
}

/// Höchstlänge beim Einfügen ins Such- bzw. Chatfeld (Zeichen).
const MAX_PASTE: usize = 4000;

/// Strg-Kürzel im Suchfeld wie in einem normalen Textfeld.
unsafe fn on_ctrl_key(hwnd: HWND, vk: u16) -> bool {
    const VK_A: u16 = b'A' as u16;
    const VK_C: u16 = b'C' as u16;
    const VK_V: u16 = b'V' as u16;
    const VK_X: u16 = b'X' as u16;
    match vk {
        VK_A => {
            with(|st| st.select_all = !field(st).is_empty());
            InvalidateRect(hwnd, null(), 0);
        }
        VK_C | VK_X => {
            let (q, chat) = with(|st| (field(st).clone(), chat_input(st))).unwrap_or_default();
            if !q.is_empty() {
                set_clipboard_text(hwnd, &q);
            } else if chat && vk == VK_C {
                // Leere Eingabe im Chat: die letzte Antwort kopieren.
                if let Some(a) = crate::ai::current().filter(|a| !a.text.trim().is_empty()) {
                    set_clipboard_text(hwnd, a.text.trim());
                }
            }
            if vk == VK_X {
                edit_query(hwnd, |q, _| q.clear());
            }
        }
        VK_V => {
            // Suche: nur die erste Zeile; Folgefrage: Zeilen mit Leerzeichen
            // verbinden. Ohne Steuerzeichen (Tabs, Zeilenumbrüche).
            let Some(t) = clipboard_text(hwnd) else { return true };
            let chat = with(|st| chat_input(st)).unwrap_or(false);
            let text = if chat { t.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ") } else { t.lines().next().unwrap_or("").to_string() };
            // Höchstens 4000 Zeichen (riesige Zwischenablage nicht ins Feld).
            let line: String = text.chars().filter(|c| !c.is_control()).take(MAX_PASTE).collect();
            edit_query(hwnd, |q, all| {
                if all {
                    q.clear();
                }
                q.push_str(line.trim());
                // Auch mehrfaches Einfügen bleibt begrenzt (auf Zeichengrenze).
                if let Some((i, _)) = q.char_indices().nth(MAX_PASTE) {
                    q.truncate(i);
                }
            });
        }
        // Strg+Rücktaste: letztes Wort löschen.
        VK_BACK => edit_query(hwnd, |q, all| {
            if all {
                q.clear();
                return;
            }
            let trimmed = q.trim_end().len();
            // Hinter dem Leerzeichen schneiden – es kann mehr als ein Byte lang sein.
            let cut = q[..trimmed].char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8());
            q.truncate(cut);
        }),
        // Strg+Pos1/Ende im Chat: an den Anfang bzw. ans Ende.
        VK_HOME | VK_END if with(|st| chat_input(st)).unwrap_or(false) => {
            with(|st| {
                st.follow = vk == VK_END;
                st.scroll = if vk == VK_END { st.max_scroll } else { 0 };
            });
            InvalidateRect(hwnd, null(), 0);
        }
        _ => return false,
    }
    true
}

/// Tasten im Chat: Enter schickt die Folgefrage, Escape leert erst die
/// Eingabe und geht dann zurück, Pfeile und Bild-Tasten scrollen.
fn on_chat_key(st: &mut State, vk: u16, send: &mut Option<String>) -> bool {
    let line = px(st.dpi, 40);
    let page = (st.list_h - line).max(line);
    let by = |st: &mut State, delta: i32| {
        st.scroll = (st.scroll + delta).clamp(0, st.max_scroll);
        st.follow = st.scroll >= st.max_scroll;
    };
    match vk {
        VK_ESCAPE => {
            if std::mem::take(&mut st.select_all) || !st.input.is_empty() {
                st.input.clear();
            } else {
                crate::ai::cancel();
                st.view = View::Home;
                st.scroll = 0;
                st.chat_layout = None;
            }
        }
        VK_BACK => {
            if std::mem::take(&mut st.select_all) {
                st.input.clear();
            } else {
                st.input.pop();
            }
        }
        VK_RETURN => {
            let q = st.input.trim().to_string();
            if !q.is_empty() {
                *send = Some(q);
            }
        }
        VK_UP => by(st, -line),
        VK_DOWN => by(st, line),
        VK_PRIOR => by(st, -page),
        VK_NEXT => by(st, page),
        _ => return false,
    }
    true
}

/// Tab-Reihenfolge der Abschnitte in der aktuellen Ansicht. Jeder Abschnitt
/// steht mit dem Element da, auf dem der Fokus beim Hineinspringen landet.
fn sections(st: &State) -> Vec<Focus> {
    let mut v = Vec::new();
    if !st.query.is_empty() {
        // Suche: Feld samt Trefferliste, dann die Fußleiste.
        v.push(if st.results.is_empty() { Focus::Search } else { Focus::Result(st.sel) });
    } else {
        v.push(Focus::Search);
        v.push(Focus::Pill);
        match st.view {
            View::All => {
                let n = item_count(&st.all_rows);
                if n > 0 {
                    v.push(Focus::List(st.last_list.min(n - 1)));
                }
            }
            _ => {
                if pinned_count(st) > 0 {
                    v.push(Focus::Pinned(0));
                }
                if rec_count(st) > 0 {
                    v.push(Focus::Rec(0));
                }
            }
        }
    }
    v.push(Focus::Account);
    v.push(Focus::Power);
    v
}

/// Gehören zwei Fokusziele zum selben Abschnitt?
fn same_section(a: Focus, b: Focus) -> bool {
    use Focus::*;
    match (a, b) {
        (Search | Result(_), Search | Result(_)) => true,
        (Pinned(_), Pinned(_)) | (Rec(_), Rec(_)) | (List(_), List(_)) => true,
        _ => a == b,
    }
}

fn pinned_count(st: &State) -> usize {
    st.pin_idx.len().min(COLS * PIN_ROWS)
}

/// Sichtbare Einträge unter „Empfohlen“ (so viele, wie zuletzt Platz hatten).
fn rec_count(st: &State) -> usize {
    st.nodes.iter().filter(|(f, _)| matches!(f, Focus::Rec(_))).count()
}

/// Neues Fokusziel für eine Navigationstaste, `None` = keine Navigationstaste
/// bzw. hier ohne Wirkung.
fn nav_target(st: &State, vk: u16, shift: bool) -> Option<Focus> {
    use Focus::*;
    if vk == VK_TAB {
        let secs = sections(st);
        let cur = secs.iter().position(|s| same_section(*s, st.focus)).unwrap_or(0);
        let n = secs.len();
        let next = if shift { (cur + n - 1) % n } else { (cur + 1) % n };
        return Some(secs[next]);
    }
    let dir = match vk {
        VK_LEFT => Dir::Left,
        VK_RIGHT => Dir::Right,
        VK_UP => Dir::Up,
        VK_DOWN => Dir::Down,
        VK_HOME | VK_END | VK_PRIOR | VK_NEXT => return page_target(st, vk),
        _ => return None,
    };
    // Suche: hoch/runter durch die Treffer; das Feld behält dabei die Eingabe.
    if !st.query.is_empty() {
        let count = st.results.len();
        return match (st.focus, dir) {
            (Search | Result(_), Dir::Down) if count > 0 => Some(Result((st.sel + 1).min(count - 1))),
            (Result(0), Dir::Up) => Some(Search),
            (Result(i), Dir::Up) => Some(Result(i - 1)),
            (Account, Dir::Right) => Some(Power),
            (Power, Dir::Left) => Some(Account),
            (Account | Power, Dir::Up) => Some(if count > 0 { Result(st.sel) } else { Search }),
            _ => None,
        };
    }
    let all = st.view == View::All;
    let (np, nr, nl) = (pinned_count(st), rec_count(st), item_count(&st.all_rows));
    // Erstes Element unter dem Suchfeld bzw. letztes über der Fußleiste.
    let first_body = if all {
        (nl > 0).then_some(List(0))
    } else if np > 0 {
        Some(Pinned(0))
    } else {
        (nr > 0).then_some(Rec(0))
    };
    let last_body = if all {
        (nl > 0).then_some(List(st.last_list.min(nl.saturating_sub(1))))
    } else if nr > 0 {
        Some(Rec(0))
    } else {
        (np > 0).then_some(Pinned(0))
    };
    match (st.focus, dir) {
        (Search, Dir::Down) => first_body,
        (Pill, Dir::Down) => first_body,
        (Pill, Dir::Up | Dir::Left) => Some(Search),
        (Pinned(i), d) => match grid_move(i, np, COLS, d) {
            Some(j) => Some(Pinned(j)),
            None => match d {
                Dir::Up => Some(Search),
                // Spalte ungefähr mitnehmen: oben 6 Spalten, unten 2.
                Dir::Down if nr > 0 => Some(Rec(((i % COLS) * 2 / COLS).min(nr - 1))),
                Dir::Down => Some(Account),
                _ => None,
            },
        },
        (Rec(i), d) => match grid_move(i, nr, 2, d) {
            Some(j) => Some(Rec(j)),
            None => match d {
                Dir::Up if np > 0 => {
                    let last_row = (np - 1) / COLS;
                    Some(Pinned((last_row * COLS + (i % 2) * COLS / 2).min(np - 1)))
                }
                Dir::Up => Some(Search),
                Dir::Down => Some(Account),
                _ => None,
            },
        },
        (List(i), Dir::Up) => Some(if i == 0 { Search } else { List(i - 1) }),
        (List(i), Dir::Down) => Some(if i + 1 < nl { List(i + 1) } else { Account }),
        (Account, Dir::Right) => Some(Power),
        (Power, Dir::Left) => Some(Account),
        (Account | Power, Dir::Up) => Some(last_body.unwrap_or(Search)),
        _ => None,
    }
}

/// Pos1/Ende/Bild auf/ab in „Alle Apps“.
fn page_target(st: &State, vk: u16) -> Option<Focus> {
    let Focus::List(i) = st.focus else { return None };
    let n = item_count(&st.all_rows);
    if n == 0 {
        return None;
    }
    let page = ((st.list_h / px(st.dpi, ROW_H)).max(2) - 1) as usize;
    Some(Focus::List(match vk {
        VK_HOME => 0,
        VK_END => n - 1,
        VK_PRIOR => i.saturating_sub(page),
        _ => (i + page).min(n - 1),
    }))
}

/// Fokus setzen und das Ziel sichtbar machen.
fn set_focus(st: &mut State, f: Focus) {
    st.focus = f;
    match f {
        Focus::Result(i) => {
            st.sel = i;
            scroll_into_view(st, false, i);
        }
        Focus::List(i) => {
            st.last_list = i;
            scroll_into_view(st, true, i);
        }
        _ => {}
    }
}

/// Fokus nach einem Neuladen gültig halten.
fn clamp_focus(st: &mut State) {
    let ok = match st.focus {
        Focus::Pinned(i) => i < pinned_count(st),
        Focus::Rec(i) => i < st.recent.len(),
        Focus::List(i) => i < item_count(&st.all_rows),
        Focus::Result(i) => i < st.results.len(),
        _ => true,
    };
    if !ok {
        st.focus = Focus::Search;
    }
    st.last_list = st.last_list.min(item_count(&st.all_rows).saturating_sub(1));
}

/// Ansicht „Alle Apps“ bzw. Startseite.
fn show_view(st: &mut State, view: View) {
    if view == View::Home && st.view == View::Answer {
        crate::ai::cancel();
        st.input.clear();
        st.chat_layout = None;
    }
    st.view = view;
    st.scroll = 0;
    st.last_list = 0;
}

/// Ziel eines Fokuselements (App, Datei, …), falls es eins öffnet.
fn focus_target(st: &State, f: Focus) -> Option<Target> {
    match f {
        Focus::Pinned(i) => st.pin_idx.get(i).map(|&a| Target::App(a)),
        Focus::Rec(i) => st.recent.get(i).map(|e| e.target.clone()),
        Focus::List(i) => nth_item(&st.all_rows, i).map(|e| e.target.clone()),
        Focus::Result(i) => nth_item(&st.results, i).map(|e| e.target.clone()),
        _ => None,
    }
}

/// Fokuselement auslösen – wie ein Klick darauf (Enter, Screenreader).
unsafe fn activate(hwnd: HWND, f: Focus) {
    if let Some(t) = with(|st| focus_target(st, f)).flatten() {
        open(t);
        return;
    }
    match f {
        Focus::Pill => {
            with(|st| {
                let to = if st.view == View::Home { View::All } else { View::Home };
                show_view(st, to);
                st.focus = Focus::Pill;
            });
            raise_focus(hwnd, Focus::Pill);
        }
        Focus::Account => {
            if menus::account_menu(hwnd) {
                hide();
            }
        }
        Focus::Power => {
            if menus::power_menu(hwnd) {
                hide();
            }
        }
        Focus::Search => {
            if let Some(t) = with(|st| nth_item(&st.results, st.sel).map(|e| e.target.clone())).flatten() {
                open(t);
            }
        }
        _ => {}
    }
    InvalidateRect(hwnd, null(), 0);
}

/// Fokuswechsel an Screenreader melden (außerhalb von `with`, weil UIA dabei
/// zurückfragen kann).
fn raise_focus(hwnd: HWND, f: Focus) {
    crate::uia::start_focus_changed(hwnd as isize, f.code());
}

unsafe fn on_key(hwnd: HWND, vk: u16) -> bool {
    if GetKeyState(VK_CONTROL as i32) < 0 && on_ctrl_key(hwnd, vk) {
        return true;
    }
    let shift = GetKeyState(VK_SHIFT as i32) < 0;
    let mut open_target = None;
    let mut close = false;
    let mut schedule_files = false;
    let mut send = None;
    let mut moved = None;
    let mut trigger = None;
    let handled = with(|st| {
        if chat_input(st) {
            return on_chat_key(st, vk, &mut send);
        }
        if let Some(f) = nav_target(st, vk, shift) {
            st.kb = true;
            if f != st.focus {
                set_focus(st, f);
                moved = Some(f);
            }
            return true;
        }
        match vk {
            VK_ESCAPE => {
                st.select_all = false;
                st.focus = Focus::Search;
                if st.query.is_empty() && st.view == View::Home {
                    close = true;
                } else {
                    if st.query.is_empty() && st.view == View::Answer {
                        crate::ai::cancel();
                    }
                    st.query.clear();
                    st.view = View::Home;
                    st.scroll = 0;
                }
            }
            VK_BACK => {
                if std::mem::take(&mut st.select_all) {
                    st.query.clear();
                } else {
                    st.query.pop();
                }
                st.results = if st.query.is_empty() { Vec::new() } else { search(st) };
                schedule_files = true;
                st.sel = 0;
                st.scroll = 0;
                st.focus = Focus::Search;
            }
            VK_RETURN => match st.focus {
                Focus::Search | Focus::Result(_) => {
                    if let Some(Row::Item(e)) = st.results.get(st.sel) {
                        open_target = Some(e.target.clone());
                    }
                }
                f => trigger = Some(f),
            },
            _ => return false,
        }
        true
    })
    .unwrap_or(false);
    if schedule_files {
        SetTimer(hwnd, TIMER_FILES, 160, None);
    }
    if let Some(f) = moved {
        raise_focus(hwnd, f);
    }
    if close {
        hide();
    } else if let Some(t) = open_target {
        open(t);
    } else if let Some(f) = trigger {
        activate(hwnd, f);
    } else if let Some(q) = send {
        ask(q);
    } else if handled {
        InvalidateRect(hwnd, null(), 0);
    }
    handled
}

/// App hinter dem fokussierten Element (für das Kontextmenü per Tastatur).
fn focused_app(st: &State) -> Option<usize> {
    let f = match st.focus {
        Focus::Search if !st.query.is_empty() => Focus::Result(st.sel),
        f => f,
    };
    match focus_target(st, f)? {
        Target::App(i) => Some(i),
        _ => None,
    }
}

// --------------------------------------------------- UI Automation

/// Rolle eines Elements für Screenreader.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Edit,
    ListItem,
    Button,
}

/// Ein Element des Startmenüs für UI Automation.
pub struct UiNode {
    pub code: usize,
    pub name: String,
    pub role: Role,
    /// Bildschirmkoordinaten.
    pub rect: RECT,
}

fn node_name(st: &State, f: Focus) -> String {
    let entry = |e: &Entry| if e.sub.is_empty() { e.title.clone() } else { format!("{}, {}", e.title, e.sub) };
    match f {
        Focus::Search => {
            if chat_input(st) {
                t!("Frage an die KI", "Ask the AI").to_string()
            } else {
                t!("Suchfeld", "Search box").to_string()
            }
        }
        Focus::Pill => if st.view == View::All { t!("Zurück", "Back") } else { t!("Alle Apps", "All apps") }.to_string(),
        Focus::Pinned(i) => st.pin_idx.get(i).and_then(|&a| st.apps.get(a)).map(|a| a.name.clone()).unwrap_or_default(),
        Focus::Rec(i) => st.recent.get(i).map(entry).unwrap_or_default(),
        Focus::List(i) => nth_item(&st.all_rows, i).map(entry).unwrap_or_default(),
        Focus::Result(i) => nth_item(&st.results, i).map(|e| if e.kind.is_empty() { entry(e) } else { format!("{}, {}", entry(e), e.kind) }).unwrap_or_default(),
        Focus::Account => {
            if crate::settings::get().presentation {
                t!("Benutzer", "User").to_string()
            } else {
                std::env::var("USERNAME").unwrap_or_default()
            }
        }
        Focus::Power => t!("Ein/Aus", "Power").to_string(),
    }
}

/// Alle sichtbaren Elemente in Lesereihenfolge (Stand des letzten Frames).
pub fn ui_nodes() -> Vec<UiNode> {
    let h = hwnd();
    if h.is_null() || unsafe { IsWindowVisible(h) } == 0 {
        return Vec::new();
    }
    let mut origin = POINT { x: 0, y: 0 };
    unsafe { ClientToScreen(h, &mut origin) };
    with(|st| {
        st.nodes
            .iter()
            .map(|&(f, r)| UiNode {
                code: f.code(),
                name: node_name(st, f),
                role: match f {
                    Focus::Search => Role::Edit,
                    Focus::Pill | Focus::Account | Focus::Power => Role::Button,
                    _ => Role::ListItem,
                },
                rect: RECT { left: r.left + origin.x, top: r.top + origin.y, right: r.right + origin.x, bottom: r.bottom + origin.y },
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Element mit Tastaturfokus (als Code), wenn Start sichtbar ist.
pub fn ui_focus() -> Option<usize> {
    let h = hwnd();
    if h.is_null() || unsafe { IsWindowVisible(h) } == 0 {
        return None;
    }
    with(|st| st.focus.code())
}

/// Fokus von außen setzen (Screenreader).
pub fn ui_set_focus(code: usize) {
    let Some(f) = Focus::from_code(code) else { return };
    let h = hwnd();
    with(|st| set_focus(st, f));
    unsafe { InvalidateRect(h, null(), 0) };
    raise_focus(h, f);
}

/// Element auslösen (Screenreader) – asynchron, damit der UIA-Aufruf nicht
/// in Menüs oder das Ausblenden hinein wartet.
pub fn ui_invoke(code: usize) {
    unsafe { PostMessageW(hwnd(), WM_UIA_INVOKE, code, 0) };
}

/// Text im Suchfeld (bzw. der Folgefrage im Chat).
pub fn ui_query() -> String {
    with(|st| if chat_input(st) { st.input.clone() } else { st.query.clone() }).unwrap_or_default()
}

/// Suchtext von außen setzen (Screenreader, ValuePattern).
pub fn ui_set_query(text: &str) {
    let line: String = text.chars().filter(|c| !c.is_control()).take(MAX_PASTE).collect();
    unsafe {
        edit_query(hwnd(), |q, _| {
            q.clear();
            q.push_str(&line);
        })
    };
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
        WM_TIMER if wp == TIMER_COPIED => {
            KillTimer(hwnd, TIMER_COPIED);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_SETCURSOR if (lp & 0xffff) as u32 == HTCLIENT => {
            // Hand über Links in Antworten.
            let mut p = POINT { x: 0, y: 0 };
            GetCursorPos(&mut p);
            ScreenToClient(hwnd, &mut p);
            if matches!(with(|st| hit(st, p.x, p.y)).flatten(), Some(Act::OpenLink(_))) {
                SetCursor(LoadCursorW(null_mut(), IDC_HAND));
                return 1;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_FILES => {
            apply_file_results(hwnd, wp as u64);
            0
        }
        WM_AI => {
            if with(|st| st.view == View::Answer).unwrap_or(false) {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_RELOAD => {
            reload(hwnd);
            0
        }
        WM_CATALOG => {
            with(|st| {
                st.checking = false;
                st.loaded_at = Instant::now();
            });
            if wp == 1 {
                reload(hwnd);
            }
            0
        }
        WM_SHELL_CHANGE => {
            // Installationen schreiben oft mehrere Verknüpfungen nacheinander.
            SetTimer(hwnd, TIMER_APPS, 1500, None);
            0
        }
        WM_TIMER if wp == TIMER_APPS => {
            KillTimer(hwnd, TIMER_APPS);
            if IsWindowVisible(hwnd) != 0 {
                check_catalog(hwnd);
            } else {
                reload(hwnd);
            }
            0
        }
        WM_UIA_INVOKE => {
            if let Some(f) = Focus::from_code(wp) {
                if IsWindowVisible(hwnd) != 0 {
                    activate(hwnd, f);
                }
            }
            0
        }
        // Kontextmenü per Tastatur (Menütaste, Umschalt+F10): für die App im Fokus.
        WM_CONTEXTMENU => {
            if lp as i32 == -1 {
                if let Some(i) = with(|st| focused_app(st)).flatten() {
                    app_menu(hwnd, i);
                }
            }
            0
        }
        // Screenreader fragen nach unserem UI-Automation-Provider.
        WM_GETOBJECT if lp as i32 == windows::Win32::UI::Accessibility::UiaRootObjectId => {
            use windows::Win32::Foundation as wf;
            let root = crate::uia::start_root(hwnd as isize);
            windows::Win32::UI::Accessibility::UiaReturnRawElementProvider(wf::HWND(hwnd as _), wf::WPARAM(wp), wf::LPARAM(lp), &root).0
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
                st.follow = st.scroll >= st.max_scroll;
            });
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(lp);
            // Mausbedienung: Fokusrahmen wieder aus, wie bei Windows.
            with(|st| st.kb = false);
            match with(|st| hit(st, x, y)).flatten() {
                Some(Act::Open(t)) => open(t),
                Some(Act::ShowAll) => {
                    with(|st| {
                        show_view(st, View::All);
                        st.focus = Focus::Search;
                    });
                }
                Some(Act::ShowHome) => {
                    with(|st| {
                        show_view(st, View::Home);
                        st.focus = Focus::Search;
                    });
                }
                Some(Act::NewChat) => {
                    with(new_chat);
                }
                Some(Act::ScrollEnd) => {
                    with(|st| {
                        st.follow = true;
                        st.scroll = st.max_scroll;
                    });
                }
                Some(Act::OpenLink(url)) => {
                    if url.starts_with("https://") || url.starts_with("http://") {
                        open(Target::Uri(url));
                    }
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
                Some(Act::CopyText(t)) => {
                    set_clipboard_text(hwnd, &t);
                    with(|st| st.copied = Some((hash_of(&t), Instant::now())));
                    SetTimer(hwnd, TIMER_COPIED, 1600, None);
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
            // Emoji (Win+.) kommen als zwei WM_CHAR mit Surrogat-Hälften.
            let c = if wp > 0xFFFF {
                char::from_u32(wp as u32)
            } else {
                HIGH_SURROGATE.with(|p| {
                    let mut pending = p.get();
                    let c = utf16_char(&mut pending, wp as u16);
                    p.set(pending);
                    c
                })
            };
            if let Some(c) = c.filter(|c| !c.is_control()) {
                let chat = with(|st| {
                    // Nach Strg+A ersetzt das erste Zeichen die markierte Eingabe.
                    if std::mem::take(&mut st.select_all) {
                        field(st).clear();
                    }
                    field(st).push(c);
                    if chat_input(st) {
                        return true;
                    }
                    // Tippen geht immer in die Suche, egal wo der Fokus stand.
                    st.focus = Focus::Search;
                    st.results = search(st);
                    st.sel = 0;
                    st.scroll = 0;
                    false
                })
                .unwrap_or(false);
                if chat {
                    InvalidateRect(hwnd, null(), 0);
                    return 0;
                }
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
    let mut st = State::new(apps, pins, load_launches());
    rebuild(&mut st);
    STATE.with(|s| *s.borrow_mut() = Some(st));
    HWND_START.store(hwnd, Ordering::Relaxed);
    watch_start_folders(hwnd);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surrogate_pairs_make_emoji() {
        let mut p = None;
        assert_eq!(utf16_char(&mut p, 'a' as u16), Some('a'));
        let mut buf = [0u16; 2];
        let units = '😀'.encode_utf16(&mut buf);
        assert_eq!(utf16_char(&mut p, units[0]), None);
        assert_eq!(utf16_char(&mut p, units[1]), Some('😀'));
        assert_eq!(p, None);
        // Einzelne Hälften fallen weg, das nächste normale Zeichen kommt durch.
        assert_eq!(utf16_char(&mut p, 0xDC00), None);
        assert_eq!(utf16_char(&mut p, 0xD83D), None);
        assert_eq!(utf16_char(&mut p, 'b' as u16), Some('b'));
        assert_eq!(p, None);
    }

    /// Chat-Layout mit echten GDI-Schriften: alles bleibt in der Breite,
    /// Zeilen überlappen nicht.
    #[test]
    fn chat_layout_with_gdi_fonts() {
        unsafe {
            let f = Fonts::new(144);
            let dc = CreateCompatibleDC(null_mut());
            let text = "## Titel\nEin **fetter** Satz mit `code` und https://example.com/ein/sehr/langer/pfad/der/umbrechen/muss/weil/er/nicht/passt.\n\n- eins\n- zwei\n\n```\nfn main() { println!(\"hallo\"); }\n```";
            let turn = crate::chat::Turn { question: "Frage?", steps: &[], text, thinking: false, done: true, error: None };
            let width = px(144, 560);
            let l = {
                let m = GdiMeasure::new(dc, &f, 144);
                crate::chat::layout(&m, &[turn], width, 144)
            };
            assert!(l.height > px(144, 200));
            let mut last_top = i32::MIN;
            for item in &l.items {
                if let crate::chat::Item::Run(r) = item {
                    assert!(r.rect.left >= 0 && r.rect.right <= width, "{} ragt hinaus", r.text);
                    assert!(r.rect.bottom > r.rect.top);
                    assert!(r.rect.top >= last_top.saturating_sub(px(144, 30)));
                    last_top = r.rect.top;
                }
            }
            DeleteDC(dc);
            f.delete();
        }
    }

    // ------------------------------------------------ Standardbelegung

    fn pins_for(names: &[(&str, &str)]) -> Vec<String> {
        default_pins(names.iter().map(|&(n, id)| (n, id)))
    }

    #[test]
    fn default_pins_match_whole_words() {
        // "word" darf nicht 1Password oder WordPad treffen, "editor" nicht
        // den Registrierungs-Editor.
        let pins = pins_for(&[("1Password", "pw"), ("WordPad", "wp"), ("Registrierungs-Editor", "reg"), ("Registry Editor", "reg-en")]);
        assert!(pins.is_empty(), "{pins:?}");
        let pins = pins_for(&[("1Password", "pw"), ("Word", "word"), ("Registry Editor", "reg"), ("Editor", "notepad")]);
        assert_eq!(pins, vec!["notepad", "word"]);
    }

    #[test]
    fn default_pins_know_english_names() {
        let pins = pins_for(&[
            ("Calculator", "calc"),
            ("Photos", "photos"),
            ("Notepad", "notepad"),
            ("Settings", "settings"),
            ("Microsoft Edge", "edge"),
            ("File Explorer", "explorer"),
            ("Internet Explorer", "ie"),
            ("Word", "word"),
            ("Microsoft Store", "store"),
        ]);
        assert_eq!(pins, vec!["edge", "explorer", "settings", "store", "photos", "calc", "notepad", "word"]);
    }

    #[test]
    fn default_pins_prefer_exact_names() {
        // Genauer Name vor längeren Varianten; Deinstallierer nie.
        let pins = pins_for(&[("Paint 3D", "p3d"), ("Paint", "paint"), ("Steam uninstall", "su"), ("Outlook (new)", "outlook")]);
        assert_eq!(pins, vec!["paint", "outlook"]);
        assert_eq!(pin_match("Datei-Explorer", "datei-explorer"), Some(0));
        assert_eq!(pin_match("Microsoft Edge WebView2", "microsoft edge"), Some(1));
        assert_eq!(pin_match("Microsoft Edge", "edge"), None);
    }

    // ------------------------------------------------------- Suche

    fn hit(title: &str, quality: u32, category: u32, launches: u32) -> Hit {
        let entry = Entry { title: title.into(), kind: "", sub: String::new(), target: Target::Uri(String::new()), glyph: Glyph::App };
        Hit { quality, category, launches, entry }
    }

    #[test]
    fn apps_match_exe_and_id() {
        // "notepad" findet den deutschen „Editor“ über den exe-Namen bzw. die Store-Kennung.
        assert_eq!(app_quality("notepad", "editor", Some("notepad"), "windowsnotepad"), Some(0));
        assert_eq!(app_quality("notepad", "editor", None, "windowsnotepad"), Some(2));
        // "cmd" findet die Eingabeaufforderung (Kennung "{GUID}\cmd.exe").
        assert_eq!(app_quality("cmd", "eingabeaufforderung", None, "cmd"), Some(2));
        assert_eq!(app_quality("cmd", "eingabeaufforderung", Some("cmd"), "cmd"), Some(0));
        // Name schlägt Zusatzschlüssel; kurze Anfragen nur über den Namen.
        assert_eq!(app_quality("ed", "editor", Some("notepad"), "windowsnotepad"), Some(0));
        assert_eq!(app_quality("n", "editor", Some("notepad"), "windowsnotepad"), None);
        assert_eq!(app_quality("xyz", "editor", Some("notepad"), "windowsnotepad"), None);
    }

    #[test]
    fn settings_keywords_in_both_languages() {
        let find = |q: &str| -> Vec<&str> { SETTINGS.iter().filter(|(de, en, _, k)| settings_quality(q, de, en, k).is_some()).map(|s| s.1).collect() };
        assert!(find("wlan").contains(&"Wi-Fi"));
        assert!(find("wifi").contains(&"Wi-Fi"));
        assert!(find("anzeige").contains(&"Display"));
        assert!(find("display").contains(&"Display"));
        assert!(find("dunkel").contains(&"Colors"));
        assert!(find("dark").contains(&"Colors"));
        assert!(find("drucker").contains(&"Printers & scanners"));
        assert!(find("printer").contains(&"Printers & scanners"));
        assert_eq!(settings_quality("blue", "Bluetooth & Geräte", "Bluetooth & devices", "bluetooth"), Some(0));
    }

    #[test]
    fn launches_break_ties() {
        let mut hits = vec![hit("Visual Studio", 0, 0, 1), hit("Visual Studio Code", 0, 0, 9), hit("Vivaldi", 1, 0, 50), hit("Video", 0, 1, 0)];
        sort_hits(&mut hits);
        let order: Vec<&str> = hits.iter().map(|h| h.entry.title.as_str()).collect();
        // Häufiger vor kürzer – aber nie über eine bessere Trefferqualität hinweg.
        assert_eq!(order, vec!["Visual Studio Code", "Visual Studio", "Video", "Vivaldi"]);
        // Ohne Starts entscheidet wie bisher der kürzere Titel.
        let mut hits = vec![hit("Paint 3D", 0, 0, 0), hit("Paint", 0, 0, 0)];
        sort_hits(&mut hits);
        assert_eq!(hits[0].entry.title, "Paint");
    }

    #[test]
    fn launches_file_round_trip() {
        let mut m = HashMap::new();
        m.insert(r"{6D809377-6AF0-444B-8957-A3773F02200E}\app.exe".to_string(), 3);
        m.insert("Microsoft.WindowsNotepad_8wekyb3d8bbwe!App".to_string(), 12);
        let text = format_launches(&m);
        assert!(text.starts_with("Microsoft.WindowsNotepad"));
        assert_eq!(parse_launches(&text), m);
        // Kaputte Zeilen werden übersprungen.
        assert_eq!(parse_launches("ohne-tab\n\tleer\nx\tabc\ny\t2\n").len(), 1);
    }

    // ---------------------------------------------------- Tastatur

    #[test]
    fn grid_moves_like_windows() {
        // 8 Elemente in 6 Spalten: Zeile 0 = 0..6, Zeile 1 = 6..8.
        assert_eq!(grid_move(5, 8, 6, Dir::Right), Some(6));
        assert_eq!(grid_move(6, 8, 6, Dir::Left), Some(5));
        assert_eq!(grid_move(0, 8, 6, Dir::Left), None);
        assert_eq!(grid_move(7, 8, 6, Dir::Right), None);
        assert_eq!(grid_move(1, 8, 6, Dir::Down), Some(7));
        assert_eq!(grid_move(4, 8, 6, Dir::Down), Some(7));
        assert_eq!(grid_move(7, 8, 6, Dir::Down), None);
        assert_eq!(grid_move(7, 8, 6, Dir::Up), Some(1));
        assert_eq!(grid_move(3, 8, 6, Dir::Up), None);
        assert_eq!(grid_move(0, 0, 6, Dir::Down), None);
    }

    #[test]
    fn focus_codes_round_trip() {
        for f in [Focus::Search, Focus::Pill, Focus::Pinned(17), Focus::Rec(3), Focus::List(412), Focus::Result(0), Focus::Account, Focus::Power] {
            assert_eq!(Focus::from_code(f.code()), Some(f));
        }
        assert_eq!(Focus::from_code(99 << 24), None);
    }

    fn row(t: &str) -> Row {
        Row::Item(Entry { title: t.into(), kind: "", sub: String::new(), target: Target::Uri(String::new()), glyph: Glyph::App })
    }

    #[test]
    fn list_spans_count_headers() {
        let rows = vec![Row::Header("A".into()), row("Alarm"), row("Atlas"), Row::Header("B".into()), row("Bing")];
        assert_eq!(item_count(&rows), 3);
        assert_eq!(nth_item(&rows, 2).map(|e| e.title.as_str()), Some("Bing"));
        assert_eq!(item_span(&rows, 96, 0), Some((HEAD_H, ROW_H)));
        assert_eq!(item_span(&rows, 96, 2), Some((2 * HEAD_H + 2 * ROW_H, ROW_H)));
        assert_eq!(item_span(&rows, 96, 3), None);
    }

    /// Startseite mit 8 angehefteten Apps und 3 Empfehlungen.
    fn home_state() -> State {
        let mut st = State::new(Vec::new(), Vec::new(), HashMap::new());
        st.dpi = 96;
        st.pin_idx = (0..8).collect();
        st.nodes = (0..3).map(|i| (Focus::Rec(i), RECT { left: 0, top: 0, right: 1, bottom: 1 })).collect();
        st
    }

    #[test]
    fn tab_cycles_sections() {
        let mut st = home_state();
        let mut seen = Vec::new();
        for _ in 0..7 {
            let f = nav_target(&st, VK_TAB, false).unwrap();
            seen.push(f);
            st.focus = f;
        }
        assert_eq!(seen, vec![Focus::Pill, Focus::Pinned(0), Focus::Rec(0), Focus::Account, Focus::Power, Focus::Search, Focus::Pill]);
        // Umschalt+Tab rückwärts, auch mitten aus einem Abschnitt heraus.
        st.focus = Focus::Pinned(5);
        assert_eq!(nav_target(&st, VK_TAB, true), Some(Focus::Pill));
        st.focus = Focus::Search;
        assert_eq!(nav_target(&st, VK_TAB, true), Some(Focus::Power));
    }

    #[test]
    fn arrows_cross_sections() {
        let mut st = home_state();
        let go = |st: &mut State, vk: u16| {
            let f = nav_target(st, vk, false);
            if let Some(f) = f {
                st.focus = f;
            }
            f
        };
        assert_eq!(go(&mut st, VK_DOWN), Some(Focus::Pinned(0)));
        assert_eq!(go(&mut st, VK_RIGHT), Some(Focus::Pinned(1)));
        assert_eq!(go(&mut st, VK_DOWN), Some(Focus::Pinned(7)));
        // Spalte 2 von 6 liegt in der linken Hälfte → linke Empfehlung.
        assert_eq!(go(&mut st, VK_DOWN), Some(Focus::Rec(0)));
        assert_eq!(go(&mut st, VK_DOWN), Some(Focus::Rec(2)));
        assert_eq!(go(&mut st, VK_DOWN), Some(Focus::Account));
        assert_eq!(go(&mut st, VK_RIGHT), Some(Focus::Power));
        assert_eq!(go(&mut st, VK_RIGHT), None);
        assert_eq!(go(&mut st, VK_UP), Some(Focus::Rec(0)));
        assert_eq!(go(&mut st, VK_UP), Some(Focus::Pinned(6)));
        assert_eq!(go(&mut st, VK_UP), Some(Focus::Pinned(0)));
        assert_eq!(go(&mut st, VK_UP), Some(Focus::Search));
        // Pfeile links/rechts im Suchfeld tun nichts.
        assert_eq!(go(&mut st, VK_LEFT), None);
    }

    #[test]
    fn arrows_in_search_results() {
        let mut st = home_state();
        st.query = "pa".into();
        st.results = vec![row("Paint"), row("Paint 3D")];
        assert_eq!(nav_target(&st, VK_DOWN, false), Some(Focus::Result(1)));
        st.focus = Focus::Result(1);
        st.sel = 1;
        assert_eq!(nav_target(&st, VK_DOWN, false), Some(Focus::Result(1)));
        assert_eq!(nav_target(&st, VK_UP, false), Some(Focus::Result(0)));
        // Tab: Treffer → Konto → Ein/Aus → Treffer.
        assert_eq!(nav_target(&st, VK_TAB, false), Some(Focus::Account));
        st.focus = Focus::Power;
        assert_eq!(nav_target(&st, VK_TAB, false), Some(Focus::Result(1)));
    }

    #[test]
    fn all_apps_list_navigation() {
        let mut st = home_state();
        st.view = View::All;
        st.all_rows = vec![Row::Header("A".into()), row("Alarm"), row("Atlas"), Row::Header("B".into()), row("Bing")];
        st.list_h = 2 * ROW_H;
        assert_eq!(nav_target(&st, VK_DOWN, false), Some(Focus::List(0)));
        st.focus = Focus::List(0);
        assert_eq!(nav_target(&st, VK_UP, false), Some(Focus::Search));
        assert_eq!(nav_target(&st, VK_END, false), Some(Focus::List(2)));
        set_focus(&mut st, Focus::List(2));
        // Zeile 2 liegt bei 2 Überschriften + 2 Zeilen; ganz sichtbar halten.
        assert_eq!(st.scroll, 2 * HEAD_H + 3 * ROW_H - 2 * ROW_H);
        assert_eq!(nav_target(&st, VK_DOWN, false), Some(Focus::Account));
        st.focus = Focus::Account;
        assert_eq!(nav_target(&st, VK_UP, false), Some(Focus::List(2)));
    }

    #[test]
    fn catalog_signature_ignores_order() {
        let a = [("Paint", "p"), ("Word", "w")];
        let b = [("Word", "w"), ("Paint", "p")];
        assert_eq!(catalog_sig(a.iter().copied()), catalog_sig(b.iter().copied()));
        let c = [("Word", "w"), ("Paint", "p"), ("Neu", "n")];
        assert_ne!(catalog_sig(a.iter().copied()), catalog_sig(c.iter().copied()));
    }
}

#[cfg(test)]
mod render {
    use super::*;

    /// Zeichnet das Startmenü mit einem Beispiel-Chat nach `LIGHTBAR_SHOT`
    /// (BMP). Nur von Hand: `LIGHTBAR_SHOT=chat.bmp cargo test render -- --ignored`
    #[test]
    #[ignore]
    fn chat_screenshot() {
        let Ok(path) = std::env::var("LIGHTBAR_SHOT") else { return };
        let q1 = crate::ai::Answer { question: "Wie liste ich Dateien in PowerShell?".into(), text: "## Dateien auflisten\nMit **Get-ChildItem** (kurz `ls`):\n\n```powershell\nGet-ChildItem -Recurse -Filter *.rs | Select-Object FullName\n```\n\n- `-Recurse` geht in Unterordner\n- `-Filter` grenzt ein\n  - auch mit Platzhaltern\n1. erstens\n2. zweitens\n\nMehr unter https://learn.microsoft.com/powershell.".into(), done: true, ..Default::default() };
        let q2 = crate::ai::Answer { question: "Und versteckte Dateien?".into(), text: "Dafür gibt es den Schalter `-Force`, der auch versteckte und Systemdateien".into(), ..Default::default() };
        crate::ai::set_for_test(vec![q1], Some(q2));
        unsafe {
            let dpi = 120;
            let (w, h) = (px(dpi, W), px(dpi, H));
            let screen = GetDC(null_mut());
            let dc = CreateCompatibleDC(screen);
            let bmp = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(dc, bmp);
            let mut st = State {
                dpi,
                fonts: Some(Fonts::new(dpi)),
                view: View::Answer,
                input: "Geht das auch rek".into(),
                ..State::new(Vec::new(), Vec::new(), HashMap::new())
            };
            draw(&mut st, dc, w, h);
            if let Ok(s) = std::env::var("LIGHTBAR_SCROLL") {
                st.follow = false;
                st.scroll = s.parse().unwrap_or(0);
                draw(&mut st, dc, w, h);
            }
            // 24-bit-BMP schreiben.
            let row = ((w * 3 + 3) & !3) as usize;
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 24;
            let mut pixels = vec![0u8; row * h as usize];
            SelectObject(dc, old);
            GetDIBits(dc, bmp, 0, h as u32, pixels.as_mut_ptr() as _, &mut bi, DIB_RGB_COLORS);
            let mut out = Vec::new();
            out.extend_from_slice(b"BM");
            out.extend_from_slice(&((54 + pixels.len()) as u32).to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&54u32.to_le_bytes());
            out.extend_from_slice(&40u32.to_le_bytes());
            out.extend_from_slice(&w.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(&1u16.to_le_bytes());
            out.extend_from_slice(&24u16.to_le_bytes());
            out.extend_from_slice(&[0; 24]);
            out.extend_from_slice(&pixels);
            std::fs::write(path, out).unwrap();
            DeleteObject(bmp);
            DeleteDC(dc);
            ReleaseDC(null_mut(), screen);
        }
    }
}

/// Sprache umgeschaltet: zwischengespeicherte Texte (Zeilen, Kopfzeilen) neu
/// aufbauen. Wird von der Leiste nach `i18n::apply` aufgerufen.
pub fn language_changed() {
    let h = hwnd();
    if h.is_null() {
        return;
    }
    // Zuletzt verwendete Dateien tragen Art und Alter als fertigen Text.
    let recent = load_recent();
    with(|st| {
        st.recent = recent;
        for e in &mut st.files {
            e.kind = t!("Datei", "File");
        }
        rebuild(st);
        // Trefferliste mit neuen Namen/Arten (Einstellungen, „… fragen“, Web).
        if !st.query.is_empty() {
            let sel = st.sel;
            st.results = search(st);
            st.sel = sel.min(st.results.len().saturating_sub(1));
        }
        // Gemerktes Chat-Layout beim nächsten Zeichnen neu setzen.
        st.chat_layout = None;
    });
    unsafe { InvalidateRect(h, null(), 0) };
}
