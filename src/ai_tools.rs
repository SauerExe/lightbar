//! Werkzeuge für die KI (OpenAI-Function-Calling): Apps und Webseiten öffnen,
//! Dateien suchen, Einstellungen öffnen, Systeminfos lesen, Lautstärke,
//! Fenster, Zwischenablage.
//!
//! Bewusst nur harmlose, sichtbare Aktionen: keine Befehlszeile, kein Löschen
//! oder Schreiben von Dateien, keine simulierten Tastendrücke. Ausführbare
//! Dateien werden nicht gestartet, sondern nur im Explorer gezeigt.
//!
//! Ablauf: `parse` macht aus Name + JSON-Argumenten einen `Call` (prüft dabei
//! alles), `Call::step` liefert die Zeile fürs Startmenü, `Call::run` führt aus.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};
use windows_sys::Win32::Foundation::{HWND, LPARAM, BOOL};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::util::shell_open;

/// Testlauf (für --ai-test): Werkzeuge, die etwas verändern, melden nur, was
/// sie täten.
static DRY_RUN: AtomicBool = AtomicBool::new(false);

pub fn set_dry_run(on: bool) {
    DRY_RUN.store(on, Ordering::Relaxed);
}

/// Beschreibung aller Werkzeuge im OpenAI-Format (`tools` der Anfrage).
pub fn schema() -> Value {
    let f = |name: &str, desc: &str, params: Value| json!({ "type": "function", "function": { "name": name, "description": desc, "parameters": params } });
    let none = json!({ "type": "object", "properties": {} });
    let one = |key: &str, kind: &str, desc: &str| json!({ "type": "object", "properties": { key: { "type": kind, "description": desc } }, "required": [key] });
    json!([
        f("open_app", t!("Startet eine installierte App (aus dem Startmenü-Katalog) per Name, z. B. \"Rechner\", \"Edge\", \"Spotify\".", "Launches an installed app (from the Start menu catalog) by name, e.g. \"Calculator\", \"Edge\", \"Spotify\"."), one("name", "string", t!("Name der App", "Name of the app"))),
        f("open_url", t!("Öffnet eine Webseite im Standardbrowser. Nur http:// und https://.", "Opens a website in the default browser. Only http:// and https://."), one("url", "string", t!("Vollständige Adresse mit http:// oder https://", "Full address with http:// or https://"))),
        f("search_files", t!("Sucht Dateien und Ordner im Benutzerordner über den Windows-Suchindex (nach Dateinamen). Liefert Namen und Pfade.", "Searches files and folders in the user folder via the Windows search index (by file name). Returns names and paths."), one("query", "string", t!("Suchbegriff (Teil des Dateinamens)", "Search term (part of the file name)"))),
        f("open_path", t!("Öffnet eine Datei oder einen Ordner (absoluter Pfad, z. B. aus search_files). Programme werden nicht gestartet, sondern im Explorer gezeigt.", "Opens a file or folder (absolute path, e.g. from search_files). Programs are not started but shown in Explorer."), one("path", "string", t!("Absoluter Pfad", "Absolute path"))),
        f("open_settings", t!("Öffnet eine Seite der Windows-Einstellungen, z. B. \"wlan\", \"bluetooth\", \"anzeige\", \"sound\", \"akku\", \"update\", \"taskleiste\".", "Opens a page of Windows Settings, e.g. \"wifi\", \"bluetooth\", \"display\", \"sound\", \"battery\", \"update\", \"taskbar\"."), one("page", "string", t!("Thema oder ms-settings:-Adresse", "Topic or ms-settings: address"))),
        f("get_system_info", t!("Aktuelle Uhrzeit und Datum, Akkustand, Lautstärke, Netzwerk und freier Speicherplatz der Laufwerke.", "Current time and date, battery level, volume, network and free disk space of the drives."), none.clone()),
        f("set_volume", t!("Stellt die Systemlautstärke ein (0–100 %).", "Sets the system volume (0-100 %)."), one("percent", "integer", t!("Lautstärke in Prozent, 0–100", "Volume in percent, 0-100"))),
        f("mute", t!("Schaltet den Ton stumm oder wieder an.", "Mutes or unmutes the sound."), json!({ "type": "object", "properties": { "on": { "type": "boolean", "description": t!("true = stumm, false = Ton wieder an (Standard: true)", "true = mute, false = unmute (default: true)") } } })),
        f("list_windows", t!("Listet die offenen Fenster (Titel und Programm).", "Lists the open windows (title and program)."), none.clone()),
        f("focus_window", t!("Holt ein offenes Fenster in den Vordergrund (Teil des Titels oder Programmname genügt).", "Brings an open window to the front (part of the title or the program name is enough)."), one("title", "string", t!("Teil des Fenstertitels oder Programmname", "Part of the window title or program name"))),
        f("clipboard_get", t!("Liest den Text in der Zwischenablage. Nur benutzen, wenn der Nutzer danach fragt.", "Reads the text in the clipboard. Only use when the user asks for it."), none),
        f("clipboard_set", t!("Legt Text in die Zwischenablage (zum Einfügen mit Strg+V).", "Puts text on the clipboard (to paste with Ctrl+V)."), one("text", "string", t!("Der Text", "The text"))),
    ])
}

/// Ein geprüfter Werkzeug-Aufruf.
#[derive(Debug, PartialEq)]
pub enum Call {
    OpenApp(String),
    OpenUrl(String),
    SearchFiles(String),
    OpenPath(String),
    /// Anzeigename und URI der Einstellungsseite.
    OpenSettings(String, String),
    SystemInfo,
    SetVolume(u8),
    Mute(bool),
    ListWindows,
    FocusWindow(String),
    ClipboardGet,
    ClipboardSet(String),
}

/// Name + Argumente (JSON-Text, wie vom Modell geliefert) prüfen.
pub fn parse(name: &str, args: &str) -> Result<Call, String> {
    let args = args.trim();
    let v: Value = if args.is_empty() { json!({}) } else { serde_json::from_str(args).map_err(|_| t!("Argumente sind kein gültiges JSON", "Arguments are not valid JSON").to_string())? };
    let text = |key: &str| -> Result<String, String> {
        let s = v[key].as_str().map(str::trim).unwrap_or("");
        if s.is_empty() {
            return Err(tf!("Argument \"{key}\" fehlt", "Argument \"{key}\" missing"));
        }
        Ok(s.to_string())
    };
    Ok(match name {
        "open_app" => Call::OpenApp(text("name")?),
        "open_url" => Call::OpenUrl(check_url(&text("url")?)?),
        "search_files" => Call::SearchFiles(text("query")?),
        "open_path" => Call::OpenPath(check_path(&text("path")?)?),
        "open_settings" => {
            let (name, uri) = settings_page(&text("page")?)?;
            Call::OpenSettings(name, uri)
        }
        "get_system_info" => Call::SystemInfo,
        "set_volume" => {
            // Zahl oder Text ("30", "30%"); Werte wie 0.3 als Anteil deuten.
            let n = match &v["percent"] {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().trim_end_matches('%').trim().parse::<f64>().ok(),
                _ => None,
            }
            .ok_or(t!("Argument \"percent\" fehlt", "Argument \"percent\" missing"))?;
            let n = if n > 0.0 && n < 1.0 { n * 100.0 } else { n };
            if !(0.0..=100.0).contains(&n) {
                return Err(t!("Lautstärke muss zwischen 0 und 100 liegen", "Volume must be between 0 and 100").into());
            }
            Call::SetVolume(n.round() as u8)
        }
        "mute" => Call::Mute(v["on"].as_bool().unwrap_or(true)),
        "list_windows" => Call::ListWindows,
        "focus_window" => Call::FocusWindow(text("title")?),
        "clipboard_get" => Call::ClipboardGet,
        "clipboard_set" => {
            let s = v["text"].as_str().ok_or(t!("Argument \"text\" fehlt", "Argument \"text\" missing"))?;
            if s.chars().count() > 20_000 {
                return Err(t!("Text ist zu lang", "Text is too long").into());
            }
            Call::ClipboardSet(s.to_string())
        }
        _ => return Err(tf!("Unbekanntes Werkzeug \"{name}\"", "Unknown tool \"{name}\"")),
    })
}

/// Nur http(s), keine Leer- oder Steuerzeichen, kein Benutzer:Passwort@.
fn check_url(url: &str) -> Result<String, String> {
    let lower = url.to_ascii_lowercase();
    let rest = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")).ok_or(t!("Nur http:// und https:// sind erlaubt", "Only http:// and https:// are allowed"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || host.contains('@') || url.len() > 2048 || url.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"') {
        return Err(t!("Ungültige Adresse", "Invalid address").into());
    }
    Ok(url.to_string())
}

/// Absoluter lokaler Pfad, der existiert. Netzwerkpfade (\\server) nicht –
/// die würden ungefragt Anmeldedaten an fremde Rechner schicken.
fn check_path(path: &str) -> Result<String, String> {
    let path = path.trim_matches('"');
    let b = path.as_bytes();
    let absolute = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    if !absolute || path.contains(['"', '*', '?', '<', '>', '|']) {
        return Err(t!("Nur absolute lokale Pfade (z. B. C:\\…) sind erlaubt", "Only absolute local paths (e.g. C:\\...) are allowed").into());
    }
    Ok(path.to_string())
}

/// Dateiendungen, die etwas ausführen würden – solche Dateien nur zeigen.
fn is_executable(path: &str) -> bool {
    const EXT: [&str; 26] = [
        "exe", "com", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "msi", "msp", "scr", "pif",
        "cpl", "msc", "hta", "reg", "lnk", "url", "jar", "py", "pyw", "appref-ms",
    ];
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    EXT.contains(&ext.as_str())
}

/// Einstellungsseite aus der Tabelle des Startmenüs. Direkte
/// `ms-settings:`-Adressen gehen auch, solange sie nur aus Buchstaben,
/// Ziffern und Bindestrichen bestehen.
fn settings_page(page: &str) -> Result<(String, String), String> {
    // Name in der Oberflächensprache; gesucht wird in beiden Sprachen.
    let local = |de: &str, en: &str| if crate::i18n::en() { en.to_string() } else { de.to_string() };
    let p = page.trim().to_lowercase();
    if let Some(rest) = p.strip_prefix("ms-settings:") {
        if rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            let name = crate::start::SETTINGS.iter().find(|(_, _, u, _)| *u == p).map_or(p.clone(), |(de, en, ..)| local(de, en));
            return Ok((name, p));
        }
        return Err(t!("Ungültige Einstellungsseite", "Invalid settings page").into());
    }
    let words: Vec<&str> = p.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 2).collect();
    // Beste Zeile: Name enthält den ganzen Begriff, sonst die meisten Stichwort-Treffer.
    let mut best: Option<(usize, &(&str, &str, &str, &str))> = None;
    for s in crate::start::SETTINGS {
        let (de, en, _, keys) = s;
        let names = [de.to_lowercase(), en.to_lowercase()];
        let mut score = if names.iter().any(|n| *n == p) {
            100
        } else if !p.is_empty() && names.iter().any(|n| n.contains(&p)) {
            50
        } else {
            0
        };
        score += words.iter().filter(|w| keys.split(' ').any(|k| k.starts_with(**w) || w.starts_with(k)) || names.iter().any(|n| n.contains(**w))).count();
        if score > 0 && best.is_none_or(|(b, _)| score > b) {
            best = Some((score, s));
        }
    }
    best.map(|(_, (de, en, u, _))| (local(de, en), u.to_string())).ok_or_else(|| tf!("Keine Einstellungsseite zu \"{page}\" gefunden", "No settings page found for \"{page}\""))
}

impl Call {
    /// Ändert etwas am PC (im Testlauf nicht ausführen).
    fn changes_state(&self) -> bool {
        !matches!(self, Call::SearchFiles(_) | Call::SystemInfo | Call::ListWindows | Call::ClipboardGet)
    }

    /// Kurze Zeile fürs Startmenü.
    pub fn step(&self) -> String {
        let short = |s: &str| if s.chars().count() > 60 { format!("{} …", s.chars().take(60).collect::<String>()) } else { s.to_string() };
        match self {
            Call::OpenApp(n) => tf!("Öffne {} …", "Opening {} ...", short(n)),
            Call::OpenUrl(u) => tf!("Öffne {} …", "Opening {} ...", short(u)),
            Call::SearchFiles(q) => tf!("Suche Dateien: {}", "Searching files: {}", short(q)),
            Call::OpenPath(p) => tf!("Öffne {} …", "Opening {} ...", short(p)),
            Call::OpenSettings(n, _) => tf!("Öffne Einstellungen: {n}", "Opening settings: {n}"),
            Call::SystemInfo => t!("Lese Systeminfos …", "Reading system info ...").into(),
            Call::SetVolume(p) => tf!("Lautstärke auf {p} %", "Volume to {p} %"),
            Call::Mute(true) => t!("Ton aus", "Sound off").into(),
            Call::Mute(false) => t!("Ton an", "Sound on").into(),
            Call::ListWindows => t!("Sehe offene Fenster an …", "Looking at open windows ...").into(),
            Call::FocusWindow(t) => tf!("Wechsle zu {} …", "Switching to {} ...", short(t)),
            Call::ClipboardGet => t!("Lese Zwischenablage …", "Reading clipboard ...").into(),
            Call::ClipboardSet(_) => t!("Kopiere in die Zwischenablage …", "Copying to the clipboard ...").into(),
        }
    }

    /// Ausführen; Ergebnis als Text fürs Modell. Braucht ein COM-Apartment
    /// im aufrufenden Thread (App-Katalog, Suchindex, Audio).
    pub fn run(&self) -> Result<String, String> {
        if self.changes_state() && DRY_RUN.load(Ordering::Relaxed) {
            return Ok(tf!("(Testlauf – nicht ausgeführt: {})", "(Dry run - not executed: {})", self.step()));
        }
        unsafe {
            match self {
                Call::OpenApp(name) => open_app(name),
                Call::OpenUrl(url) => {
                    shell_open(url, "", SW_SHOWNORMAL);
                    Ok(tf!("Geöffnet: {url}", "Opened: {url}"))
                }
                Call::SearchFiles(q) => {
                    let hits = crate::apps::search_index_checked(q, 10)?;
                    if hits.is_empty() {
                        return Ok(tf!("Keine Dateien zu \"{q}\" gefunden.", "No files found for \"{q}\"."));
                    }
                    Ok(hits.iter().map(|(n, p)| format!("{n} | {p}")).collect::<Vec<_>>().join("\n"))
                }
                Call::OpenPath(p) => {
                    let path = std::path::Path::new(p);
                    if !path.exists() {
                        return Err(tf!("Pfad nicht gefunden: {p}", "Path not found: {p}"));
                    }
                    if path.is_file() && is_executable(p) {
                        shell_open("explorer.exe", &format!("/select,\"{p}\""), SW_SHOWNORMAL);
                        return Ok(tf!("Programm/Skript wird nicht gestartet, nur im Explorer gezeigt: {p}", "Program/script is not started, only shown in Explorer: {p}"));
                    }
                    shell_open(p, "", SW_SHOWNORMAL);
                    Ok(tf!("Geöffnet: {p}", "Opened: {p}"))
                }
                Call::OpenSettings(name, uri) => {
                    shell_open(uri, "", SW_SHOWNORMAL);
                    Ok(tf!("Einstellungen geöffnet: {name}", "Settings opened: {name}"))
                }
                Call::SystemInfo => Ok(system_info()),
                Call::SetVolume(p) => {
                    crate::status::set_volume(*p as f32 / 100.0);
                    Ok(tf!("Lautstärke ist jetzt {p} %", "Volume is now {p} %"))
                }
                Call::Mute(on) => {
                    crate::status::set_mute(*on);
                    Ok(if *on { t!("Ton ist aus", "Sound is off").into() } else { t!("Ton ist an", "Sound is on").into() })
                }
                Call::ListWindows => {
                    let list = windows();
                    if list.is_empty() {
                        return Ok(t!("Keine offenen Fenster.", "No open windows.").into());
                    }
                    Ok(list.iter().map(|w| format!("{} | {}", w.title, w.exe)).collect::<Vec<_>>().join("\n"))
                }
                Call::FocusWindow(t) => focus_window(t),
                Call::ClipboardGet => match crate::util::clipboard_text(null_mut()) {
                    Some(s) if !s.is_empty() => Ok(s.chars().take(4000).collect()),
                    _ => Ok(t!("Die Zwischenablage enthält keinen Text.", "The clipboard contains no text.").into()),
                },
                Call::ClipboardSet(s) => {
                    crate::util::set_clipboard_text(null_mut(), s);
                    Ok(t!("Text liegt in der Zwischenablage.", "Text is on the clipboard.").into())
                }
            }
        }
    }
}

/// App per Name: genauer Treffer vor Anfang vor Teilwort; Deinstallierer nie.
unsafe fn open_app(name: &str) -> Result<String, String> {
    let apps = crate::apps::enumerate();
    let q = name.to_lowercase();
    let usable = |a: &&crate::apps::App| !a.lower.contains("uninstall") && !a.lower.contains("deinstall");
    let found = apps
        .iter()
        .filter(usable)
        .find(|a| a.lower == q)
        .or_else(|| apps.iter().filter(usable).find(|a| a.lower.starts_with(&q)))
        .or_else(|| apps.iter().filter(usable).find(|a| a.lower.contains(&q)));
    match found {
        Some(a) => {
            crate::apps::launch(&a.id);
            Ok(tf!("Gestartet: {}", "Started: {}", a.name))
        }
        None => Err(tf!("Keine App \"{name}\" gefunden", "No app \"{name}\" found")),
    }
}

fn system_info() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut out = Vec::new();
    let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    let days: [&str; 7] = if crate::i18n::en() {
        ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"]
    } else {
        ["Sonntag", "Montag", "Dienstag", "Mittwoch", "Donnerstag", "Freitag", "Samstag"]
    };
    let (h, mi, wd, d, mo, y) = (t.wHour, t.wMinute, days[t.wDayOfWeek as usize % 7], t.wDay, t.wMonth, t.wYear);
    out.push(tf!("Zeit: {h:02}:{mi:02} Uhr, {wd}, {d:02}.{mo:02}.{y}", "Time: {h:02}:{mi:02}, {wd}, {y}-{mo:02}-{d:02}"));
    let s = crate::status::poll();
    out.push(match s.battery {
        Some((p, true)) => tf!("Akku: {p} %, lädt", "Battery: {p} %, charging"),
        Some((p, false)) => tf!("Akku: {p} %, Akkubetrieb", "Battery: {p} %, on battery"),
        None => t!("Akku: keiner (Netzbetrieb)", "Battery: none (mains power)").into(),
    });
    out.push(tf!("Lautstärke: {} %{}", "Volume: {} %{}", (s.volume * 100.0).round(), if s.muted { t!(", stumm", ", muted") } else { "" }));
    out.push(match s.net {
        crate::status::Net::Wifi(q, _) => tf!("Netzwerk: WLAN, Signal {q} %", "Network: Wi-Fi, signal {q} %"),
        crate::status::Net::Wired => t!("Netzwerk: verbunden (Kabel)", "Network: connected (cable)").into(),
        crate::status::Net::Offline => t!("Netzwerk: offline", "Network: offline").into(),
    });
    out.extend(disks());
    out.join("\n")
}

/// Freier Platz aller festen Laufwerke.
fn disks() -> Vec<String> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives};
    const DRIVE_FIXED: u32 = 3;
    let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let root = format!("{}:\\", (b'A' + i as u8) as char);
        let h = HSTRING::from(root.as_str());
        if unsafe { GetDriveTypeW(&h) } != DRIVE_FIXED {
            continue;
        }
        let (mut free, mut total) = (0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(&h, None, Some(&mut total), Some(&mut free)) }.is_ok() && total > 0 {
            out.push(tf!("Laufwerk {root}: {:.1} GB von {:.1} GB frei", "Drive {root}: {:.1} GB of {:.1} GB free", gb(free), gb(total)));
        }
    }
    out
}

struct Win {
    hwnd: HWND,
    title: String,
    exe: String,
}

/// Fenster wie in der Taskleiste (sichtbar, nicht versteckt, mit Titel).
unsafe fn windows() -> Vec<Win> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        if crate::bar::is_task_window(h) {
            (*(lp as *mut Vec<HWND>)).push(h);
        }
        1
    }
    let mut hs: Vec<HWND> = Vec::new();
    EnumWindows(Some(cb), &mut hs as *mut _ as LPARAM);
    hs.into_iter()
        .take(40)
        .map(|h| {
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
            let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
            let exe = crate::bar::exe_of(h).and_then(|p| p.rsplit('\\').next().map(String::from)).unwrap_or_default();
            Win { hwnd: h, title, exe }
        })
        .collect()
}

/// Fenster nach Titel oder Programmname in den Vordergrund holen – ohne
/// simulierte Tastendrücke (anders als util::force_foreground).
unsafe fn focus_window(query: &str) -> Result<String, String> {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    let q = query.to_lowercase();
    let list = windows();
    let w = list
        .iter()
        .find(|w| w.title.to_lowercase() == q)
        .or_else(|| list.iter().find(|w| w.title.to_lowercase().contains(&q)))
        .or_else(|| list.iter().find(|w| w.exe.to_lowercase().trim_end_matches(".exe") == q.trim_end_matches(".exe")))
        .ok_or_else(|| tf!("Kein offenes Fenster \"{query}\"", "No open window \"{query}\""))?;
    if IsIconic(w.hwnd) != 0 {
        ShowWindow(w.hwnd, SW_RESTORE);
    }
    if SetForegroundWindow(w.hwnd) == 0 {
        let fg = GetWindowThreadProcessId(GetForegroundWindow(), null_mut());
        let me = GetCurrentThreadId();
        if fg != 0 && fg != me && windows_sys::Win32::System::Threading::AttachThreadInput(me, fg, 1) != 0 {
            SetForegroundWindow(w.hwnd);
            let _ = SetFocus(w.hwnd);
            windows_sys::Win32::System::Threading::AttachThreadInput(me, fg, 0);
        }
    }
    Ok(tf!("Im Vordergrund: {}", "In front: {}", w.title))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_lists_every_tool_and_parses() {
        let s = schema();
        let names: Vec<&str> = s.as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 12);
        // Jedes Werkzeug aus dem Schema muss parse kennen (Fehler wegen fehlender
        // Argumente ja, "unbekannt" nein).
        for n in names {
            if let Err(e) = parse(n, "{}") {
                assert!(!e.contains("Unbekannt") && !e.contains("Unknown"), "{n}: {e}");
            }
        }
    }

    #[test]
    fn parses_arguments() {
        assert_eq!(parse("open_app", r#"{"name":" Rechner "}"#), Ok(Call::OpenApp("Rechner".into())));
        assert_eq!(parse("search_files", r#"{"query":"lightbar"}"#), Ok(Call::SearchFiles("lightbar".into())));
        assert_eq!(parse("get_system_info", ""), Ok(Call::SystemInfo));
        assert_eq!(parse("list_windows", "{}"), Ok(Call::ListWindows));
        assert_eq!(parse("focus_window", r#"{"title":"Editor"}"#), Ok(Call::FocusWindow("Editor".into())));
        assert_eq!(parse("clipboard_set", r#"{"text":"Hallo"}"#), Ok(Call::ClipboardSet("Hallo".into())));
        assert_eq!(parse("clipboard_get", "{}"), Ok(Call::ClipboardGet));
        assert!(parse("open_app", "{}").is_err());
        assert!(parse("open_app", "{kaputt").is_err());
        let e = parse("run_shell", r#"{"cmd":"del"}"#).unwrap_err();
        assert!(e.contains("Unbekannt") || e.contains("Unknown"));
    }

    #[test]
    fn volume_and_mute() {
        assert_eq!(parse("set_volume", r#"{"percent":30}"#), Ok(Call::SetVolume(30)));
        assert_eq!(parse("set_volume", r#"{"percent":"45 %"}"#), Ok(Call::SetVolume(45)));
        assert_eq!(parse("set_volume", r#"{"percent":0.5}"#), Ok(Call::SetVolume(50)));
        assert_eq!(parse("set_volume", r#"{"percent":0}"#), Ok(Call::SetVolume(0)));
        assert!(parse("set_volume", r#"{"percent":150}"#).is_err());
        assert!(parse("set_volume", r#"{"percent":-5}"#).is_err());
        assert!(parse("set_volume", "{}").is_err());
        assert_eq!(parse("mute", "{}"), Ok(Call::Mute(true)));
        assert_eq!(parse("mute", r#"{"on":false}"#), Ok(Call::Mute(false)));
    }

    #[test]
    fn urls_only_http() {
        assert_eq!(parse("open_url", r#"{"url":"https://example.com/a?b=1"}"#), Ok(Call::OpenUrl("https://example.com/a?b=1".into())));
        assert!(parse("open_url", r#"{"url":"HTTP://example.com"}"#).is_ok());
        for bad in ["file:///C:/Windows/System32/calc.exe", "javascript:alert(1)", "ms-settings:", "https://", "https://a b.com", "https://user:pw@evil.com", "\\\\server\\share", "cmd.exe"] {
            assert!(parse("open_url", &json!({ "url": bad }).to_string()).is_err(), "{bad}");
        }
    }

    #[test]
    fn paths_local_and_executables_only_shown() {
        assert!(parse("open_path", r#"{"path":"C:\\Users\\Timo\\Dokumente"}"#).is_ok());
        assert!(parse("open_path", r#"{"path":"\\\\server\\share\\x.txt"}"#).is_err());
        assert!(parse("open_path", r#"{"path":"relativ\\x.txt"}"#).is_err());
        assert!(parse("open_path", r#"{"path":"shell:startup"}"#).is_err());
        assert!(is_executable("C:\\x\\setup.EXE"));
        assert!(is_executable("C:\\x\\run.ps1"));
        assert!(is_executable("C:\\x\\link.lnk"));
        assert!(!is_executable("C:\\x\\bericht.pdf"));
        assert!(!is_executable("C:\\x\\Ordner"));
    }

    #[test]
    fn settings_pages() {
        let page = |p: &str| match parse("open_settings", &json!({ "page": p }).to_string()) {
            Ok(Call::OpenSettings(_, uri)) => uri,
            other => panic!("{p}: {other:?}"),
        };
        assert_eq!(page("WLAN"), "ms-settings:network-wifi");
        assert_eq!(page("bluetooth"), "ms-settings:bluetooth");
        assert_eq!(page("Akku"), "ms-settings:powersleep");
        assert_eq!(page("Sound"), "ms-settings:sound");
        assert_eq!(page("Windows Update"), "ms-settings:windowsupdate");
        assert_eq!(page("ms-settings:display"), "ms-settings:display");
        assert!(parse("open_settings", r#"{"page":"ms-settings:x&calc"}"#).is_err());
        assert!(parse("open_settings", r#"{"page":"xyzzy"}"#).is_err());
    }

    #[test]
    fn steps_and_dry_run() {
        assert_eq!(Call::OpenApp("Rechner".into()).step(), t!("Öffne Rechner …", "Opening Rechner ..."));
        assert_eq!(Call::SearchFiles("lightbar".into()).step(), t!("Suche Dateien: lightbar", "Searching files: lightbar"));
        assert_eq!(Call::SetVolume(30).step(), t!("Lautstärke auf 30 %", "Volume to 30 %"));
        assert!(Call::OpenApp("x".into()).changes_state());
        assert!(Call::ClipboardSet("x".into()).changes_state());
        assert!(!Call::SystemInfo.changes_state());
        // Im Testlauf passiert bei verändernden Werkzeugen nichts.
        set_dry_run(true);
        for c in [Call::OpenApp("Rechner".into()), Call::SetVolume(10), Call::Mute(true), Call::FocusWindow("x".into()), Call::ClipboardSet("x".into()), Call::OpenUrl("https://example.com".into())] {
            let r = c.run().unwrap();
            assert!(r.starts_with("(Testlauf") || r.starts_with("(Dry run"));
        }
    }
}
