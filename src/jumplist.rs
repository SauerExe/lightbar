//! App-eigene Sprunglisten-Einträge: "Aufgaben" (z. B. "Neues Fenster",
//! "Stummschalten") und eigene Kategorien, die Apps per ICustomDestinationList
//! anlegen. Windows bietet zum Lesen keine Schnittstelle; Explorer liest sie aus
//! `%APPDATA%\Microsoft\Windows\Recent\CustomDestinations\<Hash>.customDestinations-ms`.
//!
//! Dateiname: CRC-64 (Polynom 0x92C64265D32139A4, reflektiert, Startwert alle
//! Bits gesetzt, ohne Abschluss-XOR) über die großgeschriebene App-Kennung in
//! UTF-16LE, als 16 Hex-Ziffern. Geprüft an der bekannten Kennung von Explorer
//! (`Microsoft.Windows.Explorer` → `f01b4d95cf55d32a`).
//!
//! Aufbau (wie in der Forensik dokumentiert, z. B. JLECmd): Kopf (Version,
//! Anzahl Kategorien, reserviert), dann je Kategorie ein Typ – 0 = eigene
//! Kategorie mit Namen, 1 = bekannte Kategorie (Zuletzt/Häufig, ohne Einträge
//! in der Datei), 2 = Aufgaben – gefolgt von den Einträgen und der Kennung
//! 0xBABFFBAB. Jeder Eintrag ist ein serialisiertes Objekt (Klassen-ID + Daten),
//! meist eine Verknüpfung; das liest Windows selbst (`OleLoadFromStream`).
//! Unbekanntes oder Kaputtes beendet das Lesen – lieber weniger Einträge als
//! falsche.

use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Storage::EnhancedStorage::PKEY_Title;
use windows::Win32::System::Com::{IStream, STREAM_SEEK_CUR, STREAM_SEEK_SET};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Ole::OleLoadFromStream;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::*;

/// Kennung der Trennlinie in Aufgabenlisten (PKEY_AppUserModel_IsDestListSeparator).
const PKEY_SEPARATOR: windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY = windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 6,
};

const FOOTER: u32 = 0xBABF_FBAB;

/// Ein Eintrag: Titel und was beim Klick gestartet wird.
#[derive(Clone, Debug)]
pub struct Entry {
    pub title: String,
    /// Programm bzw. Datei/Shell-Pfad.
    pub target: String,
    pub args: String,
    pub dir: String,
}

/// Eine Gruppe im Menü; `name` leer = "Aufgaben".
#[derive(Clone, Debug)]
pub struct Category {
    pub name: String,
    pub entries: Vec<Entry>,
}

/// Dateiname-Hash der Sprunglisten einer App-Kennung.
pub fn app_id_hash(app_id: &str) -> String {
    let mut crc: u64 = u64::MAX;
    for unit in app_id.to_uppercase().encode_utf16() {
        for b in unit.to_le_bytes() {
            crc ^= b as u64;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0x92C6_4265_D321_39A4 } else { crc >> 1 };
            }
        }
    }
    format!("{crc:016x}")
}

fn custom_path(app_id: &str) -> Option<std::path::PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(std::path::Path::new(&appdata).join("Microsoft\\Windows\\Recent\\CustomDestinations").join(format!("{}.customDestinations-ms", app_id_hash(app_id))))
}

/// Aufgaben und eigene Kategorien der ersten Kennung, zu der es eine Datei gibt.
pub fn load(app_ids: &[String]) -> Vec<Category> {
    for id in app_ids {
        let Some(path) = custom_path(id) else { continue };
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let cats = unsafe { parse(&bytes) };
        if !cats.is_empty() {
            return cats;
        }
    }
    Vec::new()
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u16(&mut self) -> Option<u16> {
        let v = u16::from_le_bytes(self.b.get(self.pos..self.pos + 2)?.try_into().ok()?);
        self.pos += 2;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        let v = u32::from_le_bytes(self.b.get(self.pos..self.pos + 4)?.try_into().ok()?);
        self.pos += 4;
        Some(v)
    }
    fn utf16(&mut self, chars: usize) -> Option<String> {
        let bytes = self.b.get(self.pos..self.pos + chars * 2)?;
        self.pos += chars * 2;
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(String::from_utf16_lossy(&units))
    }
}

unsafe fn parse(bytes: &[u8]) -> Vec<Category> {
    let mut out = Vec::new();
    let Some(stream) = SHCreateMemStream(Some(bytes)) else { return out };
    let mut r = Reader { b: bytes, pos: 0 };
    let (Some(_version), Some(count), Some(_reserved)) = (r.u32(), r.u32(), r.u32()) else { return out };
    for _ in 0..count.min(64) {
        let Some(kind) = r.u32() else { break };
        let (name, entries) = match kind {
            // Eigene Kategorie: Name, dann Einträge.
            0 => {
                let Some(len) = r.u16() else { break };
                let Some(name) = r.utf16(len as usize) else { break };
                let Some(n) = r.u32() else { break };
                let Some(entries) = read_entries(&stream, &mut r, n) else { break };
                (resolve(&name), entries)
            }
            // Bekannte Kategorie (Zuletzt verwendet/Häufig): kommt aus der
            // automatischen Liste, die Lightbar schon zeigt.
            1 => {
                if r.u32().is_none() {
                    break;
                }
                (String::new(), Vec::new())
            }
            // Aufgaben.
            2 => {
                let Some(n) = r.u32() else { break };
                let Some(entries) = read_entries(&stream, &mut r, n) else { break };
                (String::new(), entries)
            }
            _ => break,
        };
        if r.u32() != Some(FOOTER) {
            break;
        }
        if !entries.is_empty() {
            out.push(Category { name, entries });
        }
    }
    out
}

/// `n` serialisierte Objekte ab der aktuellen Position. Trennlinien werden zu
/// Einträgen ohne Titel.
unsafe fn read_entries(stream: &IStream, r: &mut Reader, n: u32) -> Option<Vec<Entry>> {
    let mut entries = Vec::new();
    for _ in 0..n.min(100) {
        stream.Seek(r.pos as i64, STREAM_SEEK_SET, None).ok()?;
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        OleLoadFromStream(stream, &windows::core::IUnknown::IID, &mut raw).ok()?;
        let object = windows::core::IUnknown::from_raw(raw);
        let mut pos = 0u64;
        stream.Seek(0, STREAM_SEEK_CUR, Some(&mut pos)).ok()?;
        r.pos = pos as usize;
        if let Some(e) = entry_of(&object) {
            entries.push(e);
        }
    }
    Some(entries)
}

/// Text aus einem Eigenschaftsspeicher.
unsafe fn prop(store: &IPropertyStore, key: &windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY) -> Option<String> {
    let v = store.GetValue(key).ok()?;
    let p = PropVariantToStringAlloc(&v).ok()?;
    let s = p.to_string().ok();
    windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _));
    s.filter(|s| !s.is_empty())
}

/// Indirekte Texte ("@shell32.dll,-123", "@{Paket?ms-resource://…}") auflösen.
fn resolve(s: &str) -> String {
    if !s.starts_with('@') {
        return s.to_string();
    }
    unsafe {
        let src = HSTRING::from(s);
        let mut buf = [0u16; 512];
        if SHLoadIndirectString(PCWSTR(src.as_ptr()), &mut buf, None).is_ok() {
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            return String::from_utf16_lossy(&buf[..n]);
        }
    }
    s.to_string()
}

unsafe fn entry_of(object: &windows::core::IUnknown) -> Option<Entry> {
    if let Ok(link) = object.cast::<IShellLinkW>() {
        let store = object.cast::<IPropertyStore>().ok();
        if let Some(store) = &store {
            if prop(store, &PKEY_SEPARATOR).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
                return Some(Entry { title: String::new(), target: String::new(), args: String::new(), dir: String::new() });
            }
        }
        let mut path = [0u16; 1024];
        let _ = link.GetPath(&mut path, std::ptr::null_mut(), 0);
        let mut args = [0u16; 2048];
        let _ = link.GetArguments(&mut args);
        let mut dir = [0u16; 1024];
        let _ = link.GetWorkingDirectory(&mut dir);
        let text = |b: &[u16]| String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]);
        let target = text(&path);
        let title = store
            .as_ref()
            .and_then(|s| prop(s, &PKEY_Title))
            .map(|t| resolve(&t))
            .or_else(|| {
                let mut d = [0u16; 512];
                link.GetDescription(&mut d).ok().map(|_| text(&d)).filter(|t| !t.is_empty())
            })
            .or_else(|| std::path::Path::new(&target).file_stem().map(|s| s.to_string_lossy().into_owned()))?;
        if target.is_empty() {
            return None;
        }
        return Some(Entry { title, target, args: text(&args), dir: text(&dir) });
    }
    // Shell-Element (z. B. eine angeheftete Datei in einer eigenen Kategorie).
    let item = object.cast::<IShellItem>().ok()?;
    let name = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
    let title = name.to_string().ok();
    windows::Win32::System::Com::CoTaskMemFree(Some(name.0 as _));
    let path = item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING).ok()?;
    let target = path.to_string().ok();
    windows::Win32::System::Com::CoTaskMemFree(Some(path.0 as _));
    Some(Entry { title: title?, target: target?, args: String::new(), dir: String::new() })
}

/// Eintrag starten wie Explorer: Ziel mit Argumenten im Arbeitsverzeichnis.
pub fn launch(e: &Entry) {
    use crate::util::w16;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (t, a, d) = (w16(&e.target), w16(&e.args), w16(&e.dir));
    let opt = |s: &str, w: &Vec<u16>| if s.is_empty() { std::ptr::null() } else { w.as_ptr() };
    unsafe {
        ShellExecuteW(std::ptr::null_mut(), w16("open").as_ptr(), t.as_ptr(), opt(&e.args, &a), opt(&e.dir, &d), SW_SHOWNORMAL);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hash_matches_explorer() {
        assert_eq!(super::app_id_hash("Microsoft.Windows.Explorer"), "f01b4d95cf55d32a");
    }
}
