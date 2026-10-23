//! Uhrzeit und Datum im Regionalformat von Windows (Einstellungen → Zeit und
//! Sprache → Region), wie die echte Taskleiste: 12/24 Stunden, Reihenfolge
//! und Trennzeichen des kurzen Datums, langes Datum im Tooltip, erster
//! Wochentag im Kalender. Gelesen wird bei jedem Zeichnen – eine Änderung
//! (WM_SETTINGCHANGE "intl") wirkt also sofort; nur die gemessene Breite der
//! Uhr ist zwischengespeichert und wird dann verworfen.
//!
//! Dazu kommen die Anzeige-Varianten der Leiste (nur Uhrzeit, mit Wochentag,
//! ohne Jahr) und ein eigenes Muster aus settings.txt (`clock_format`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::mem::zeroed;
use std::ptr::null;

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::Globalization::*;
use windows_sys::Win32::Graphics::Gdi::{CreateCompatibleDC, DeleteDC, DeleteObject, FW_BOLD, FW_NORMAL};

use crate::settings::ClockDisplay;
use crate::util::{font_face, text_width, w16};

fn from_buf(buf: &[u16], n: i32) -> Option<String> {
    // n zählt die abschließende Null mit; 0 = Fehler.
    (n > 1).then(|| String::from_utf16_lossy(&buf[..(n - 1) as usize]))
}

/// Uhrzeit, z. B. "14:05", "2:05 PM", mit Sekunden "14:05:09".
pub unsafe fn time(st: &SYSTEMTIME, seconds: bool) -> String {
    let mut buf = [0u16; 64];
    let flags = if seconds { 0 } else { TIME_NOSECONDS };
    let n = GetTimeFormatEx(null(), flags, st, null(), buf.as_mut_ptr(), buf.len() as i32);
    from_buf(&buf, n).unwrap_or_else(|| {
        if seconds {
            format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
        } else {
            format!("{:02}:{:02}", st.wHour, st.wMinute)
        }
    })
}

unsafe fn time_fmt(st: &SYSTEMTIME, format: &str) -> Option<String> {
    let mut buf = [0u16; 64];
    let f = w16(format);
    let n = GetTimeFormatEx(null(), 0, st, f.as_ptr(), buf.as_mut_ptr(), buf.len() as i32);
    from_buf(&buf, n)
}

unsafe fn date_fmt(st: &SYSTEMTIME, flags: u32, format: Option<&str>) -> Option<String> {
    let mut buf = [0u16; 128];
    let f = format.map(w16);
    let fp = f.as_ref().map_or(null(), |f| f.as_ptr());
    // Mit eigenem Muster dürfen keine DATE_*-Flags gesetzt sein.
    let flags = if format.is_some() { 0 } else { flags };
    let n = GetDateFormatEx(null(), flags, st, fp, buf.as_mut_ptr(), buf.len() as i32, null());
    from_buf(&buf, n)
}

/// Kurzes Datum, z. B. "22.10.2026" oder "10/22/2026". `narrow` (senkrechte
/// Leiste): dasselbe Muster ohne Jahr ("22.10." bzw. "10/22").
pub unsafe fn short_date(st: &SYSTEMTIME, narrow: bool) -> String {
    let pattern = if narrow { locale_string(LOCALE_SSHORTDATE).map(|p| strip_year(&p)) } else { None };
    date_fmt(st, DATE_SHORTDATE, pattern.as_deref().filter(|p| !p.is_empty()))
        .unwrap_or_else(|| format!("{:02}.{:02}.{}", st.wDay, st.wMonth, st.wYear))
}

/// Langes Datum für den Tooltip, z. B. "Donnerstag, 22. Oktober 2026".
pub unsafe fn long_date(st: &SYSTEMTIME) -> String {
    date_fmt(st, DATE_LONGDATE, None).unwrap_or_else(|| short_date(st, false))
}

unsafe fn locale_string(lctype: u32) -> Option<String> {
    let mut buf = [0u16; 128];
    let n = GetLocaleInfoEx(null(), lctype, buf.as_mut_ptr(), buf.len() as i32);
    from_buf(&buf, n)
}

/// Erster Wochentag laut Regionalformat: 0 = Montag … 6 = Sonntag
/// (dieselbe Zählung wie LOCALE_IFIRSTDAYOFWEEK).
pub fn first_weekday() -> i32 {
    unsafe { locale_string(LOCALE_IFIRSTDAYOFWEEK) }.and_then(|s| s.trim().parse::<i32>().ok()).filter(|d| (0..7).contains(d)).unwrap_or(0)
}

/// Datumsmuster ohne Jahr: "dd.MM.yyyy" → "dd.MM.", "M/d/yyyy" → "M/d",
/// "yyyy-MM-dd" → "MM-dd". Text in Hochkommas bleibt unangetastet. Ein
/// abschließender Punkt bleibt stehen (deutsche Schreibweise "22.10."),
/// andere Trennzeichen am Rand fallen weg.
pub fn strip_year(pattern: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    for c in pattern.chars() {
        if c == '\'' {
            quoted = !quoted;
        }
        if !quoted && c == 'y' {
            continue;
        }
        out.push(c);
    }
    let sep = |c: char| !c.is_alphanumeric() && c != '\'' && c != '.';
    let out = out.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '\'');
    out.trim_end_matches(sep).trim_end().to_string()
}

// ------------------------------------------------------- Anzeige der Leiste

/// Eine Zeile der Uhr in der Leiste.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Line {
    /// Uhrzeit im Regionalformat (Sekunden je nach Einstellung).
    Time,
    /// Kurzes Datum im Regionalformat, ggf. ohne Jahr und mit Wochentag davor.
    Date { year: bool, weekday: bool },
    /// Eigenes Bildmuster (Zeichen von GetTimeFormatEx und GetDateFormatEx).
    Picture(String),
}

/// Was die Uhr zeigt: obere Zeile und (falls vorhanden) die Datumszeile.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Spec {
    pub time: Line,
    pub date: Option<Line>,
    pub seconds: bool,
}

impl Spec {
    /// Ändert sich die Anzeige jede Sekunde? (Regionalformat mit Sekunden
    /// oder ein eigenes Muster mit "s".) Bestimmt den Takt der Uhr.
    pub fn shows_seconds(&self) -> bool {
        let pic = |l: &Line| matches!(l, Line::Picture(p) if split_picture(p).iter().any(|x| matches!(x, Piece::Time(t) if t.starts_with('s'))));
        (self.seconds && self.time == Line::Time) || pic(&self.time) || self.date.as_ref().is_some_and(pic)
    }
}

/// Zeilen der Uhr aus den Einstellungen. `custom` ist `clock_format` aus
/// settings.txt: "Uhrzeit|Datum"; ohne `|` nur die Uhrzeit (Datum nach
/// `display`), nichts nach dem `|` heißt keine Datumszeile. `narrow`
/// (senkrechte Leiste) lässt beim Regionalformat das Jahr weg.
pub fn spec(display: ClockDisplay, custom: &str, seconds: bool, narrow: bool) -> Spec {
    let by_display = match display {
        ClockDisplay::DateTime => Some(Line::Date { year: !narrow, weekday: false }),
        ClockDisplay::TimeOnly => None,
        ClockDisplay::WeekdayDate => Some(Line::Date { year: !narrow, weekday: true }),
        ClockDisplay::ShortDate => Some(Line::Date { year: false, weekday: false }),
    };
    let custom = custom.trim();
    let picture = |p: &str| {
        let p = p.trim();
        (!p.is_empty()).then(|| Line::Picture(p.to_string()))
    };
    let (time, date) = match custom.split_once('|') {
        _ if custom.is_empty() => (Line::Time, by_display),
        Some((t, d)) => (picture(t).unwrap_or(Line::Time), picture(d)),
        None => (Line::Picture(custom.to_string()), by_display),
    };
    Spec { time, date, seconds }
}

/// Stück eines Bildmusters: ein Lauf gleicher Zeichen für die Uhrzeit (h, H,
/// m, s, t) oder das Datum (d, M, y, g), sonst Text (Trennzeichen, Text in
/// Hochkommas – ohne die Hochkommas, '' steht für ein Hochkomma).
#[derive(PartialEq, Debug)]
enum Piece {
    Time(String),
    Date(String),
    Text(String),
}

fn split_picture(p: &str) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    let text = |out: &mut Vec<Piece>, c: char| match out.last_mut() {
        Some(Piece::Text(s)) => s.push(c),
        _ => out.push(Piece::Text(c.to_string())),
    };
    let mut quoted = false;
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                text(&mut out, '\'');
            } else {
                quoted = !quoted;
            }
            continue;
        }
        let kind = if quoted {
            None
        } else if "hHmst".contains(c) {
            Some(true)
        } else if "dMyg".contains(c) {
            Some(false)
        } else {
            None
        };
        match (kind, out.last_mut()) {
            (Some(true), Some(Piece::Time(s))) | (Some(false), Some(Piece::Date(s))) if s.starts_with(c) => s.push(c),
            (Some(true), _) => out.push(Piece::Time(c.to_string())),
            (Some(false), _) => out.push(Piece::Date(c.to_string())),
            (None, _) => text(&mut out, c),
        }
    }
    out
}

/// Eigenes Muster: jeder Lauf einzeln über GetTimeFormatEx bzw.
/// GetDateFormatEx, Text dazwischen unverändert – so geht auch
/// "ddd HH:mm" in einer Zeile.
unsafe fn picture(st: &SYSTEMTIME, p: &str) -> String {
    split_picture(p)
        .into_iter()
        .map(|piece| match piece {
            Piece::Time(t) => time_fmt(st, &t).unwrap_or(t),
            Piece::Date(d) => date_fmt(st, 0, Some(&d)).unwrap_or(d),
            Piece::Text(t) => t,
        })
        .collect()
}

/// Eine Zeile der Uhr als Text.
pub unsafe fn line(st: &SYSTEMTIME, l: &Line, seconds: bool) -> String {
    match l {
        Line::Time => time(st, seconds),
        Line::Date { year: true, weekday: false } => short_date(st, false),
        Line::Date { year, weekday } => {
            let base = locale_string(LOCALE_SSHORTDATE).unwrap_or_else(|| "dd.MM.yyyy".into());
            let base = if *year { base } else { strip_year(&base) };
            let pattern = if *weekday { format!("ddd {base}") } else { base };
            date_fmt(st, 0, Some(&pattern)).unwrap_or_else(|| short_date(st, !year))
        }
        Line::Picture(p) => picture(st, p),
    }
}

/// Wochentag wie in SYSTEMTIME (0 = Sonntag), gregorianisch.
fn weekday(y: u16, m: u16, d: u16) -> u16 {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = y as i32 - (m < 3) as i32;
    ((y + y / 4 - y / 100 + y / 400 + T[m as usize - 1] + d as i32) % 7) as u16
}

/// Ziffern durch die breiteste ersetzen: So bleibt die Uhr gleich breit,
/// während die Zeit läuft (in Schriften mit unterschiedlich breiten Ziffern).
fn widest_digits(s: &str, widest: char) -> String {
    s.chars().map(|c| if c.is_ascii_digit() { widest } else { c }).collect()
}

/// Grobe Breite ohne Messung (kein Gerätekontext): etwa 0,6 em je Zeichen.
fn estimate(s: &str, size: i32) -> i32 {
    (s.chars().count() as i32 * size * 3 + 4) / 5
}

/// Schrift der Uhr: Familie, Größe (logische Pixel), Uhrzeit fett.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Font {
    pub face: &'static str,
    pub size: i32,
    pub bold: bool,
}

thread_local! {
    /// Gemessene Breite der Uhr (logische Pixel) je Anzeige, Schrift und DPI.
    static WIDTH: RefCell<HashMap<(Spec, Font, u32), i32>> = RefCell::new(HashMap::new());
}

/// Regionalformat oder Einstellungen geändert: Breite neu messen.
pub fn invalidate() {
    WIDTH.with(|w| w.borrow_mut().clear());
}

/// Alle Texte, die eine Zeile annehmen kann: zwei Uhrzeiten (vor und nach
/// Mittag, für den AM/PM-Zusatz) über eine Woche je Monat – also jeder
/// Wochentag und jeder Monatsname einmal.
unsafe fn samples(spec: &Spec, l: &Line) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut st: SYSTEMTIME = zeroed();
    st.wYear = 2028;
    for month in 1..=12 {
        for day in 22..=28 {
            for hour in [10, 22] {
                (st.wMonth, st.wDay, st.wDayOfWeek) = (month, day, weekday(2028, month, day));
                (st.wHour, st.wMinute, st.wSecond) = (hour, 58, 58);
                out.insert(line(&st, l, spec.seconds));
            }
        }
    }
    out
}

/// Breite der Uhrtexte (ohne Rand) in logischen Pixeln (96 DPI), gemessen in
/// der Schrift der Leiste bei `dpi`: die breiteste Uhrzeit und das längste
/// Datum im aktuellen Format, Ziffern als breiteste Ziffer.
pub fn width(spec: &Spec, font: &Font, dpi: u32) -> i32 {
    let key = (spec.clone(), font.clone(), dpi);
    if let Some(w) = WIDTH.with(|w| w.borrow().get(&key).copied()) {
        return w;
    }
    let w = unsafe {
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        let time_font = font_face(dpi, font.size, if font.bold { FW_BOLD } else { FW_NORMAL }, font.face);
        let date_font = font_face(dpi, font.size, FW_NORMAL, font.face);
        let mut w = 0;
        let lines = [Some((&spec.time, time_font)), spec.date.as_ref().map(|d| (d, date_font))];
        for (l, f) in lines.into_iter().flatten() {
            // Breiteste Ziffer in dieser Schrift.
            let widest = ('0'..='9').max_by_key(|c| if dc.is_null() { 0 } else { text_width(dc, &c.to_string(), f) }).unwrap_or('0');
            for s in samples(spec, l) {
                let s = widest_digits(&s, widest);
                let measured = if dc.is_null() || f.is_null() { 0 } else { text_width(dc, &s, f) };
                // Messung gescheitert: schätzen (in Gerätepixeln).
                w = w.max(if measured > 0 { measured } else { estimate(&s, crate::util::px(dpi, font.size)) });
            }
        }
        DeleteObject(time_font);
        DeleteObject(date_font);
        if !dc.is_null() {
            DeleteDC(dc);
        }
        logical(w, dpi)
    };
    WIDTH.with(|c| c.borrow_mut().insert(key, w));
    w
}

/// Gerätepixel in logische, aufgerundet: `px(dpi, logical(w, dpi)) >= w`.
fn logical(w: i32, dpi: u32) -> i32 {
    (w * 96 + dpi as i32 - 1) / dpi as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_year_keeps_order_and_separators() {
        assert_eq!(strip_year("dd.MM.yyyy"), "dd.MM.");
        assert_eq!(strip_year("M/d/yyyy"), "M/d");
        assert_eq!(strip_year("d/M/yy"), "d/M");
        assert_eq!(strip_year("yyyy-MM-dd"), "MM-dd");
        assert_eq!(strip_year("yyyy/M/d"), "M/d");
        assert_eq!(strip_year("d. M. yyyy"), "d. M.");
        assert_eq!(strip_year("dd.MM.yyyy 'г.'"), "dd.MM. 'г.'");
        assert_eq!(strip_year("'year' yyyy"), "'year'");
    }

    #[test]
    fn display_selects_lines() {
        use ClockDisplay::*;
        let date = |year, weekday| Some(Line::Date { year, weekday });
        assert_eq!(spec(DateTime, "", false, false), Spec { time: Line::Time, date: date(true, false), seconds: false });
        assert_eq!(spec(DateTime, "", true, true).date, date(false, false));
        assert_eq!(spec(TimeOnly, "", false, false).date, None);
        assert_eq!(spec(WeekdayDate, "", false, false).date, date(true, true));
        assert_eq!(spec(WeekdayDate, "", false, true).date, date(false, true));
        assert_eq!(spec(ShortDate, "", false, false).date, date(false, false));
    }

    #[test]
    fn custom_format_overrides_display() {
        use ClockDisplay::*;
        let p = |s: &str| Line::Picture(s.into());
        // Nur Uhrzeit-Muster: Datum folgt der Anzeige.
        let s = spec(TimeOnly, "HH:mm", false, false);
        assert_eq!((s.time, s.date), (p("HH:mm"), None));
        assert_eq!(spec(DateTime, " h:mm tt ", false, false).time, p("h:mm tt"));
        // Zwei Zeilen.
        let s = spec(TimeOnly, "HH:mm|ddd dd.MM.", false, false);
        assert_eq!((s.time, s.date), (p("HH:mm"), Some(p("ddd dd.MM."))));
        // Leere Datumszeile, leere Uhrzeit = Regionalformat.
        assert_eq!(spec(DateTime, "HH:mm|", false, false).date, None);
        assert_eq!(spec(DateTime, "|dd.MM.", false, false).time, Line::Time);
        // Nur Leerzeichen zählt als leer.
        assert_eq!(spec(DateTime, "   ", false, false), spec(DateTime, "", false, false));
    }

    #[test]
    fn picture_splits_time_from_date() {
        use Piece::*;
        let s = |v: &str| v.to_string();
        assert_eq!(split_picture("ddd HH:mm"), vec![Date(s("ddd")), Text(s(" ")), Time(s("HH")), Text(s(":")), Time(s("mm"))]);
        assert_eq!(split_picture("h:mm tt"), vec![Time(s("h")), Text(s(":")), Time(s("mm")), Text(s(" ")), Time(s("tt"))]);
        assert_eq!(split_picture("dd.MM."), vec![Date(s("dd")), Text(s(".")), Date(s("MM")), Text(s("."))]);
        // Text in Hochkommas bleibt Text, auch mit Buchstaben der Uhrzeit; '' ist ein Hochkomma.
        assert_eq!(split_picture("'um' H"), vec![Text(s("um ")), Time(s("H"))]);
        assert_eq!(split_picture("HH'h'"), vec![Time(s("HH")), Text(s("h"))]);
        assert_eq!(split_picture("'it''s' d"), vec![Text(s("it's ")), Date(s("d"))]);
    }

    #[test]
    fn seconds_tick_only_when_shown() {
        use ClockDisplay::*;
        assert!(spec(DateTime, "", true, false).shows_seconds());
        assert!(!spec(DateTime, "", false, false).shows_seconds());
        // Eigenes Muster entscheidet selbst.
        assert!(spec(DateTime, "HH:mm:ss", false, false).shows_seconds());
        assert!(!spec(DateTime, "HH:mm", true, false).shows_seconds());
        assert!(!spec(DateTime, "HH:mm 's'", false, false).shows_seconds());
    }

    #[test]
    fn pictures_format_like_windows() {
        let mut st: SYSTEMTIME = unsafe { zeroed() };
        (st.wYear, st.wMonth, st.wDay, st.wDayOfWeek, st.wHour, st.wMinute, st.wSecond) = (2026, 10, 22, 4, 14, 5, 9);
        unsafe {
            assert_eq!(line(&st, &Line::Picture("HH:mm".into()), false), "14:05");
            assert_eq!(line(&st, &Line::Picture("HH:mm:ss".into()), false), "14:05:09");
            assert_eq!(line(&st, &Line::Picture("dd.MM.".into()), false), "22.10.");
            assert_eq!(line(&st, &Line::Picture("ddd HH:mm".into()), false).len() > "14:05".len(), true);
            assert_eq!(line(&st, &Line::Picture("yyyy-MM-dd HH'h'".into()), false), "2026-10-22 14h");
        }
    }

    #[test]
    fn weekday_matches_calendar() {
        assert_eq!(weekday(2026, 10, 22), 4); // Donnerstag
        assert_eq!(weekday(2028, 2, 29), 2); // Dienstag
        assert_eq!(weekday(2000, 1, 1), 6); // Samstag
    }

    #[test]
    fn width_is_stable_and_has_fallback() {
        assert_eq!(widest_digits("10:58", '8'), "88:88");
        assert_eq!(widest_digits("Do. 22.10.", '0'), "Do. 00.00.");
        // Ohne Messung: ungefähr 0,6 em je Zeichen, nie 0 für Text.
        assert_eq!(estimate("88:88", 12), 36);
        assert_eq!(estimate("", 12), 0);
        assert!(estimate("x", 12) > 0);
        // Aufrunden auf logische Pixel: nie schmaler als gemessen.
        for dpi in [96u32, 120, 144, 168, 192] {
            for w in 1..200 {
                assert!(crate::util::px(dpi, logical(w, dpi)) >= w);
            }
        }
    }

    #[test]
    fn width_grows_with_content_and_size() {
        let font = |size, bold| Font { face: "Segoe UI", size, bold };
        let s = |d| spec(d, "", false, false);
        let full = width(&s(ClockDisplay::DateTime), &font(12, false), 96);
        let time_only = width(&s(ClockDisplay::TimeOnly), &font(12, false), 96);
        let weekday = width(&s(ClockDisplay::WeekdayDate), &font(12, false), 96);
        assert!(time_only > 0 && time_only < full && full < weekday, "{time_only} {full} {weekday}");
        assert!(width(&s(ClockDisplay::DateTime), &font(16, false), 96) > full);
        assert!(width(&s(ClockDisplay::TimeOnly), &font(12, true), 96) >= time_only);
        // Höhere Skalierung: in logischen Pixeln etwa gleich breit.
        let hi = width(&s(ClockDisplay::DateTime), &font(12, false), 192);
        assert!((hi - full).abs() <= 3, "{hi} vs {full}");
    }
}
