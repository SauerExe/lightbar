//! Uhrzeit und Datum im Regionalformat von Windows (Einstellungen → Zeit und
//! Sprache → Region), wie die echte Taskleiste: 12/24 Stunden, Reihenfolge
//! und Trennzeichen des kurzen Datums, langes Datum im Tooltip, erster
//! Wochentag im Kalender. Gelesen wird bei jedem Zeichnen – eine Änderung
//! (WM_SETTINGCHANGE "intl") wirkt also sofort; nur die gemessene Breite der
//! Uhr ist zwischengespeichert und wird dann verworfen.

use std::cell::Cell;
use std::mem::zeroed;
use std::ptr::null;

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::Globalization::*;
use windows_sys::Win32::Graphics::Gdi::{CreateCompatibleDC, DeleteDC, DeleteObject, FW_NORMAL};

use crate::util::{make_font, text_width, w16};

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

/// WM_SETTINGCHANGE mit lParam = "intl": Regionalformat geändert.
pub unsafe fn is_intl(lp: isize) -> bool {
    if lp == 0 {
        return false;
    }
    let p = lp as *const u16;
    let mut s = Vec::new();
    // Nur kurz lesen: "intl" hat 4 Zeichen, längere Bereiche sind es nicht.
    for i in 0..8 {
        let c = *p.add(i);
        if c == 0 {
            break;
        }
        s.push(c);
    }
    String::from_utf16_lossy(&s).eq_ignore_ascii_case("intl")
}

thread_local! {
    /// Gemessene Breite der Uhr (logische Pixel) je Sekundenanzeige.
    static WIDTH: Cell<[Option<i32>; 2]> = const { Cell::new([None, None]) };
}

/// Regionalformat geändert: Breite neu messen.
pub fn invalidate() {
    WIDTH.with(|w| w.set([None, None]));
}

/// Breite der Uhr in logischen Pixeln (96 DPI): breiteste Uhrzeit und
/// längstes Datum im aktuellen Format plus Rand, mindestens `min`. Das
/// 12-Stunden-Format ("10:58:58 PM") ist deutlich breiter als "22:58".
pub fn width(seconds: bool, min: i32) -> i32 {
    let slot = seconds as usize;
    if let Some(w) = WIDTH.with(|w| w.get()[slot]) {
        return w;
    }
    let measured = unsafe {
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        let font = make_font(96, 12, FW_NORMAL);
        let mut st: SYSTEMTIME = zeroed();
        // Breite Ziffern, zweistelliger Tag und Monat, nach Mittag (AM/PM-Zusatz).
        (st.wYear, st.wMonth, st.wDay, st.wDayOfWeek) = (2028, 12, 28, 4);
        let mut w = 0;
        for hour in [10, 22] {
            (st.wHour, st.wMinute, st.wSecond) = (hour, 58, 58);
            w = w.max(text_width(dc, &time(&st, seconds), font));
        }
        w = w.max(text_width(dc, &short_date(&st, false), font));
        DeleteObject(font);
        DeleteDC(dc);
        // Rand wie beim Zeichnen (10 px rechts) und etwas Luft links.
        w + 20
    };
    let w = measured.max(min);
    WIDTH.with(|c| {
        let mut v = c.get();
        v[slot] = Some(w);
        c.set(v);
    });
    w
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
    fn width_never_below_minimum() {
        assert!(width(false, 84) >= 84);
        assert!(width(true, 84) >= 84);
        // Breiter als jedes Minimum: das Gemessene zählt.
        invalidate();
        assert_eq!(width(false, 10_000), 10_000);
    }
}
