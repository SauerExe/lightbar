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

/// Eine Gruppe im Menü; `name` leer = "Aufgaben" (bzw. bekannte Kategorie).
#[derive(Clone, Debug)]
pub struct Category {
    pub name: String,
    pub entries: Vec<Entry>,
    /// Bekannte Kategorie an dieser Stelle (KDC_FREQUENT = 1, KDC_RECENT = 2):
    /// ohne Einträge in der Datei, die kommen aus der automatischen Liste.
    pub known: Option<u32>,
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

pub fn custom_exists(app_id: &str) -> bool {
    custom_path(app_id).is_some_and(|p| p.exists())
}

type CacheKey = (std::time::SystemTime, u64);
type Cache = std::collections::HashMap<std::path::PathBuf, (CacheKey, Vec<Category>)>;
static CACHE: std::sync::Mutex<Option<Cache>> = std::sync::Mutex::new(None);

/// Aufgaben, eigene und bekannte Kategorien (in Datei-Reihenfolge) der ersten
/// Kennung, zu der es eine Datei mit Inhalt gibt. `None` = die App hat keine
/// eigene Sprungliste; dann zeigt Windows "Zuletzt verwendet".
/// Zwischengespeichert, solange sich die Datei nicht ändert.
pub fn load(app_ids: &[String]) -> Option<Vec<Category>> {
    for id in app_ids {
        let Some(path) = custom_path(id) else { continue };
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let key: Option<CacheKey> = meta.modified().ok().map(|m| (m, meta.len()));
        let cached = key.and_then(|k| {
            let c = CACHE.lock().ok()?;
            c.as_ref()?.get(&path).filter(|(ck, _)| *ck == k).map(|(_, v)| v.clone())
        });
        let cats = match cached {
            Some(c) => c,
            None => {
                let Ok(bytes) = std::fs::read(&path) else { continue };
                let cats = unsafe { parse(&bytes) };
                if let (Some(k), Ok(mut c)) = (key, CACHE.lock()) {
                    c.get_or_insert_with(Default::default).insert(path.clone(), (k, cats.clone()));
                }
                cats
            }
        };
        if !cats.is_empty() {
            return Some(cats);
        }
    }
    None
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
        let (name, entries, known) = match kind {
            // Eigene Kategorie: Name, dann Einträge.
            0 => {
                let Some(len) = r.u16() else { break };
                let Some(name) = r.utf16(len as usize) else { break };
                let Some(n) = r.u32() else { break };
                let Some(entries) = read_entries(&stream, &mut r, n) else { break };
                (resolve(&name), entries, None)
            }
            // Bekannte Kategorie (1 = Häufig, 2 = Zuletzt verwendet): nur die
            // Stelle; die Einträge kommen aus der automatischen Liste.
            1 => {
                let Some(id) = r.u32() else { break };
                (String::new(), Vec::new(), Some(id))
            }
            // Aufgaben.
            2 => {
                let Some(n) = r.u32() else { break };
                let Some(entries) = read_entries(&stream, &mut r, n) else { break };
                (String::new(), entries, None)
            }
            _ => break,
        };
        if r.u32() != Some(FOOTER) {
            break;
        }
        if !entries.is_empty() || known.is_some() {
            out.push(Category { name, entries, known });
        }
    }
    out
}

/// `n` serialisierte Objekte ab der aktuellen Position. Trennlinien werden zu
/// Einträgen ohne Titel.
/// Alle `n` Objekte werden gelesen (sonst stünde der Leser mitten in der
/// Kategorie und die folgenden gingen verloren), behalten nur die ersten 100.
/// Geladen werden nur Verknüpfungen und Shell-Elemente: Ein unbekanntes Objekt
/// hat keine bekannte Länge – dann endet das Lesen (`None`).
unsafe fn read_entries(stream: &IStream, r: &mut Reader, n: u32) -> Option<Vec<Entry>> {
    const KEEP: usize = 100;
    let mut entries = Vec::new();
    // Jedes Objekt belegt mindestens seine Klassen-ID (16 Byte).
    let n = (n as usize).min(r.b.len().saturating_sub(r.pos) / 16);
    for _ in 0..n {
        if !allowed_class(r.b.get(r.pos..r.pos + 16)?) {
            return None;
        }
        stream.Seek(r.pos as i64, STREAM_SEEK_SET, None).ok()?;
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        OleLoadFromStream(stream, &windows::core::IUnknown::IID, &mut raw).ok()?;
        let object = windows::core::IUnknown::from_raw(raw);
        let mut pos = 0u64;
        stream.Seek(0, STREAM_SEEK_CUR, Some(&mut pos)).ok()?;
        if pos as usize <= r.pos {
            return None; // kein Fortschritt: kaputt
        }
        r.pos = pos as usize;
        if entries.len() < KEEP {
            if let Some(e) = entry_of(&object) {
                entries.push(e);
            }
        }
    }
    Some(entries)
}

/// CLSID_ShellLink {00021401-0000-0000-C000-000000000046}.
const CLSID_SHELL_LINK: u128 = 0x00021401_0000_0000_c000_000000000046;
/// CLSID_ShellItem {9AC9FBE1-E0A2-4AD6-B4EE-E212013EA917} (IShellItem, z. B.
/// angeheftete Dateien in eigenen Kategorien).
const CLSID_SHELL_ITEM: u128 = 0x9ac9fbe1_e0a2_4ad6_b4ee_e212013ea917;

/// Darf ein Objekt mit dieser serialisierten Klassen-ID (16 Byte wie von
/// `WriteClassStm`, d. h. GUID-Speicherlayout) geladen werden?
fn allowed_class(raw: &[u8]) -> bool {
    let Ok(raw): Result<[u8; 16], _> = raw.try_into() else { return false };
    let g = windows::core::GUID {
        data1: u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
        data2: u16::from_le_bytes([raw[4], raw[5]]),
        data3: u16::from_le_bytes([raw[6], raw[7]]),
        data4: [raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15]],
    };
    [CLSID_SHELL_LINK, CLSID_SHELL_ITEM].contains(&g.to_u128())
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
    let load = |s: &str| unsafe {
        let src = HSTRING::from(s);
        let mut buf = [0u16; 512];
        SHLoadIndirectString(PCWSTR(src.as_ptr()), &mut buf, None).ok()?;
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    };
    if let Some(t) = load(s) {
        return t;
    }
    // "@{Paket_1.2.3.4_x64__id?ms-resource://…}" mit veralteter Version (App
    // inzwischen aktualisiert): mit dem installierten Paket derselben Familie.
    if let Some((full, rest)) = s.strip_prefix("@{").and_then(|r| r.split_once('?')) {
        let parts: Vec<&str> = full.split('_').collect();
        if parts.len() >= 5 {
            for now in packages_of_family(&format!("{}_{}", parts[0], parts[parts.len() - 1])) {
                if let Some(t) = load(&format!("@{{{now}?{rest}")) {
                    return t;
                }
            }
        }
    }
    s.to_string()
}

/// Vollständige Namen der installierten Pakete einer Paketfamilie.
fn packages_of_family(family: &str) -> Vec<String> {
    use windows_sys::Win32::Storage::Packaging::Appx::GetPackagesByPackageFamily;
    let f = crate::util::w16(family);
    let (mut count, mut len) = (0u32, 0u32);
    unsafe {
        GetPackagesByPackageFamily(f.as_ptr(), &mut count, std::ptr::null_mut(), &mut len, std::ptr::null_mut());
        if count == 0 || len == 0 {
            return Vec::new();
        }
        let mut names = vec![std::ptr::null_mut::<u16>(); count as usize];
        let mut buf = vec![0u16; len as usize];
        if GetPackagesByPackageFamily(f.as_ptr(), &mut count, names.as_mut_ptr(), &mut len, buf.as_mut_ptr()) != 0 {
            return Vec::new();
        }
        names
            .iter()
            .take(count as usize)
            .map(|&p| {
                let n = (0..).take_while(|&i| *p.add(i) != 0).count();
                String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
            })
            .collect()
    }
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

// ---------------------------------------------------------------- Menü wie Windows

/// Bekannte Kategorien (ICustomDestinationList::AppendKnownCategory).
pub const KDC_FREQUENT: u32 = 1;
pub const KDC_RECENT: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Known {
    Pinned,
    Recent,
    Frequent,
}

/// Ein Abschnitt des Sprunglisten-Menüs, in Windows-Reihenfolge.
#[derive(Debug)]
pub enum Section {
    /// Angeheftet, Zuletzt verwendet oder Häufig: Shell-Einträge.
    Dests(Known, Vec<crate::destlist::Dest>),
    /// Eigene Kategorie der App mit Namen.
    Custom(String, Vec<Entry>),
    /// "Aufgaben".
    Tasks(Vec<Entry>),
}

/// Höchstzahl Einträge je Abschnitt wie bei Windows: Registrierungswert
/// `JumpListItems_Maximum` (Explorer\Advanced), sonst 10.
pub fn max_items() -> usize {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let (k, n) = (crate::util::w16(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced"), crate::util::w16("JumpListItems_Maximum"));
    let mut v = 0u32;
    let mut size = 4u32;
    let ok = unsafe { RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut v as *mut u32 as _, &mut size) } == 0;
    if ok && (1..=60).contains(&v) {
        v as usize
    } else {
        10
    }
}

/// Abschnitte wie bei Windows anordnen: "Angeheftet" oben, dann die Kategorien
/// der App in ihrer Reihenfolge (bekannte = Zuletzt verwendet/Häufig aus der
/// automatischen Liste), "Aufgaben" unten. Ohne eigene Sprungliste zeigt
/// Windows "Zuletzt verwendet". Leere Abschnitte entfallen. Im
/// Präsentationsmodus nur die Aufgaben (der Rest ist oft privat).
pub fn arrange(custom: Option<Vec<Category>>, pinned: Vec<crate::destlist::Dest>, recent: Vec<crate::destlist::Dest>, frequent: Vec<crate::destlist::Dest>, limit: usize, presenting: bool) -> Vec<Section> {
    let mut out = Vec::new();
    if !presenting && !pinned.is_empty() {
        out.push(Section::Dests(Known::Pinned, pinned));
    }
    let cats = custom.unwrap_or_else(|| vec![Category { name: String::new(), entries: Vec::new(), known: Some(KDC_RECENT) }]);
    let (mut recent, mut frequent) = (Some(recent), Some(frequent));
    let mut tasks = Vec::new();
    for c in cats {
        match c.known {
            Some(k) if !presenting => {
                let (kind, list) = if k == KDC_FREQUENT { (Known::Frequent, frequent.take()) } else { (Known::Recent, recent.take()) };
                if let Some(mut list) = list.filter(|l| !l.is_empty()) {
                    list.truncate(limit);
                    out.push(Section::Dests(kind, list));
                }
            }
            Some(_) => {}
            None if c.name.is_empty() => tasks.extend(c.entries),
            None if !presenting => {
                let mut e = c.entries;
                // Trennlinien zählen nicht, sonst wie Windows begrenzen.
                let mut n = 0;
                e.retain(|x| {
                    n += !x.title.is_empty() as usize;
                    n <= limit
                });
                out.push(Section::Custom(c.name, e));
            }
            None => {}
        }
    }
    if !tasks.is_empty() {
        out.push(Section::Tasks(tasks));
    }
    out
}

/// Kennungen, unter denen Windows die Sprunglisten einer App ablegen kann, in
/// dieser Reihenfolge: ausdrückliche AppUserModelID (Fenster oder Anheft-
/// Eintrag), sonst je exe: Explorer, die Kennung im App-Katalog (Verknüpfung
/// mit eigener AppUserModelID, z. B. Electron/Squirrel über Update.exe) und die
/// aus dem Pfad gebildete Standard-Kennung.
pub fn app_ids(aumid: Option<&str>, exes: &[String], catalog: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if let Some(a) = aumid {
        v.push(a.to_string());
    }
    for exe in exes.iter().filter(|e| !e.to_lowercase().ends_with("\\applicationframehost.exe")) {
        let lower = exe.to_lowercase();
        if lower.ends_with("\\windows\\explorer.exe") {
            v.push("Microsoft.Windows.Explorer".into());
        }
        v.extend(catalog(exe));
        // Squirrel (Discord, Slack, Teams classic …): Verknüpfung zeigt auf Update.exe.
        let path = std::path::Path::new(exe);
        for up in [path.parent(), path.parent().and_then(|p| p.parent())].into_iter().flatten() {
            let u = up.join("Update.exe");
            if u.exists() {
                v.extend(catalog(&u.to_string_lossy()));
            }
        }
        v.extend(crate::destlist::default_app_id(exe));
    }
    let mut seen = std::collections::HashSet::new();
    v.retain(|id| seen.insert(id.to_uppercase()));
    v
}

/// Alles für das Rechtsklick-Menü einer App: Abschnitte in Windows-Reihenfolge.
/// Fällt die eigene DestList-Auswertung aus, kommt "Zuletzt verwendet" über
/// `IApplicationDocumentLists`.
pub fn sections(ids: &[String], presenting: bool) -> Vec<Section> {
    let limit = max_items();
    let custom = load(ids);
    let (pinned, recent, frequent) = match crate::destlist::load(ids, limit) {
        Some(a) => (a.pinned.clone(), a.recent.clone(), a.frequent.clone()),
        None => {
            let api = |kind| -> Vec<crate::destlist::Dest> {
                ids.iter().map(|id| crate::destlist::api_list(id, kind, limit)).find(|l| !l.is_empty()).unwrap_or_default()
            };
            (Vec::new(), api(false), api(true))
        }
    };
    arrange(custom, pinned, recent, frequent, limit, presenting)
}

/// Diagnose für `--jumplist-test`: nur Kennungs-Art, Abschnittsnamen, Anzahlen
/// und Zeiten – keine Dateinamen.
pub fn diagnose(arg: Option<&str>) -> String {
    let mut text = String::new();
    let line = |ids: &[String], label: &str| -> String {
        let t0 = std::time::Instant::now();
        let s = sections(ids, false);
        let cold = t0.elapsed().as_micros();
        let t1 = std::time::Instant::now();
        let _ = sections(ids, false);
        let warm = t1.elapsed().as_micros();
        let auto = ids.iter().position(|id| crate::destlist::auto_path(id).is_some_and(|p| p.exists()));
        let cust = ids.iter().position(|id| custom_exists(id));
        let kinds = |i: Option<usize>| i.map(|i| format!("Kennung #{i} ({})", if ids[i].starts_with('{') { "Standard aus Pfad" } else { "AUMID/Katalog" })).unwrap_or_else(|| "-".into());
        let parts: Vec<String> = s
            .iter()
            .map(|x| match x {
                Section::Dests(k, l) => format!("{k:?} {}", l.len()),
                Section::Custom(n, l) => format!("[{n}] {}", l.iter().filter(|e| !e.title.is_empty()).count()),
                Section::Tasks(l) => format!("Aufgaben {}", l.iter().filter(|e| !e.title.is_empty()).count()),
            })
            .collect();
        let total = ids.iter().find_map(|id| crate::destlist::load(std::slice::from_ref(id), max_items())).map(|a| a.total).unwrap_or(0);
        format!("{label}: auto {} (gesamt {total}), custom {} | {} | {} ms kalt, {} µs warm\n", kinds(auto), kinds(cust), parts.join(", "), cold / 1000, warm)
    };
    text += &format!("Limit je Abschnitt: {}\n", max_items());
    match arg {
        Some(a) if !a.is_empty() => {
            if std::env::var("LIGHTBAR_DUMP").is_ok() {
                text += &crate::destlist::dump(a);
            }
            // exe: Kennungen wie für ein laufendes Fenster ohne AppUserModelID; der
            // App-Katalog wird hier selbst aufgebaut (das Startmenü läuft nicht).
            let ids = if a.to_lowercase().ends_with(".exe") {
                let catalog: Vec<(String, String)> = crate::apps::enumerate().iter().filter_map(|x| Some((crate::bar::normalize_exe(&unsafe { crate::apps::target_path(x) }?), x.id.clone()))).collect();
                app_ids(None, &[a.to_string()], |e| catalog.iter().find(|(p, _)| *p == crate::bar::normalize_exe(e)).map(|(_, id)| id.clone()))
            } else {
                vec![a.to_string()]
            };
            let label = std::path::Path::new(a).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            text += &line(&ids, &label);
            text += &compare_api(&ids);
        }
        _ => {
            // Alle Apps des Katalogs; nur die mit Sprunglisten-Datei.
            let apps = crate::apps::enumerate();
            let mut n = 0;
            for app in &apps {
                let exe = unsafe { crate::apps::target_path(app) };
                let aumid = (!app.id.contains('\\')).then_some(app.id.as_str());
                let mut ids = app_ids(aumid, &exe.into_iter().collect::<Vec<_>>(), |_| None);
                if aumid.is_none() {
                    ids.insert(0, app.id.clone());
                }
                let has = ids.iter().any(|id| crate::destlist::auto_path(id).is_some_and(|p| p.exists()) || custom_exists(id));
                if has {
                    n += 1;
                    text += &line(&ids, &app.name);
                }
            }
            text += &format!("{n} von {} Apps mit Sprungliste\n", apps.len());
            text += &compare_api(&["Microsoft.Windows.Explorer".to_string()]);
            text += &quick_access_check();
        }
    }
    text
}

/// Abgleich mit der Windows-Schnittstelle: wie viele angeheftete sie in
/// Zuletzt/Häufig mitliefert (die blendet Lightbar dort aus) und wie gut die
/// eigene DestList-Reihenfolge (Ersatz, falls die Schnittstelle ausfällt) zur
/// gezeigten passt.
fn compare_api(ids: &[String]) -> String {
    let limit = max_items();
    let (Some(a), Some(own)) = (crate::destlist::load(ids, limit), crate::destlist::load_own(ids, limit)) else { return "DestList: nicht lesbar\n".into() };
    let id = ids.iter().find(|id| crate::destlist::auto_path(id).is_some_and(|p| p.exists())).cloned().unwrap_or_default();
    let mut out = String::new();
    for (name, shown, mine, freq) in [("Zuletzt", &a.recent, &own.recent, false), ("Häufig", &a.frequent, &own.frequent, true)] {
        let api = crate::destlist::api_list(&id, freq, limit + a.pinned.len());
        let pinned_in_api = api.iter().filter(|p| a.pinned.iter().any(|m| m.parsing.eq_ignore_ascii_case(&p.parsing))).count();
        let same = shown.iter().zip(mine.iter()).filter(|(s, m)| s.parsing.eq_ignore_ascii_case(&m.parsing)).count();
        let anywhere = mine.iter().filter(|m| shown.iter().any(|s| s.parsing.eq_ignore_ascii_case(&m.parsing))).count();
        out += &format!("{name}: API {} (davon angeheftet {pinned_in_api}), gezeigt {}; eigene Reihenfolge: gleiche Stelle {same}, enthalten {anywhere}\n", api.len(), shown.len());
    }
    out
}

/// Explorer: Stimmen die angehefteten Einträge der DestList mit den an den
/// Schnellzugriff angehefteten Ordnern überein (Namen und Reihenfolge)?
fn quick_access_check() -> String {
    let Some(a) = crate::destlist::load(&["Microsoft.Windows.Explorer".to_string()], max_items()) else { return "Schnellzugriff: keine Explorer-DestList\n".into() };
    let qa = crate::apps::folder_items("shell:::{679f85cb-0220-4080-b29b-5540cc05aab6}", 200);
    let pinned: Vec<&str> = a.pinned.iter().map(|d| d.parsing.as_str()).collect();
    let found = pinned.iter().filter(|p| qa.iter().any(|(_, q)| q.eq_ignore_ascii_case(p))).count();
    // Positionen der angehefteten im Schnellzugriff: in derselben Reihenfolge?
    let pos: Vec<usize> = pinned.iter().filter_map(|p| qa.iter().position(|(_, q)| q.eq_ignore_ascii_case(p))).collect();
    let ordered = pos.windows(2).all(|w| w[0] < w[1]);
    format!("Schnellzugriff: {} Elemente, DestList angeheftet {}, davon im Schnellzugriff {found}, Positionen dort {pos:?}, gleiche Reihenfolge {ordered}
", qa.len(), pinned.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destlist::Dest;

    fn d(n: &str) -> Dest {
        Dest::from_path(n.into(), n.into())
    }
    fn e(t: &str) -> Entry {
        Entry { title: t.into(), target: "x".into(), args: String::new(), dir: String::new() }
    }
    fn names(s: &[Section]) -> Vec<String> {
        s.iter()
            .map(|x| match x {
                Section::Dests(k, l) => format!("{k:?}{}", l.len()),
                Section::Custom(n, l) => format!("{n}{}", l.len()),
                Section::Tasks(l) => format!("Tasks{}", l.len()),
            })
            .collect()
    }

    #[test]
    fn arrange_like_windows() {
        // Explorer: bekannte Kategorie "Häufig", angeheftete oben.
        let explorer = vec![Category { name: String::new(), entries: vec![], known: Some(KDC_FREQUENT) }];
        let s = arrange(Some(explorer), vec![d("p1"), d("p2")], vec![d("r")], (0..15).map(|i| d(&i.to_string())).collect(), 10, false);
        assert_eq!(names(&s), ["Pinned2", "Frequent10"]);
        // Ohne eigene Liste: "Zuletzt verwendet".
        let s = arrange(None, vec![], vec![d("a"), d("b")], vec![d("c")], 10, false);
        assert_eq!(names(&s), ["Recent2"]);
        // Eigene Kategorie, bekannte, Aufgaben unten – Reihenfolge der Datei.
        let cats = vec![
            Category { name: String::new(), entries: vec![e("Neues Fenster"), e("")], known: None },
            Category { name: "Meistbesucht".into(), entries: vec![e("a"), e("b")], known: None },
            Category { name: String::new(), entries: vec![], known: Some(KDC_RECENT) },
        ];
        let s = arrange(Some(cats), vec![d("p")], vec![d("r")], vec![], 10, false);
        assert_eq!(names(&s), ["Pinned1", "Meistbesucht2", "Recent1", "Tasks2"]);
        // Nur Aufgaben in der Datei: kein "Zuletzt verwendet".
        let s = arrange(Some(vec![Category { name: String::new(), entries: vec![e("t")], known: None }]), vec![], vec![d("r")], vec![], 10, false);
        assert_eq!(names(&s), ["Tasks1"]);
        // Präsentationsmodus: nur Aufgaben.
        let cats = vec![Category { name: "X".into(), entries: vec![e("a")], known: None }, Category { name: String::new(), entries: vec![e("t")], known: None }];
        let s = arrange(Some(cats), vec![d("p")], vec![d("r")], vec![], 10, true);
        assert_eq!(names(&s), ["Tasks1"]);
    }

    #[test]
    fn only_known_classes_load() {
        let link = [0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46];
        assert!(allowed_class(&link));
        let item = [0xE1, 0xFB, 0xC9, 0x9A, 0xA2, 0xE0, 0xD6, 0x4A, 0xB4, 0xEE, 0xE2, 0x12, 0x01, 0x3E, 0xA9, 0x17];
        assert!(allowed_class(&item));
        assert!(!allowed_class(&[0u8; 16]));
        assert!(!allowed_class(&link[..15]));
    }

    /// Serialisierte Verknüpfung wie in der Datei: Klassen-ID + IPersistStream-Daten.
    unsafe fn link_bytes(target: &str) -> Vec<u8> {
        use windows::Win32::System::Com::{CoCreateInstance, IPersistStream, CLSCTX_INPROC_SERVER, STREAM_SEEK_END};
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
        link.SetPath(&HSTRING::from(target)).unwrap();
        let stream = SHCreateMemStream(None).unwrap();
        link.cast::<IPersistStream>().unwrap().Save(&stream, true).unwrap();
        let mut size = 0u64;
        stream.Seek(0, STREAM_SEEK_END, Some(&mut size)).unwrap();
        stream.Seek(0, STREAM_SEEK_SET, None).unwrap();
        let mut data = vec![0u8; size as usize];
        let _ = stream.Read(data.as_mut_ptr() as _, size as u32, None);
        let mut out = vec![0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46];
        out.extend(data);
        out
    }

    #[test]
    fn categories_after_long_task_list_survive() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
            let one = link_bytes(r"C:\Windows\notepad.exe");
            let mut file = Vec::new();
            file.extend(2u32.to_le_bytes()); // Version
            file.extend(2u32.to_le_bytes()); // Kategorien
            file.extend(0u32.to_le_bytes());
            // Aufgaben mit 105 Einträgen …
            file.extend(2u32.to_le_bytes());
            file.extend(105u32.to_le_bytes());
            (0..105).for_each(|_| file.extend(&one));
            file.extend(FOOTER.to_le_bytes());
            // … danach eine eigene Kategorie mit einem Eintrag.
            file.extend(0u32.to_le_bytes());
            let name: Vec<u16> = "Zuletzt".encode_utf16().collect();
            file.extend((name.len() as u16).to_le_bytes());
            name.iter().for_each(|u| file.extend(u.to_le_bytes()));
            file.extend(1u32.to_le_bytes());
            file.extend(&one);
            file.extend(FOOTER.to_le_bytes());
            let cats = parse(&file);
            assert_eq!(cats.len(), 2);
            assert_eq!(cats[0].entries.len(), 100);
            assert_eq!(cats[1].name, "Zuletzt");
            assert_eq!(cats[1].entries.len(), 1);

            // Unbekannte Klasse: Lesen endet, nichts Falsches.
            let mut bad = file.clone();
            bad[20..36].fill(0xAB);
            assert!(parse(&bad).is_empty());
        }
    }

    #[test]
    fn hash_matches_explorer() {
        assert_eq!(super::app_id_hash("Microsoft.Windows.Explorer"), "f01b4d95cf55d32a");
    }
}
