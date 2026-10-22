//! Eingabesprache (Tastaturlayout) wie bei Windows 11: "DEU" / "ENG" neben
//! den Status-Symbolen, nur wenn mehr als ein Layout installiert ist.
//!
//! Angezeigt wird das Layout des Vordergrundfensters. Die Namen der Layouts
//! (Registry, Locale-Daten) werden nur bei geänderter Layoutliste neu gelesen;
//! die regelmäßige Prüfung kostet nur ein paar billige User32-Aufrufe.

use std::cell::RefCell;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Globalization::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::util::w16;

/// Ein installiertes Layout mit allem, was Anzeige, Tooltip und Menü brauchen.
#[derive(Clone, PartialEq, Debug)]
pub struct Entry {
    pub hkl: isize,
    /// Drei Buchstaben wie bei Windows, z. B. "DEU", "ENG".
    pub abbr: String,
    /// Kleine zweite Zeile, wenn ein anderes Layout dieselbe Sprache hat
    /// ("DE" / "CH" bzw. Kürzel des Layouts).
    pub sub: Option<String>,
    /// "Deutsch (Deutschland)".
    pub language: String,
    /// "Deutsch" (Name des Tastaturlayouts).
    pub layout: String,
}

/// Was die Leiste zeigt: aktuelles Layout plus alle installierten.
#[derive(Clone, PartialEq, Debug)]
pub struct Indicator {
    pub current: isize,
    pub entries: Vec<Entry>,
}

impl Indicator {
    pub fn active(&self) -> Option<&Entry> {
        self.entries.iter().find(|e| e.hkl == self.current)
    }
}

/// Rohdaten eines Layouts vor dem Auflösen der Doppeldeutigkeiten.
struct Raw {
    hkl: isize,
    abbr: String,
    region: String,
    language: String,
    layout: String,
}

thread_local! {
    /// Zuletzt gelesene Layoutliste samt aufgelösten Einträgen.
    static CACHE: RefCell<(Vec<isize>, Vec<Entry>)> = const { RefCell::new((Vec::new(), Vec::new())) };
    /// Letztes fremdes Vordergrundfenster (Ziel beim Umschalten aus dem Menü).
    static TARGET: RefCell<isize> = const { RefCell::new(0) };
}

/// Installierte Layouts (HKL), in der Reihenfolge von Windows.
pub fn installed() -> Vec<isize> {
    unsafe {
        let n = GetKeyboardLayoutList(0, null_mut());
        if n <= 0 {
            return Vec::new();
        }
        let mut buf: Vec<HKL> = vec![null_mut(); n as usize];
        let got = GetKeyboardLayoutList(n, buf.as_mut_ptr()).max(0) as usize;
        buf.truncate(got);
        buf.into_iter().map(|h| h as isize).collect()
    }
}

/// Ist `h` ein Fenster dieses Prozesses (Leiste, Menü, Startmenü …)?
unsafe fn ours(h: HWND) -> bool {
    let mut pid = 0u32;
    GetWindowThreadProcessId(h, &mut pid);
    pid == GetCurrentProcessId()
}

/// Fremdes Vordergrundfenster; liegt eigenes vorne (Menü offen), das zuletzt
/// gesehene fremde.
pub fn target() -> HWND {
    unsafe {
        let fg = GetForegroundWindow();
        if !fg.is_null() && !ours(fg) {
            TARGET.with(|t| *t.borrow_mut() = fg as isize);
            return fg;
        }
        let last = TARGET.with(|t| *t.borrow()) as HWND;
        if IsWindow(last) != 0 {
            last
        } else {
            null_mut()
        }
    }
}

/// Layout des Vordergrundfensters (0, wenn keins bestimmbar).
fn current_hkl() -> isize {
    unsafe {
        let h = target();
        if h.is_null() {
            return GetKeyboardLayout(0) as isize;
        }
        let thread = GetWindowThreadProcessId(h, null_mut());
        GetKeyboardLayout(thread) as isize
    }
}

/// Aktueller Stand; `None`, wenn höchstens ein Layout installiert ist
/// (dann zeigt Windows nichts an).
pub fn poll() -> Option<Indicator> {
    let list = installed();
    if list.len() < 2 {
        return None;
    }
    let entries = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.0 != list {
            let raw: Vec<Raw> = list.iter().map(|&h| unsafe { describe(h) }).collect();
            *c = (list.clone(), resolve(raw));
        }
        c.1.clone()
    });
    let current = resolve_current(current_hkl(), &entries);
    Some(Indicator { current, entries })
}

/// Konsolen & Co. melden manchmal ein Layout, das nicht (mehr) in der Liste
/// steht: dann nach der Sprache suchen, sonst das erste.
fn resolve_current(current: isize, entries: &[Entry]) -> isize {
    if entries.iter().any(|e| e.hkl == current) {
        return current;
    }
    entries.iter().find(|e| e.hkl & 0xFFFF == current & 0xFFFF).or(entries.first()).map_or(current, |e| e.hkl)
}

/// Stimmt die Anzeige noch (gleiche Layoutliste, gleiches aktives Layout)?
/// Ohne Speicheranforderung – für das häufige Nachsehen im Takt.
pub fn unchanged(prev: &Indicator) -> bool {
    let mut buf: [HKL; 32] = [null_mut(); 32];
    let n = unsafe { GetKeyboardLayoutList(buf.len() as i32, buf.as_mut_ptr()) };
    if n <= 0 || n as usize != prev.entries.len() {
        return false;
    }
    if !buf[..n as usize].iter().zip(&prev.entries).all(|(&h, e)| h as isize == e.hkl) {
        return false;
    }
    resolve_current(current_hkl(), &prev.entries) == prev.current
}

/// Doppelte Sprachkürzel unterscheiden: verschiedene Regionen → Regionskürzel
/// ("DE" / "CH"), sonst Kürzel des Layouts.
fn resolve(raw: Vec<Raw>) -> Vec<Entry> {
    raw.iter()
        .map(|r| {
            let same: Vec<&Raw> = raw.iter().filter(|o| o.abbr == r.abbr).collect();
            let sub = if same.len() < 2 {
                None
            } else if same.iter().filter(|o| o.region == r.region).count() == 1 && !r.region.is_empty() {
                Some(r.region.to_uppercase())
            } else {
                Some(layout_abbr(&r.layout))
            };
            Entry { hkl: r.hkl, abbr: r.abbr.clone(), sub, language: r.language.clone(), layout: r.layout.clone() }
        })
        .collect()
}

/// Kurzform eines Layoutnamens: Klammerinhalt ("Deutsch (IBM)" → "IBM"),
/// sonst die ersten drei Buchstaben.
fn layout_abbr(name: &str) -> String {
    let inner = name.rsplit_once('(').and_then(|(_, r)| r.split_once(')')).map(|(i, _)| i.trim());
    let base = inner.filter(|i| !i.is_empty()).unwrap_or(name);
    base.chars().filter(|c| c.is_alphanumeric()).take(3).collect::<String>().to_uppercase()
}

unsafe fn locale_info(name: &[u16], kind: u32) -> String {
    let mut buf = [0u16; 128];
    let n = GetLocaleInfoEx(name.as_ptr(), kind, buf.as_mut_ptr(), buf.len() as i32);
    if n <= 1 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize - 1])
}

/// Sprache und Layoutname eines HKL nachschlagen.
unsafe fn describe(hkl: isize) -> Raw {
    let lang = (hkl & 0xFFFF) as u32;
    let mut name = [0u16; 85];
    let n = LCIDToLocaleName(lang, name.as_mut_ptr(), name.len() as i32, 0);
    let locale: Vec<u16> = if n > 0 { name[..n as usize].to_vec() } else { vec![0] };
    // Windows zeigt die dreistelligen ISO-639-2-Kürzel ("DEU", "ENG"); die
    // älteren Windows-Kürzel (LOCALE_SABBREVLANGNAME, z. B. "ENU") nur als Ersatz.
    let mut abbr = locale_info(&locale, LOCALE_SISO639LANGNAME2);
    if abbr.is_empty() {
        abbr = locale_info(&locale, LOCALE_SABBREVLANGNAME);
    }
    let abbr = if abbr.is_empty() { format!("{lang:04X}") } else { abbr.to_uppercase() };
    let region = locale_info(&locale, LOCALE_SISO3166CTRYNAME);
    let mut language = locale_info(&locale, LOCALE_SLOCALIZEDDISPLAYNAME);
    if language.is_empty() {
        language = abbr.clone();
    }
    let layout = layout_name(hkl).unwrap_or_else(|| language.clone());
    Raw { hkl, abbr, region, language, layout }
}

const LAYOUTS: &str = r"SYSTEM\CurrentControlSet\Control\Keyboard Layouts";

/// KLID, der sich direkt aus dem HKL ergibt (ohne Registry). Obere 16 Bit
/// 0xExxx = IME: Der KLID ist das ganze HKL (z. B. "E0010411"); 0xFxxx =
/// Layout-ID, die erst in der Registry gesucht werden muss (`None`); sonst die
/// Geräte-ID selbst.
fn klid_direct(hkl: isize) -> Option<String> {
    let device = ((hkl as usize >> 16) & 0xFFFF) as u32;
    match device & 0xF000 {
        0xE000 => Some(format!("{:08X}", hkl as u32)),
        0xF000 => None,
        _ => Some(format!("{device:08X}")),
    }
}

/// Registry-Schlüssel (KLID, z. B. "00000407") eines HKL. Obere 16 Bit
/// 0xFxxx = Layout-ID ("Layout Id" in der Registry), siehe `klid_direct`.
unsafe fn klid(hkl: isize) -> Option<String> {
    if let Some(k) = klid_direct(hkl) {
        return Some(k);
    }
    let device = ((hkl as usize >> 16) & 0xFFFF) as u32;
    let wanted = device & 0x0FFF;
    let mut key: HKEY = null_mut();
    if RegOpenKeyExW(HKEY_LOCAL_MACHINE, w16(LAYOUTS).as_ptr(), 0, KEY_READ, &mut key) != ERROR_SUCCESS {
        return None;
    }
    let mut found = None;
    for i in 0.. {
        let mut buf = [0u16; 64];
        let mut len = buf.len() as u32;
        if RegEnumKeyExW(key, i, buf.as_mut_ptr(), &mut len, null_mut(), null_mut(), null_mut(), null_mut()) != ERROR_SUCCESS {
            break;
        }
        let sub = String::from_utf16_lossy(&buf[..len as usize]);
        let mut value = [0u16; 16];
        let mut bytes = (value.len() * 2) as u32;
        let ok = RegGetValueW(key, w16(&sub).as_ptr(), w16("Layout Id").as_ptr(), RRF_RT_REG_SZ, null_mut(), value.as_mut_ptr() as _, &mut bytes);
        if ok == ERROR_SUCCESS {
            let v = String::from_utf16_lossy(&value[..(bytes as usize / 2).saturating_sub(1)]);
            if u32::from_str_radix(v.trim(), 16).ok() == Some(wanted) {
                found = Some(sub);
                break;
            }
        }
    }
    RegCloseKey(key);
    found
}

/// Anzeigename eines Layouts in der Windows-Anzeigesprache ("Deutsch",
/// "Schweizerdeutsch").
unsafe fn layout_name(hkl: isize) -> Option<String> {
    let klid = klid(hkl)?;
    let mut key: HKEY = null_mut();
    if RegOpenKeyExW(HKEY_LOCAL_MACHINE, w16(&format!(r"{LAYOUTS}\{klid}")).as_ptr(), 0, KEY_READ, &mut key) != ERROR_SUCCESS {
        return None;
    }
    let mut buf = [0u16; 256];
    let mut bytes = 0u32;
    let mut name = None;
    if RegLoadMUIStringW(key, w16("Layout Display Name").as_ptr(), buf.as_mut_ptr(), (buf.len() * 2) as u32, &mut bytes, 0, std::ptr::null()) == ERROR_SUCCESS {
        let s = String::from_utf16_lossy(&buf[..(bytes as usize / 2)]);
        let s = s.trim_end_matches('\0').trim().to_string();
        if !s.is_empty() {
            name = Some(s);
        }
    }
    if name.is_none() {
        let mut bytes = (buf.len() * 2) as u32;
        if RegGetValueW(key, null_mut(), w16("Layout Text").as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr() as _, &mut bytes) == ERROR_SUCCESS {
            let s = String::from_utf16_lossy(&buf[..(bytes as usize / 2).saturating_sub(1)]);
            if !s.trim().is_empty() {
                name = Some(s.trim().to_string());
            }
        }
    }
    RegCloseKey(key);
    name
}

/// Voller Name für Tooltip und Menü: "Deutsch (Deutschland) – Tastatur: Deutsch".
pub fn full_name(e: &Entry) -> String {
    tf!("{} – Tastatur: {}", "{} – Keyboard: {}", e.language, e.layout)
}

/// Tooltip: voller Name plus Hinweis aufs Tastenkürzel.
pub fn tooltip(i: &Indicator) -> String {
    let name = i.active().map(full_name).unwrap_or_default();
    format!("{name}\n{}", t!("Umschalten: Windows-Taste + Leertaste", "Switch: Windows key + Space"))
}

/// Menü der Eingabesprache: alle Layouts (aktuelles mit Häkchen) und die
/// Spracheinstellungen. Schaltet das Fenster um, das vor dem Klick vorne war.
pub unsafe fn menu(owner: HWND, i: &Indicator) -> bool {
    use crate::menus::{self, Item};
    // Vor dem Menü merken: Das Menü holt die Leiste nach vorne.
    let prev = target();
    let names: Vec<String> = i.entries.iter().map(full_name).collect();
    let mut items: Vec<Item> = i.entries.iter().zip(&names).enumerate().map(|(n, (e, name))| Item::Check(1 + n, name, e.hkl == i.current)).collect();
    items.push(Item::Sep);
    const SETTINGS: usize = 1000;
    items.push(Item::Cmd(SETTINGS, t!("Spracheinstellungen", "Language settings")));
    let id = menus::popup(owner, &items);
    match id {
        0 => false,
        SETTINGS => {
            crate::util::shell_open("ms-settings:regionlanguage", "", SW_SHOWNORMAL);
            false
        }
        n => {
            let Some(e) = i.entries.get(n - 1) else { return false };
            if prev.is_null() {
                return false;
            }
            crate::util::force_foreground(prev);
            PostMessageW(prev, WM_INPUTLANGCHANGEREQUEST, 0, e.hkl);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(hkl: isize, abbr: &str, region: &str, layout: &str) -> Raw {
        Raw { hkl, abbr: abbr.into(), region: region.into(), language: format!("{abbr}-{region}"), layout: layout.into() }
    }

    #[test]
    fn current_layout_falls_back_to_language_then_first() {
        let e = resolve(vec![raw(0x04070407, "DEU", "DE", "Deutsch"), raw(0x04090409, "ENG", "US", "US")]);
        assert_eq!(resolve_current(0x04090409, &e), 0x04090409);
        // Unbekanntes Layout derselben Sprache (en-US mit anderem Gerät).
        assert_eq!(resolve_current(0xF0020409u32 as i32 as isize, &e), 0x04090409);
        // Ganz unbekannt: das erste.
        assert_eq!(resolve_current(0x040C040C, &e), 0x04070407);
    }

    #[test]
    fn single_languages_have_no_second_line() {
        let e = resolve(vec![raw(0x04070407, "DEU", "DE", "Deutsch"), raw(0x04090409, "ENG", "US", "US")]);
        assert!(e.iter().all(|e| e.sub.is_none()));
    }

    #[test]
    fn same_language_shows_region() {
        let e = resolve(vec![raw(0x04070407, "DEU", "DE", "Deutsch"), raw(0x08070807, "DEU", "CH", "Schweizerdeutsch"), raw(0x04090409, "ENG", "US", "US")]);
        assert_eq!(e[0].sub.as_deref(), Some("DE"));
        assert_eq!(e[1].sub.as_deref(), Some("CH"));
        assert_eq!(e[2].sub, None);
    }

    #[test]
    fn same_locale_shows_layout() {
        let e = resolve(vec![raw(0x04070407, "DEU", "DE", "Deutsch"), raw(0xF0070407u32 as isize, "DEU", "DE", "Deutsch (IBM)")]);
        assert_eq!(e[0].sub.as_deref(), Some("DEU"));
        assert_eq!(e[1].sub.as_deref(), Some("IBM"));
    }

    #[test]
    fn abbreviations() {
        assert_eq!(layout_abbr("Deutsch (IBM)"), "IBM");
        assert_eq!(layout_abbr("United States-International"), "UNI");
        assert_eq!(layout_abbr("US"), "US");
    }

    /// Liest nur: installierte Layouts dieses Rechners beschreiben.
    #[test]
    fn describes_installed_layouts() {
        for h in installed() {
            let r = unsafe { describe(h) };
            println!("{h:#x}: {} / {} / {} / {}", r.abbr, r.region, r.language, r.layout);
            assert!(!r.abbr.is_empty() && !r.language.is_empty() && !r.layout.is_empty());
        }
        if let Some(i) = poll() {
            println!("aktuell: {:?}\n{}", i.active().map(|e| (&e.abbr, &e.sub)), tooltip(&i));
        }
    }

    #[test]
    fn klid_from_device_id() {
        unsafe {
            assert_eq!(klid(0x04070407).as_deref(), Some("00000407"));
            assert_eq!(klid(0x08070407).as_deref(), Some("00000807"));
        }
    }
}

#[cfg(test)]
mod klid_tests {
    use super::klid_direct;

    #[test]
    fn klid_from_hkl() {
        // Deutsch: Gerät 0407.
        assert_eq!(klid_direct(0x0407_0407).as_deref(), Some("00000407"));
        // Japanisch (IME): KLID ist das ganze HKL.
        assert_eq!(klid_direct(0xE001_0411u32 as i32 as isize).as_deref(), Some("E0010411"));
        // Layout-ID (Dvorak u. Ä.): erst in der Registry nachschlagen.
        assert_eq!(klid_direct(0xF002_0409u32 as i32 as isize), None);
    }
}
