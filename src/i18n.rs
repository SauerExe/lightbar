//! Sprache der Oberfläche (Deutsch/Englisch).
//!
//! Texte stehen paarweise direkt an der Aufrufstelle: `t!("Suchen", "Search")`
//! liefert `&'static str`, `tf!("„{q}“ suchen", "Search \"{q}\"")` einen
//! `String` (wie `format!`, auch mit eingefangenen Variablen). Die Wahl ist ein
//! einzelnes Atomic – ein Lesezugriff pro Text, keine Tabelle, kein Hashing.
//! Log-Zeilen (lightbar.log) bleiben deutsch.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Language {
    De,
    En,
}

/// Einstellung `language` in settings.txt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LangSetting {
    /// Folgt der Windows-Anzeigesprache (Deutsch → De, sonst En).
    Auto,
    De,
    En,
}

impl LangSetting {
    pub fn key(self) -> &'static str {
        match self {
            LangSetting::Auto => "auto",
            LangSetting::De => "de",
            LangSetting::En => "en",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [LangSetting::Auto, LangSetting::De, LangSetting::En].into_iter().find(|l| l.key() == s)
    }
}

// 0 = noch nicht bestimmt, 1 = De, 2 = En.
static CURRENT: AtomicU8 = AtomicU8::new(0);

#[link(name = "kernel32")]
extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
}

/// Windows-Anzeigesprache: Primärsprache Deutsch (LANG_GERMAN = 0x07) → De.
pub fn system_language() -> Language {
    let id = unsafe { GetUserDefaultUILanguage() };
    if id & 0x3FF == 0x07 {
        Language::De
    } else {
        Language::En
    }
}

pub fn resolve(s: LangSetting) -> Language {
    match s {
        LangSetting::Auto => system_language(),
        LangSetting::De => Language::De,
        LangSetting::En => Language::En,
    }
}

// Tests: Sprache je Thread, Standard Deutsch – unabhängig von settings.txt
// und ohne dass parallele Tests sich gegenseitig umschalten.
#[cfg(test)]
thread_local! {
    static TEST_LANG: std::cell::Cell<Language> = const { std::cell::Cell::new(Language::De) };
}

/// Aktuelle Sprache; beim ersten Aufruf aus den Einstellungen bestimmt.
#[cfg(test)]
pub fn current() -> Language {
    TEST_LANG.with(|c| c.get())
}

#[cfg(test)]
pub fn set(l: Language) {
    TEST_LANG.with(|c| c.set(l));
}

#[cfg(not(test))]
pub fn current() -> Language {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Language::De,
        2 => Language::En,
        _ => {
            let l = resolve(crate::settings::get().language);
            set(l);
            l
        }
    }
}

#[cfg(not(test))]
pub fn set(l: Language) {
    CURRENT.store(if l == Language::En { 2 } else { 1 }, Ordering::Relaxed);
}

/// Englische Oberfläche?
#[inline]
pub fn en() -> bool {
    current() == Language::En
}

/// Einstellung ändern, speichern und die Sprache sofort umschalten.
/// Die Aufrufer zeichnen danach Leisten und Popups neu.
pub fn apply(s: LangSetting) {
    crate::settings::update(|x| x.language = s);
    set(resolve(s));
}

/// Paar aus deutschem und englischem Literal → `&'static str`.
#[macro_export]
macro_rules! t {
    ($de:literal, $en:literal $(,)?) => {
        if $crate::i18n::en() {
            $en
        } else {
            $de
        }
    };
}

/// Wie `format!`, mit deutscher und englischer Vorlage → `String`.
#[macro_export]
macro_rules! tf {
    ($de:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::en() {
            format!($en $(, $arg)*)
        } else {
            format!($de $(, $arg)*)
        }
    };
}

#[cfg(test)]
mod tests {
    //! Prüft alle `t!`/`tf!`-Paare im Quelltext: keine leeren Texte, gleiche
    //! Platzhalter in beiden Sprachen.

    const SOURCES: &[(&str, &str)] = &[
        ("ai.rs", include_str!("ai.rs")),
        ("ai_tools.rs", include_str!("ai_tools.rs")),
        ("apps.rs", include_str!("apps.rs")),
        ("bar.rs", include_str!("bar.rs")),
        ("calendar.rs", include_str!("calendar.rs")),
        ("chat.rs", include_str!("chat.rs")),
        ("display.rs", include_str!("display.rs")),
        ("jumplist.rs", include_str!("jumplist.rs")),
        ("main.rs", include_str!("main.rs")),
        ("menus.rs", include_str!("menus.rs")),
        ("preview.rs", include_str!("preview.rs")),
        ("quick.rs", include_str!("quick.rs")),
        ("shell.rs", include_str!("shell.rs")),
        ("start.rs", include_str!("start.rs")),
        ("status.rs", include_str!("status.rs")),
        ("tray.rs", include_str!("tray.rs")),
        ("uia.rs", include_str!("uia.rs")),
        ("usage.rs", include_str!("usage.rs")),
        ("watchdog.rs", include_str!("watchdog.rs")),
    ];

    /// Liest ein Rust-Stringliteral (normal oder raw) ab `s` (nach Leerraum).
    /// Gibt (Inhalt, Rest) zurück.
    fn literal(s: &str) -> Option<(String, &str)> {
        let s = s.trim_start();
        if let Some(rest) = s.strip_prefix('r') {
            let hashes = rest.chars().take_while(|&c| c == '#').count();
            let rest = rest[hashes..].strip_prefix('"')?;
            let end = format!("\"{}", "#".repeat(hashes));
            let i = rest.find(&end)?;
            return Some((rest[..i].to_string(), &rest[i + end.len()..]));
        }
        let rest = s.strip_prefix('"')?;
        let mut out = String::new();
        let mut it = rest.char_indices();
        while let Some((i, c)) = it.next() {
            match c {
                '\\' => {
                    if let Some((_, e)) = it.next() {
                        out.push(match e {
                            'n' => '\n',
                            't' => '\t',
                            _ => e,
                        });
                    }
                }
                '"' => return Some((out, &rest[i + 1..])),
                _ => out.push(c),
            }
        }
        None
    }

    /// Platzhalter `{…}` (ohne `{{`), sortiert.
    fn placeholders(s: &str) -> Vec<String> {
        let s = s.replace("{{", "").replace("}}", "");
        let mut v: Vec<String> = s.split('{').skip(1).filter_map(|p| p.split_once('}').map(|(a, _)| a.to_string())).collect();
        v.sort();
        v
    }

    fn pairs() -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for (file, src) in SOURCES {
            for (needle, is_fmt) in [("t!(", false), ("tf!(", true)] {
                let mut rest = *src;
                while let Some(i) = rest.find(needle) {
                    // `tf!(` enthält kein `t!(` an derselben Stelle, aber `t!(`
                    // findet das Ende von `tf!(` – das Zeichen davor prüfen.
                    let before = rest[..i].chars().last();
                    rest = &rest[i + needle.len()..];
                    if !is_fmt && matches!(before, Some(c) if c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                    // Makrodefinition selbst und Kommentare/Doku überspringen.
                    let Some((de, r)) = literal(rest) else { continue };
                    let Some(r) = r.trim_start().strip_prefix(',') else { continue };
                    let Some((en, r)) = literal(r) else { continue };
                    rest = r;
                    out.push((file.to_string(), de, en));
                }
            }
        }
        out
    }

    #[test]
    fn pairs_complete() {
        let all = pairs();
        assert!(all.len() > 250, "zu wenige Textpaare gefunden: {}", all.len());
        for (file, de, en) in &all {
            assert!(!de.trim().is_empty(), "{file}: leerer deutscher Text (en = {en:?})");
            assert!(!en.trim().is_empty(), "{file}: leerer englischer Text (de = {de:?})");
            assert_eq!(placeholders(de), placeholders(en), "{file}: Platzhalter verschieden: {de:?} / {en:?}");
        }
    }

    #[test]
    fn english_has_no_umlauts() {
        for (file, de, en) in pairs() {
            assert!(!en.contains(['ä', 'ö', 'ü', 'Ä', 'Ö', 'Ü', 'ß', '„']), "{file}: deutsch im englischen Text? {en:?} (de = {de:?})");
        }
    }

    #[test]
    fn spot_checks() {
        use super::*;
        let all = pairs();
        let has = |de: &str, en: &str| all.iter().any(|(_, d, e)| d == de && e == en);
        assert!(has("Lightbar beenden", "Quit Lightbar"));
        assert!(has("Task-Manager", "Task Manager"));
        assert!(has("„{raw}“ im Web suchen", "Search the web for \"{raw}\""));
        assert!(has("Kalenderwoche {}", "Week {}"));
        assert!(has("Einstellungen", "Settings"));
        set(Language::En);
        assert_eq!(t!("Suchen", "Search"), "Search");
        let q = "rust";
        assert_eq!(tf!("„{q}“ im Web suchen", "Search the web for \"{q}\""), "Search the web for \"rust\"");
        set(Language::De);
        assert_eq!(t!("Suchen", "Search"), "Suchen");
        assert_eq!(tf!("„{q}“ im Web suchen", "Search the web for \"{q}\""), "„rust“ im Web suchen");
        assert_eq!(LangSetting::parse("en"), Some(LangSetting::En));
        assert_eq!(LangSetting::parse("auto"), Some(LangSetting::Auto));
    }
}
