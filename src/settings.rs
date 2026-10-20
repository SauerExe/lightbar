//! Nutzer-Einstellungen in `%APPDATA%\rtaskbar\settings.txt` (key=value).
//! Ersetzt die früheren Marker-Dateien `mono-tray` und `no-glow`, die beim
//! ersten Laden übernommen und entfernt werden.

use std::path::{Path, PathBuf};
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

#[derive(Clone, Copy)]
pub struct Settings {
    pub glow: GlowStyle,
    pub mono_tray: bool,
    pub clock_seconds: bool,
    pub show_usage: bool,
}

const DEFAULTS: Settings = Settings { glow: GlowStyle::Aurora, mono_tray: false, clock_seconds: false, show_usage: true };

static CURRENT: Mutex<Option<Settings>> = Mutex::new(None);

fn dir() -> Option<PathBuf> {
    Some(Path::new(&std::env::var("APPDATA").ok()?).join("rtaskbar"))
}

fn load() -> Settings {
    let mut s = DEFAULTS;
    let Some(dir) = dir() else { return s };
    let file = dir.join("settings.txt");
    match std::fs::read_to_string(&file) {
        Ok(text) => {
            for line in text.lines() {
                let Some((k, v)) = line.split_once('=') else { continue };
                let (k, v) = (k.trim(), v.trim());
                let on = v == "1" || v == "true";
                match k {
                    "glow" => s.glow = GlowStyle::parse(v).unwrap_or(s.glow),
                    "mono_tray" => s.mono_tray = on,
                    "clock_seconds" => s.clock_seconds = on,
                    "show_usage" => s.show_usage = on,
                    _ => {}
                }
            }
        }
        Err(_) => {
            // Übernahme der alten Marker-Dateien.
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
        "glow={}\nmono_tray={}\nclock_seconds={}\nshow_usage={}\n",
        s.glow.key(),
        b(s.mono_tray),
        b(s.clock_seconds),
        b(s.show_usage)
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
