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

#[derive(Clone, Copy)]
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
}

// Usage-Anzeige standardmäßig aus: Sie liest lokale Logins und nutzt nicht
// offiziell dokumentierte Schnittstellen – das soll man bewusst einschalten.
const DEFAULTS: Settings = Settings { glow: GlowStyle::Aurora, mono_tray: false, clock_seconds: false, show_usage: false, usage_claude: true, usage_codex: true, start_logo: StartLogo::Orb, presentation: false, start_avatar: true, language: crate::i18n::LangSetting::Auto, input_lang: true, taskbar_search: false, week_numbers: false };

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
        "glow={}\nmono_tray={}\nclock_seconds={}\nshow_usage={}\nusage_claude={}\nusage_codex={}\nstart_logo={}\npresentation={}\nstart_avatar={}\nlanguage={}\ninput_lang={}\ntaskbar_search={}\nweek_numbers={}\n",
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
        b(s.week_numbers)
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
    if let Err(e) = crate::util::write_atomic(&dir.join("settings.txt"), serialize(s, &extra).as_bytes()) {
        crate::util::log(&format!("settings.txt nicht speicherbar ({e})"));
    }
}

pub fn get() -> Settings {
    let mut g = match CURRENT.lock() {
        Ok(g) => g,
        Err(_) => return DEFAULTS,
    };
    *g.get_or_insert_with(load)
}

/// Einstellung ändern und sofort speichern. War die Datei beim Laden nicht
/// lesbar, wird sie vorher noch einmal gelesen (gelingt das, gilt die Änderung
/// auf den gespeicherten Werten). Bleibt sie unlesbar, gilt die Änderung nur
/// für diese Sitzung, und die Datei bleibt unangetastet.
pub fn update(f: impl FnOnce(&mut Settings)) {
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
}
