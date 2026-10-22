//! Kalender beim Klick auf die Uhr: Datum groß, Monatsansicht (Woche ab dem
//! ersten Wochentag des Regionalformats, Kalenderwochen links nur auf Wunsch –
//! wie bei Windows standardmäßig aus), Blättern per Pfeil oder Mausrad, Klick
//! auf den Monatsnamen springt zurück zu heute. Unten: Benachrichtigungen und
//! Datums-Einstellungen.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::util::*;

const WM_MOUSELEAVE: u32 = 0x02A3;
const W: i32 = 340;
const PAD: i32 = 16;
const CELL: i32 = 38;
const HEAD: i32 = 92;
const FOOT: i32 = 52;
const H: i32 = HEAD + 40 + 28 + 6 * CELL + 8 + FOOT;

const DAYS_DE: [&str; 7] = ["Mo", "Di", "Mi", "Do", "Fr", "Sa", "So"];
const DAY_NAMES_DE: [&str; 7] = ["Montag", "Dienstag", "Mittwoch", "Donnerstag", "Freitag", "Samstag", "Sonntag"];
const MONTHS_DE: [&str; 12] = ["Januar", "Februar", "März", "April", "Mai", "Juni", "Juli", "August", "September", "Oktober", "November", "Dezember"];
const DAYS_EN: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
const DAY_NAMES_EN: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const MONTHS_EN: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn days() -> &'static [&'static str; 7] {
    if crate::i18n::en() { &DAYS_EN } else { &DAYS_DE }
}
fn day_names() -> &'static [&'static str; 7] {
    if crate::i18n::en() { &DAY_NAMES_EN } else { &DAY_NAMES_DE }
}
fn months() -> &'static [&'static str; 12] {
    if crate::i18n::en() { &MONTHS_EN } else { &MONTHS_DE }
}

static HWND_CAL: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Prev,
    Next,
    Today,
    Notifications,
    Settings,
}

struct State {
    dpi: u32,
    fonts: Option<(HFONT, HFONT, HFONT, HFONT)>, // normal, small, big, icons
    year: i32,
    month: i32,
    mouse: (i32, i32),
    tracking: bool,
    hits: Vec<(RECT, Act)>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| {
        let mut g = s.try_borrow_mut().ok()?;
        g.as_mut().map(f)
    })
}

// ------------------------------------------------------------- Datum

/// Tage seit 1970-01-01 (proleptisch gregorianisch).
fn days_from_civil(y: i32, m: i32, d: i32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d as i64 - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}

/// Wochentag 0 = Montag … 6 = Sonntag.
fn weekday(y: i32, m: i32, d: i32) -> i32 {
    // 1970-01-01 war ein Donnerstag (3).
    ((days_from_civil(y, m, d) + 3).rem_euclid(7)) as i32
}

fn days_in_month(y: i32, m: i32) -> i32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    (days_from_civil(ny, nm, 1) - days_from_civil(y, m, 1)) as i32
}

/// ISO-Kalenderwoche.
fn iso_week(y: i32, m: i32, d: i32) -> i32 {
    let days = days_from_civil(y, m, d);
    // Donnerstag derselben Woche bestimmt das Jahr der Woche.
    let thursday = days - weekday(y, m, d) as i64 + 3;
    let ty = {
        let mut yy = y + 1;
        while days_from_civil(yy, 1, 1) > thursday {
            yy -= 1;
        }
        yy
    };
    ((thursday - days_from_civil(ty, 1, 1)) / 7 + 1) as i32
}

/// Spalte (0…6) eines Wochentags (0 = Montag), wenn die Woche an `first` beginnt.
fn column(weekday: i32, first: i32) -> i32 {
    (weekday - first).rem_euclid(7)
}

/// Kalenderwoche einer Rasterzeile, die am Tag `days` (seit 1970) beginnt:
/// die ISO-Woche ihres Montags (eine am Sonntag beginnende Zeile gehört
/// überwiegend zur Woche des folgenden Montags).
fn row_week(days: i64) -> i32 {
    let monday = (0..7).map(|k| days + k).find(|d| (d + 3).rem_euclid(7) == 0).unwrap_or(days);
    let (y, m, d) = civil_from_days(monday);
    iso_week(y, m, d)
}

/// Umkehrung von `days_from_civil`.
fn civil_from_days(z: i64) -> (i32, i32, i32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    (y as i32, m as i32, d as i32)
}

fn today() -> (i32, i32, i32) {
    let mut st: SYSTEMTIME = unsafe { zeroed() };
    unsafe { GetLocalTime(&mut st) };
    (st.wYear as i32, st.wMonth as i32, st.wDay as i32)
}

// ----------------------------------------------------------- Zeichnen

const LINE: u32 = DT_LEFT | DT_VCENTER | DT_SINGLELINE;
const CENTER: u32 = DT_CENTER | DT_VCENTER | DT_SINGLELINE;

unsafe fn draw(st: &mut State, dc: HDC, w: i32, h: i32) {
    let Some((normal, small, big, icons)) = st.fonts else { return };
    let d = st.dpi;
    st.hits.clear();
    fill(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, BG);
    let (ty, tm, td) = today();
    let pad = px(d, PAD);

    // Kopf: heutiges Datum groß
    let head = RECT { left: pad, top: px(d, 14), right: w - pad, bottom: px(d, 50) };
    let (day, month) = (day_names()[weekday(ty, tm, td) as usize], months()[(tm - 1) as usize]);
    text(dc, &tf!("{day}, {td}. {month}", "{day}, {month} {td}"), &head, big, TEXT, LINE);
    let weeks = crate::settings::get().week_numbers;
    let first_day = crate::clockfmt::first_weekday();
    if weeks {
        let sub = RECT { left: pad, top: px(d, 50), right: w - pad, bottom: px(d, 74) };
        text(dc, &tf!("Kalenderwoche {}", "Week {}", iso_week(ty, tm, td)), &sub, small, SUBTEXT, LINE);
    }
    fill(dc, &RECT { left: 0, top: px(d, HEAD - 6), right: w, bottom: px(d, HEAD - 5) }, BORDER);

    // Monatszeile mit Pfeilen
    let row = RECT { left: pad, top: px(d, HEAD), right: w - pad, bottom: px(d, HEAD + 36) };
    let title = RECT { right: row.right - px(d, 80), ..row };
    let is_now = st.year == ty && st.month == tm;
    if !is_now && inside(&title, st.mouse.0, st.mouse.1) {
        round(dc, &title, HOVER, px(d, 6));
    }
    text(dc, &format!("{} {}", months()[(st.month - 1) as usize], st.year), &RECT { left: title.left + px(d, 8), ..title }, normal, TEXT, LINE);
    st.hits.push((title, Act::Today));
    for (i, (glyph, act)) in [("\u{E70E}", Act::Prev), ("\u{E70D}", Act::Next)].into_iter().enumerate() {
        let r = RECT { left: row.right - px(d, 76) + i as i32 * px(d, 40), top: row.top + px(d, 2), right: row.right - px(d, 40) + i as i32 * px(d, 40), bottom: row.bottom - px(d, 2) };
        if inside(&r, st.mouse.0, st.mouse.1) {
            round(dc, &r, HOVER, px(d, 6));
        }
        text(dc, glyph, &r, icons, TEXT, CENTER);
        st.hits.push((r, act));
    }

    // Wochentage + Raster (links optional die Kalenderwoche)
    let cell = px(d, CELL);
    let grid_left = pad + if weeks { px(d, 30) } else { 0 };
    let cw = (w - grid_left - pad) / 7;
    let wd_top = row.bottom + px(d, 4);
    for i in 0..7 {
        let name = days()[((i + first_day) % 7) as usize];
        let r = RECT { left: grid_left + i * cw, top: wd_top, right: grid_left + (i + 1) * cw, bottom: wd_top + px(d, 24) };
        text(dc, name, &r, small, SUBTEXT, CENTER);
    }
    let first = column(weekday(st.year, st.month, 1), first_day);
    let len = days_in_month(st.year, st.month);
    let (py, pm) = if st.month == 1 { (st.year - 1, 12) } else { (st.year, st.month - 1) };
    let prev_len = days_in_month(py, pm);
    let grid_top = wd_top + px(d, 28);
    let month_start = days_from_civil(st.year, st.month, 1);
    for week in 0..6 {
        let y0 = grid_top + week * cell;
        if weeks {
            // Erster Tag dieser Zeile → Kalenderwoche
            let row_start = month_start + (week * 7 - first) as i64;
            let kw = RECT { left: pad - px(d, 4), top: y0, right: grid_left, bottom: y0 + cell };
            text(dc, &row_week(row_start).to_string(), &kw, small, DIM, CENTER);
        }
        for col in 0..7 {
            let n = week * 7 + col - first + 1;
            let (day, current) = if n < 1 {
                (prev_len + n, false)
            } else if n > len {
                (n - len, false)
            } else {
                (n, true)
            };
            let r = RECT { left: grid_left + col * cw, top: y0, right: grid_left + (col + 1) * cw, bottom: y0 + cell };
            let is_today = current && st.year == ty && st.month == tm && day == td;
            let size = px(d, 32);
            let circle = RECT { left: (r.left + r.right - size) / 2, top: (r.top + r.bottom - size) / 2, right: (r.left + r.right + size) / 2, bottom: (r.top + r.bottom + size) / 2 };
            if is_today {
                round(dc, &circle, accent(), size);
            } else if inside(&r, st.mouse.0, st.mouse.1) && current {
                round(dc, &circle, HOVER, size);
            }
            let color = if is_today {
                rgb(0, 0, 0)
            } else if !current {
                DIM
            } else if (col + first_day) % 7 >= 5 {
                SUBTEXT
            } else {
                TEXT
            };
            text(dc, &day.to_string(), &r, normal, color, CENTER);
        }
    }

    // Fußleiste
    let foot = RECT { left: 0, top: h - px(d, FOOT), right: w, bottom: h };
    fill(dc, &foot, PANEL);
    fill(dc, &RECT { bottom: foot.top + 1, ..foot }, BORDER);
    let half = w / 2;
    for (i, (label, act)) in [(t!("Benachrichtigungen", "Notifications"), Act::Notifications), (t!("Datum & Uhrzeit", "Date & time"), Act::Settings)].into_iter().enumerate() {
        let r = RECT {
            left: if i == 0 { pad - px(d, 6) } else { half + px(d, 4) },
            top: foot.top + px(d, 10),
            right: if i == 0 { half - px(d, 4) } else { w - pad + px(d, 6) },
            bottom: h - px(d, 10),
        };
        if inside(&r, st.mouse.0, st.mouse.1) {
            round(dc, &r, HOVER, px(d, 6));
        }
        text(dc, label, &r, small, TEXT, CENTER);
        st.hits.push((r, act));
    }
}

// ----------------------------------------------------------- Steuerung

pub fn visible() -> bool {
    let h = HWND_CAL.load(Ordering::Relaxed);
    unsafe { !h.is_null() && IsWindowVisible(h) != 0 }
}

pub unsafe fn toggle() {
    // Automatisch ausgeblendete Leiste: nach dem Öffnen/Schließen nachsehen.
    crate::bar::autohide_wake_later();
    let h = HWND_CAL.load(Ordering::Relaxed);
    if h.is_null() {
        return;
    }
    if IsWindowVisible(h) != 0 {
        ShowWindow(h, SW_HIDE);
        return;
    }
    // Titel (Screenreader, Fensterlisten) in der aktuellen Sprache.
    SetWindowTextW(h, w16(t!("Kalender", "Calendar")).as_ptr());
    let bar = crate::bar::anchor();
    let dpi = GetDpiForWindow(bar);
    let (y, m, _) = today();
    with(|st| {
        if st.dpi != dpi || st.fonts.is_none() {
            if let Some((a, b, c, e)) = st.fonts.take() {
                for f in [a, b, c, e] {
                    DeleteObject(f);
                }
            }
            st.fonts = Some((make_font(dpi, 13, FW_NORMAL), make_font(dpi, 12, FW_NORMAL), make_font(dpi, 20, FW_SEMIBOLD), icon_font(dpi, 12)));
            st.dpi = dpi;
        }
        st.year = y;
        st.month = m;
    });
    // Angedockte Lage – beim automatischen Ausblenden kann die Leiste gerade weggeschoben sein.
    let br = crate::bar::docked_rect(bar);
    let (w, hh) = (px(dpi, W), px(dpi, H));
    // Am Ende der Leiste (rechts bzw. bei einer seitlichen Leiste unten).
    let (x, y) = crate::display::place_near(br, w, hh, px(dpi, 12), crate::display::Along::End);
    crate::anim::pop_in(h, x, y, w, hh, crate::display::pop_offset(px(dpi, 18)));
    force_foreground(h);
    InvalidateRect(h, null(), 0);
}

/// Offenen Kalender neu zeichnen (Regionalformat, Uhrzeit oder Einstellung geändert).
pub unsafe fn refresh() {
    let h = HWND_CAL.load(Ordering::Relaxed);
    if !h.is_null() && IsWindowVisible(h) != 0 {
        InvalidateRect(h, null(), 0);
    }
}

fn shift_month(delta: i32) {
    with(|st| {
        let idx = st.year * 12 + (st.month - 1) + delta;
        st.year = idx.div_euclid(12);
        st.month = idx.rem_euclid(12) + 1;
    });
}

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
                ShowWindow(hwnd, SW_HIDE);
            }
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
            let delta = ((wp >> 16) & 0xffff) as i16;
            shift_month(if delta > 0 { -1 } else { 1 });
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(lp);
            let act = with(|st| st.hits.iter().find(|(r, _)| inside(r, x, y)).map(|(_, a)| *a)).flatten();
            match act {
                Some(Act::Prev) => shift_month(-1),
                Some(Act::Next) => shift_month(1),
                Some(Act::Today) => {
                    let (y, m, _) = today();
                    with(|st| {
                        st.year = y;
                        st.month = m;
                    });
                }
                Some(Act::Notifications) => {
                    ShowWindow(hwnd, SW_HIDE);
                    send_keys(&[(VK_LWIN, false), (b'N' as u16, false), (b'N' as u16, true), (VK_LWIN, true)]);
                }
                Some(Act::Settings) => {
                    ShowWindow(hwnd, SW_HIDE);
                    shell_open("ms-settings:dateandtime", "", SW_SHOWNORMAL);
                }
                None => {}
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_KEYDOWN => {
            match wp as u16 {
                VK_ESCAPE => {
                    ShowWindow(hwnd, SW_HIDE);
                }
                VK_LEFT | VK_PRIOR => shift_month(-1),
                VK_RIGHT | VK_NEXT => shift_month(1),
                _ => return DefWindowProcW(hwnd, msg, wp, lp),
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub unsafe fn create(inst: HINSTANCE) {
    let class = w16("LightbarCalendar");
    register_class(inst, &class, Some(wndproc), 0);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
        class.as_ptr(),
        // Sprache kann bis zum Öffnen wechseln: `toggle` setzt den Titel neu.
        w16(t!("Kalender", "Calendar")).as_ptr(),
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
    let round_pref: u32 = 2;
    let border = BORDER;
    DwmSetWindowAttribute(hwnd, 33, &round_pref as *const _ as _, 4);
    DwmSetWindowAttribute(hwnd, 34, &border as *const _ as _, 4);
    let (y, m, _) = today();
    STATE.with(|s| {
        *s.borrow_mut() = Some(State { dpi: 0, fonts: None, year: y, month: m, mouse: (-1, -1), tracking: false, hits: Vec::new() })
    });
    HWND_CAL.store(hwnd, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for z in [-800_000i64, -1, 0, 1, 10_957, 20_748, 60_000, 400_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(days_from_civil(2026, 10, 22)), (2026, 10, 22));
    }

    #[test]
    fn columns_follow_first_weekday() {
        // 1. Oktober 2026 ist ein Donnerstag (3).
        let thu = weekday(2026, 10, 1);
        assert_eq!(thu, 3);
        assert_eq!(column(thu, 0), 3); // Woche ab Montag
        assert_eq!(column(thu, 6), 4); // Woche ab Sonntag (USA)
        assert_eq!(column(thu, 5), 5); // Woche ab Samstag
        assert_eq!(column(6, 6), 0);
    }

    #[test]
    fn row_week_uses_the_rows_monday() {
        // Zeile ab Montag, 28.12.2026: ISO-Woche 53.
        assert_eq!(row_week(days_from_civil(2026, 12, 28)), 53);
        // Zeile ab Sonntag, 3.1.2027: Montag 4.1.2027 → Woche 1.
        assert_eq!(row_week(days_from_civil(2027, 1, 3)), 1);
        // Zeile ab Sonntag, 18.10.2026: Montag 19.10. → Woche 43.
        assert_eq!(row_week(days_from_civil(2026, 10, 18)), 43);
    }
}
