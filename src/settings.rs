//! Nutzer-Einstellungen in `%APPDATA%\Lightbar\settings.txt` (key=value).
//! Ersetzt die früheren Marker-Dateien `mono-tray` und `no-glow`, die beim
//! ersten Laden übernommen und entfernt werden.
//!
//! Geschrieben wird atomar (`util::write_atomic`). Unbekannte Zeilen (von
//! einer neueren oder älteren Version) bleiben beim Speichern erhalten, damit
//! ein Wechsel der Version keine Einstellungen verliert.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GlowStyle {
    Off,
    Aurora,
    Accent,
    Neon,
}

impl GlowStyle {
    fn key(self) -> &'static str {
        match self {
            GlowStyle::Off => "off",
            GlowStyle::Aurora => "aurora",
            GlowStyle::Accent => "accent",
            GlowStyle::Neon => "neon",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        [GlowStyle::Off, GlowStyle::Aurora, GlowStyle::Accent, GlowStyle::Neon].into_iter().find(|g| g.key() == s)
    }
}

/// Startsymbol. Standard ist der eigene Orb: Das Windows-Logo ist eine Marke
/// von Microsoft und soll nicht ungefragt erscheinen.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StartLogo {
    Orb,
    Windows,
    Tux,
}

impl StartLogo {
    fn key(self) -> &'static str {
        match self {
            StartLogo::Orb => "orb",
            StartLogo::Windows => "windows",
            StartLogo::Tux => "tux",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        [StartLogo::Orb, StartLogo::Windows, StartLogo::Tux].into_iter().find(|l| l.key() == s)
    }
}

/// Größe der Leiste. `Auto` folgt Windows ("Kleinere Taskleistenschaltflächen"
/// → klein, sonst normal); eine eigene Wahl übersteuert die Windows-Einstellung.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BarSize {
    Auto,
    Small,
    Normal,
    Large,
    XLarge,
}

impl BarSize {
    /// Alle festen Größen (ohne `Auto`), von klein nach groß.
    pub const ALL: [BarSize; 4] = [BarSize::Small, BarSize::Normal, BarSize::Large, BarSize::XLarge];

    pub fn key(self) -> &'static str {
        match self {
            BarSize::Auto => "auto",
            BarSize::Small => "small",
            BarSize::Normal => "normal",
            BarSize::Large => "large",
            BarSize::XLarge => "xlarge",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        [BarSize::Auto, BarSize::Small, BarSize::Normal, BarSize::Large, BarSize::XLarge].into_iter().find(|b| b.key() == s)
    }
    /// Tatsächliche Größe: `Auto` wird je nach Windows-Einstellung klein oder normal.
    pub fn resolve(self, windows_small: bool) -> BarSize {
        match self {
            BarSize::Auto if windows_small => BarSize::Small,
            BarSize::Auto => BarSize::Normal,
            s => s,
        }
    }
}

/// Was die Uhr zeigt (ohne eigenes Format, siehe `clock_format`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClockDisplay {
    /// Uhrzeit und kurzes Datum, wie Windows.
    DateTime,
    TimeOnly,
    /// Uhrzeit, darunter Wochentag und Datum ("Do. 22.10.2026").
    WeekdayDate,
    /// Uhrzeit und Datum ohne Jahr ("22.10.").
    ShortDate,
}

impl ClockDisplay {
    fn key(self) -> &'static str {
        match self {
            ClockDisplay::DateTime => "datetime",
            ClockDisplay::TimeOnly => "time",
            ClockDisplay::WeekdayDate => "weekday",
            ClockDisplay::ShortDate => "short",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        [ClockDisplay::DateTime, ClockDisplay::TimeOnly, ClockDisplay::WeekdayDate, ClockDisplay::ShortDate].into_iter().find(|c| c.key() == s)
    }
}

/// Schrift für die Texte der Leiste (Uhr, Eingabesprache, Usage, Suche).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BarFont {
    SegoeVariable,
    Segoe,
    Bahnschrift,
    CascadiaMono,
    Consolas,
}

impl BarFont {
    pub const ALL: [BarFont; 5] = [BarFont::SegoeVariable, BarFont::Segoe, BarFont::Bahnschrift, BarFont::CascadiaMono, BarFont::Consolas];

    fn key(self) -> &'static str {
        match self {
            BarFont::SegoeVariable => "segoe-variable",
            BarFont::Segoe => "segoe",
            BarFont::Bahnschrift => "bahnschrift",
            BarFont::CascadiaMono => "cascadia-mono",
            BarFont::Consolas => "consolas",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.key() == s)
    }
    /// Name im Menü.
    pub fn label(self) -> &'static str {
        match self {
            BarFont::SegoeVariable => "Segoe UI Variable",
            BarFont::Segoe => "Segoe UI",
            BarFont::Bahnschrift => "Bahnschrift",
            BarFont::CascadiaMono => "Cascadia Mono",
            BarFont::Consolas => "Consolas",
        }
    }
    /// GDI-Familienname. Segoe UI Variable heißt dort nach ihrer optischen
    /// Größe; für Text in 12–16 px passt "Text".
    pub fn face(self) -> &'static str {
        match self {
            BarFont::SegoeVariable => "Segoe UI Variable Text",
            f => f.label(),
        }
    }
}

#[derive(Clone)]
pub struct Settings {
    pub glow: GlowStyle,
    pub mono_tray: bool,
    pub clock_seconds: bool,
    pub show_usage: bool,
    /// Je Dienst abschaltbar (z. B. Claude dauerhaft gedrosselt, Codex weiter zeigen).
    pub usage_claude: bool,
    pub usage_codex: bool,
    pub start_logo: StartLogo,
    /// Präsentationsmodus (Streamen, Bildschirm teilen): keine persönlichen
    /// Daten – Name, zuletzt verwendete Dateien, Dateitreffer, Usage.
    pub presentation: bool,
    /// Kontobild statt Anfangsbuchstabe im Startmenü.
    pub start_avatar: bool,
    /// Sprache der Oberfläche (auto = Windows-Anzeigesprache).
    pub language: crate::i18n::LangSetting,
    /// Eingabesprache ("DEU"/"ENG") anzeigen, wenn mehrere Layouts installiert sind.
    pub input_lang: bool,
    /// Suchfeld in der Leiste. Standard aus – auch wenn Windows es zeigt
    /// (stört meist nur, Win-Taste + Tippen sucht ja schon).
    pub taskbar_search: bool,
    /// Kalenderwochen im Kalender. Standard aus, wie bei Windows.
    pub week_numbers: bool,
    /// Größe der Leiste (Dicke, Symbole, Buttons, Schrift).
    pub bar_size: BarSize,
    pub clock_display: ClockDisplay,
    /// Eigenes Uhrformat (leer = Regionalformat), nur in settings.txt
    /// einstellbar: Bildmuster von GetTimeFormatEx/GetDateFormatEx, zwei
    /// Zeilen durch `|` getrennt, z. B. `HH:mm|ddd dd.MM.`. Ohne `|` gilt das
    /// Muster für die Uhrzeit, das Datum folgt `clock_display`; nichts nach
    /// dem `|` = keine Datumszeile.
    pub clock_format: String,
    pub bar_font: BarFont,
    /// Uhrzeit fett.
    pub clock_bold: bool,
}

// Usage-Anzeige standardmäßig aus: Sie liest lokale Logins und nutzt nicht
// offiziell dokumentierte Schnittstellen – das soll man bewusst einschalten.
const DEFAULTS: Settings = Settings { glow: GlowStyle::Aurora, mono_tray: false, clock_seconds: false, show_usage: false, usage_claude: true, usage_codex: true, start_logo: StartLogo::Orb, presentation: false, start_avatar: true, language: crate::i18n::LangSetting::Auto, input_lang: true, taskbar_search: false, week_numbers: false, bar_size: BarSize::Auto, clock_display: ClockDisplay::DateTime, clock_format: String::new(), bar_font: BarFont::SegoeVariable, clock_bold: false };

static CURRENT: Mutex<Option<Settings>> = Mutex::new(None);
/// Zeilen aus settings.txt, die diese Version nicht kennt. Beim Speichern
/// werden sie unverändert wieder angehängt.
static EXTRA: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Das Laden ist gescheitert (Datei da, aber nicht lesbar): dann nicht
/// überschreiben, sonst wären die gespeicherten Einstellungen still weg.
static LOAD_FAILED: AtomicBool = AtomicBool::new(false);

/// Schlüssel, die diese Version kennt (auch frühere, die beim Speichern entfallen).
const KNOWN: &[&str] = &[
    "glow",
    "mono_tray",
    "clock_seconds",
    "show_usage",
    "usage_claude",
    "usage_codex",
    "start_logo",
    "windows_logo",
    "presentation",
    "start_avatar",
    "language",
    "input_lang",
    "taskbar_search",
    "week_numbers",
    "bar_size",
    "clock_display",
    "clock_format",
    "bar_font",
    "clock_bold",
];

fn dir() -> Option<PathBuf> {
    crate::util::data_dir()
}

/// Inhalt von settings.txt auswerten. Zurück kommen die Einstellungen und die
/// unbekannten `key=value`-Zeilen (unverändert, je Schlüssel die erste).
fn parse(text: &str) -> (Settings, Vec<String>) {
    let mut s = DEFAULTS;
    let mut extra: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let on = v == "1" || v == "true";
        match k {
            "glow" => s.glow = GlowStyle::parse(v).unwrap_or(s.glow),
            "mono_tray" => s.mono_tray = on,
            "clock_seconds" => s.clock_seconds = on,
            "show_usage" => s.show_usage = on,
            "usage_claude" => s.usage_claude = on,
            "usage_codex" => s.usage_codex = on,
            "start_logo" => s.start_logo = StartLogo::parse(v).unwrap_or(s.start_logo),
            // Frühere Ja/Nein-Einstellung (bis 0.2.2).
            "windows_logo" if on => s.start_logo = StartLogo::Windows,
            "presentation" => s.presentation = on,
            "start_avatar" => s.start_avatar = on,
            "language" => s.language = crate::i18n::LangSetting::parse(v).unwrap_or(s.language),
            "input_lang" => s.input_lang = on,
            "taskbar_search" => s.taskbar_search = on,
            "week_numbers" => s.week_numbers = on,
            "bar_size" => s.bar_size = BarSize::parse(v).unwrap_or(s.bar_size),
            "clock_display" => s.clock_display = ClockDisplay::parse(v).unwrap_or(s.clock_display),
            "clock_format" => s.clock_format = v.to_string(),
            "bar_font" => s.bar_font = BarFont::parse(v).unwrap_or(s.bar_font),
            "clock_bold" => s.clock_bold = on,
            _ => {
                let seen = extra.iter().any(|l| l.split_once('=').is_some_and(|(e, _)| e.trim() == k));
                if !k.is_empty() && !KNOWN.contains(&k) && !seen {
                    extra.push(line.trim_end().to_string());
                }
            }
        }
    }
    (s, extra)
}

/// Text für settings.txt: bekannte Einstellungen, danach die unbekannten Zeilen.
fn serialize(s: &Settings, extra: &[String]) -> String {
    let b = |v: bool| if v { "1" } else { "0" };
    let mut text = format!(
        "glow={}\nmono_tray={}\nclock_seconds={}\nshow_usage={}\nusage_claude={}\nusage_codex={}\nstart_logo={}\npresentation={}\nstart_avatar={}\nlanguage={}\ninput_lang={}\ntaskbar_search={}\nweek_numbers={}\nbar_size={}\nclock_display={}\nclock_format={}\nbar_font={}\nclock_bold={}\n",
        s.glow.key(),
        b(s.mono_tray),
        b(s.clock_seconds),
        b(s.show_usage),
        b(s.usage_claude),
        b(s.usage_codex),
        s.start_logo.key(),
        b(s.presentation),
        b(s.start_avatar),
        s.language.key(),
        b(s.input_lang),
        b(s.taskbar_search),
        b(s.week_numbers),
        s.bar_size.key(),
        s.clock_display.key(),
        // Ein Zeilenumbruch würde die Datei zerlegen.
        s.clock_format.replace(['\r', '\n'], ""),
        s.bar_font.key(),
        b(s.clock_bold)
    );
    for line in extra {
        text.push_str(line);
        text.push('\n');
    }
    text
}

fn load() -> Settings {
    let Some(dir) = dir() else { return DEFAULTS };
    let file = dir.join("settings.txt");
    // Kurz gesperrt (Virenscanner, Synchronisierung)? Ein paarmal versuchen.
    // Kaputte Zeichen nach einer Handbearbeitung kosten nur diese Zeile.
    let mut read = std::fs::read(&file);
    for _ in 0..5 {
        match &read {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                std::thread::sleep(std::time::Duration::from_millis(200));
                read = std::fs::read(&file);
            }
            _ => break,
        }
    }
    match read {
        Ok(bytes) => {
            let (s, extra) = parse(&String::from_utf8_lossy(&bytes));
            if let Ok(mut e) = EXTRA.lock() {
                *e = extra;
            }
            LOAD_FAILED.store(false, Ordering::Relaxed);
            remember_stamp();
            s
        }
        // Nicht lesbar: mit Standardwerten laufen, die Datei aber nicht
        // überschreiben (sonst wäre z. B. der Präsentationsmodus still weg).
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            crate::util::log(&format!("settings.txt nicht lesbar ({e}) – Standardwerte für diese Sitzung"));
            LOAD_FAILED.store(true, Ordering::Relaxed);
            DEFAULTS
        }
        Err(_) => {
            LOAD_FAILED.store(false, Ordering::Relaxed);
            // Erster Start: Übernahme der alten Marker-Dateien.
            let mut s = DEFAULTS;
            let (mono, noglow) = (dir.join("mono-tray"), dir.join("no-glow"));
            s.mono_tray = mono.exists();
            if noglow.exists() {
                s.glow = GlowStyle::Off;
            }
            save(&s);
            let _ = std::fs::remove_file(mono);
            let _ = std::fs::remove_file(noglow);
            s
        }
    }
}

fn save(s: &Settings) {
    let Some(dir) = dir() else { return };
    let extra = EXTRA.lock().map(|e| e.clone()).unwrap_or_default();
    let written = crate::util::write_atomic(&dir.join("settings.txt"), serialize(s, &extra).as_bytes());
    remember_stamp();
    if let Err(e) = written {
        crate::util::log(&format!("settings.txt nicht speicherbar ({e})"));
    }
}

/// Änderungszeit von settings.txt beim letzten Lesen oder Schreiben.
static STAMP: Mutex<Option<std::time::SystemTime>> = Mutex::new(None);

fn file_stamp() -> Option<std::time::SystemTime> {
    std::fs::metadata(dir()?.join("settings.txt")).ok()?.modified().ok()
}

fn remember_stamp() {
    let now = file_stamp();
    if let Ok(mut s) = STAMP.lock() {
        *s = now;
    }
}

/// settings.txt von Hand geändert (z. B. `clock_format`, das nur dort
/// einstellbar ist)? Dann neu lesen. `true`, wenn neu gelesen wurde.
pub fn reload_if_changed() -> bool {
    let now = file_stamp();
    let known = STAMP.lock().ok().and_then(|s| *s);
    if now.is_none() || now == known {
        return false;
    }
    let s = load();
    if LOAD_FAILED.load(Ordering::Relaxed) {
        return false;
    }
    if let Ok(mut g) = CURRENT.lock() {
        *g = Some(s);
    }
    true
}

pub fn get() -> Settings {
    let mut g = match CURRENT.lock() {
        Ok(g) => g,
        Err(_) => return DEFAULTS,
    };
    g.get_or_insert_with(load).clone()
}

/// Einstellung ändern und sofort speichern. War die Datei beim Laden nicht
/// lesbar, wird sie vorher noch einmal gelesen (gelingt das, gilt die Änderung
/// auf den gespeicherten Werten). Bleibt sie unlesbar, gilt die Änderung nur
/// für diese Sitzung, und die Datei bleibt unangetastet.
pub fn update(f: impl FnOnce(&mut Settings)) {
    // Von Hand geändert (Editor offen)? Erst übernehmen, sonst überschriebe
    // das Speichern die Änderung.
    reload_if_changed();
    let mut s = get();
    if LOAD_FAILED.load(Ordering::Relaxed) {
        let fresh = load();
        if !LOAD_FAILED.load(Ordering::Relaxed) {
            s = fresh;
        }
    }
    f(&mut s);
    if LOAD_FAILED.load(Ordering::Relaxed) {
        crate::util::log("settings.txt weiterhin nicht lesbar – Änderung nur für diese Sitzung");
    } else {
        save(&s);
    }
    if let Ok(mut g) = CURRENT.lock() {
        *g = Some(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_unknown_keys() {
        let text = "glow=neon\nmono_tray=1\nfuture_option=42\n# Kommentar\nclock_seconds=0\nanother = a=b\npresentation=1\n";
        let (s, extra) = parse(text);
        assert!(s.glow == GlowStyle::Neon);
        assert!(s.mono_tray && s.presentation && !s.clock_seconds);
        assert_eq!(extra, vec!["future_option=42".to_string(), "another = a=b".to_string()]);
        let out = serialize(&s, &extra);
        assert!(out.contains("future_option=42\n"));
        assert!(out.contains("another = a=b\n"));
        let (s2, extra2) = parse(&out);
        assert_eq!(extra2, extra);
        assert_eq!(serialize(&s2, &extra2), out);
    }

    #[test]
    fn legacy_key_is_not_kept() {
        let (s, extra) = parse("windows_logo=1\n");
        assert!(s.start_logo == StartLogo::Windows);
        assert!(extra.is_empty());
        assert!(!serialize(&s, &extra).contains("windows_logo"));
    }

    #[test]
    fn duplicate_unknown_key_kept_once() {
        let (_, extra) = parse("x=1\nx=2\n");
        assert_eq!(extra, vec!["x=1".to_string()]);
    }

    #[test]
    fn size_font_and_clock_round_trip() {
        let (s, extra) = parse("bar_size=xlarge\nclock_display=weekday\nclock_format=HH:mm|ddd dd.MM.\nbar_font=bahnschrift\nclock_bold=1\n");
        assert!(extra.is_empty());
        assert_eq!(s.bar_size, BarSize::XLarge);
        assert_eq!(s.clock_display, ClockDisplay::WeekdayDate);
        assert_eq!(s.clock_format, "HH:mm|ddd dd.MM.");
        assert_eq!(s.bar_font, BarFont::Bahnschrift);
        assert!(s.clock_bold);
        let (s2, _) = parse(&serialize(&s, &extra));
        assert_eq!((s2.bar_size, s2.clock_display, s2.clock_format.as_str(), s2.bar_font, s2.clock_bold), (s.bar_size, s.clock_display, s.clock_format.as_str(), s.bar_font, s.clock_bold));
        // Standard und Unbekanntes: Auto, Datum und Uhrzeit, Regionalformat.
        let (d, _) = parse("bar_size=riesig\nclock_display=?\n");
        assert_eq!((d.bar_size, d.clock_display, d.bar_font), (BarSize::Auto, ClockDisplay::DateTime, BarFont::SegoeVariable));
        assert!(d.clock_format.is_empty() && !d.clock_bold);
    }

    #[test]
    fn auto_size_follows_windows() {
        assert_eq!(BarSize::Auto.resolve(true), BarSize::Small);
        assert_eq!(BarSize::Auto.resolve(false), BarSize::Normal);
        // Eigene Wahl übersteuert Windows.
        assert_eq!(BarSize::Large.resolve(true), BarSize::Large);
        assert_eq!(BarSize::Normal.resolve(true), BarSize::Normal);
        assert_eq!(BarSize::Small.resolve(false), BarSize::Small);
    }
}
