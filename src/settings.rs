//! Nutzer-Einstellungen in `%APPDATA%\Lightbar\settings.txt` (key=value).
//! Ersetzt die früheren Marker-Dateien `mono-tray` und `no-glow`, die beim
//! ersten Laden übernommen und entfernt werden.

use std::path::PathBuf;
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
}

// Usage-Anzeige standardmäßig aus: Sie liest lokale Logins und nutzt nicht
// offiziell dokumentierte Schnittstellen – das soll man bewusst einschalten.
const DEFAULTS: Settings = Settings { glow: GlowStyle::Aurora, mono_tray: false, clock_seconds: false, show_usage: false, usage_claude: true, usage_codex: true, start_logo: StartLogo::Orb, presentation: false, start_avatar: true, language: crate::i18n::LangSetting::Auto, input_lang: true };

static CURRENT: Mutex<Option<Settings>> = Mutex::new(None);

fn dir() -> Option<PathBuf> {
    crate::util::data_dir()
}

fn load() -> Settings {
    let mut s = DEFAULTS;
    let Some(dir) = dir() else { return s };
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
            let text = String::from_utf8_lossy(&bytes);
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
                    _ => {}
                }
            }
        }
        // Nicht lesbar: mit Standardwerten laufen, die Datei aber nicht
        // überschreiben (sonst wäre z. B. der Präsentationsmodus still weg).
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            crate::util::log(&format!("settings.txt nicht lesbar ({e}) – Standardwerte für diese Sitzung"));
        }
        Err(_) => {
            // Erster Start: Übernahme der alten Marker-Dateien.
            let (mono, noglow) = (dir.join("mono-tray"), dir.join("no-glow"));
            s.mono_tray = mono.exists();
            if noglow.exists() {
                s.glow = GlowStyle::Off;
            }
            save(&s);
            let _ = std::fs::remove_file(mono);
            let _ = std::fs::remove_file(noglow);
        }
    }
    s
}

fn save(s: &Settings) {
    let Some(dir) = dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let b = |v: bool| if v { "1" } else { "0" };
    let text = format!(
        "glow={}\nmono_tray={}\nclock_seconds={}\nshow_usage={}\nusage_claude={}\nusage_codex={}\nstart_logo={}\npresentation={}\nstart_avatar={}\nlanguage={}\ninput_lang={}\n",
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
        b(s.input_lang)
    );
    let _ = std::fs::write(dir.join("settings.txt"), text);
}

pub fn get() -> Settings {
    let mut g = match CURRENT.lock() {
        Ok(g) => g,
        Err(_) => return DEFAULTS,
    };
    *g.get_or_insert_with(load)
}

/// Einstellung ändern und sofort speichern.
pub fn update(f: impl FnOnce(&mut Settings)) {
    let mut s = get();
    f(&mut s);
    save(&s);
    if let Ok(mut g) = CURRENT.lock() {
        *g = Some(s);
    }
}
