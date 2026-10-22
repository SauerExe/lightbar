//! Automatische Sprunglisten (`…\Recent\AutomaticDestinations\<Hash>.automaticDestinations-ms`)
//! selbst lesen: angeheftete Einträge (mit Reihenfolge), "Zuletzt verwendet" und
//! "Häufig" aus einer Quelle. `IApplicationDocumentLists` kennt keine angehefteten
//! Einträge; bei Explorer sind das genau die an den Schnellzugriff angehefteten
//! Ordner.
//!
//! Die Datei ist ein OLE-Verbunddokument (Compound File Binary, MS-CFB). Darin:
//! je Eintrag ein Datenstrom mit hexadezimaler Nummer als Name, der eine
//! Verknüpfung (Shell-Link-Binärformat) enthält, und der Datenstrom `DestList`
//! (Aufbau wie in der Forensik dokumentiert, z. B. JLECmd):
//!
//! Kopf, 32 Byte: Version (u32; 1 = Windows 7/8, 3/4 = Windows 10, 6 = Windows 11 neu), Anzahl
//! Einträge, Anzahl angehefteter Einträge, Zähler (f32), letzte vergebene
//! Nummer, …
//!
//! Eintrag in Version 3/4: Prüfsumme (8), vier Objekt-GUIDs (64), Rechnername
//! (16, ASCII) → ab Byte 88: Nummer (u32, = Datenstrom-Name), Rang (f32, je
//! höher, desto häufiger), letzter Zugriff (FILETIME), Anheft-Position (i32;
//! -1 = nicht angeheftet, sonst 0-basierte Position), unbekannt (i32), Anzahl
//! Zugriffe (u32), unbekannt (8), Pfadlänge in Zeichen (u16), Pfad (UTF-16),
//! danach 4 Byte. Version 1 endet nach der Anheft-Position mit Pfadlänge und
//! Pfad ab Byte 108 (ohne die 4 Byte).
//!
//! Version 6 (Windows 11, Build 26xxx; auf diesem Rechner nachgemessen): hinter
//! der Nummer 4 Byte mehr, dann Rang (96), letzter Zugriff (100),
//! Anheft-Position (108), unbekannt (112, 116, 120..128), Pfadlänge (128), Pfad,
//! 4 Byte. Die Anheft-Reihenfolge der Explorer-Liste ist dieselbe wie die der
//! an den Schnellzugriff angehefteten Ordner.
//!
//! "Zuletzt verwendet"/"Häufig" kommen in der Reihenfolge der Windows-
//! Schnittstelle (Häufig rechnet Windows mit Zeitabfall, das steht so nicht in
//! der Datei); die DestList liefert die angehefteten und ist Ersatz.
//!
//! Gelesen wird nur; geparste Dateien bleiben nach Änderungszeit und Größe im
//! Speicher, damit das Rechtsklick-Menü auch bei großen Listen sofort aufgeht.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

// ---------------------------------------------------------------- Verbunddokument

const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
const MAX_REGULAR: u32 = 0xFFFF_FFFA;

/// Minimaler, nur lesender MS-CFB-Leser: Datenströme der Wurzel nach Namen.
pub struct Cfb<'a> {
    b: &'a [u8],
    sector: usize,
    mini_sector: usize,
    cutoff: u64,
    fat: Vec<u32>,
    minifat: Vec<u32>,
    /// Inhalt des Mini-Datenstroms (enthält alle kleinen Datenströme).
    mini: Vec<u8>,
    /// (Name, Startsektor, Größe) aller Datenströme.
    streams: Vec<(String, u32, u64)>,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}
fn f32_at(b: &[u8], at: usize) -> Option<f32> {
    Some(f32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

impl<'a> Cfb<'a> {
    pub fn parse(b: &'a [u8]) -> Option<Cfb<'a>> {
        if b.get(..8)? != [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1] {
            return None;
        }
        let shift = u16_at(b, 0x1E)?;
        let mini_shift = u16_at(b, 0x20)?;
        if !(9..=12).contains(&shift) || mini_shift > shift {
            return None;
        }
        let mut c = Cfb {
            b,
            sector: 1 << shift,
            mini_sector: 1 << mini_shift,
            cutoff: u32_at(b, 0x38)? as u64,
            fat: Vec::new(),
            minifat: Vec::new(),
            mini: Vec::new(),
            streams: Vec::new(),
        };
        // FAT-Sektoren: 109 im Kopf, weitere in der DIFAT-Kette.
        let fat_count = u32_at(b, 0x2C)? as usize;
        let mut fat_sectors: Vec<u32> = (0..109).filter_map(|i| u32_at(b, 0x4C + i * 4)).filter(|&s| s <= MAX_REGULAR).collect();
        let mut difat = u32_at(b, 0x44)?;
        let per = c.sector / 4 - 1;
        let mut guard = 0;
        while difat <= MAX_REGULAR && fat_sectors.len() < fat_count && guard < 10_000 {
            let s = c.sector_bytes(difat)?;
            fat_sectors.extend((0..per).filter_map(|i| u32_at(s, i * 4)).filter(|&x| x <= MAX_REGULAR));
            difat = u32_at(s, per * 4)?;
            guard += 1;
        }
        fat_sectors.truncate(fat_count);
        for s in fat_sectors {
            let bytes = c.sector_bytes(s)?;
            c.fat.extend(bytes.chunks_exact(4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]])));
        }
        // Verzeichnis: 128-Byte-Einträge; Datenströme flach einsammeln (alle
        // Sprunglisten-Ströme liegen direkt in der Wurzel).
        let dir = c.chain(u32_at(b, 0x30)?, None)?;
        let mut root: Option<(u32, u64)> = None;
        for e in dir.chunks_exact(128) {
            let kind = e[66];
            let name_len = (u16_at(e, 64)? as usize).min(64);
            let units: Vec<u16> = e[..name_len.saturating_sub(2)].chunks_exact(2).map(|x| u16::from_le_bytes([x[0], x[1]])).collect();
            let start = u32_at(e, 116)?;
            let size = u64_at(e, 120)? & if shift == 9 { 0xFFFF_FFFF } else { u64::MAX };
            match kind {
                2 => c.streams.push((String::from_utf16_lossy(&units), start, size)),
                5 => root = Some((start, size)),
                _ => {}
            }
        }
        if let Some((start, size)) = root {
            if size > 0 {
                c.mini = c.chain(start, Some(size))?;
            }
        }
        let mf = u32_at(b, 0x3C)?;
        if mf <= MAX_REGULAR {
            let bytes = c.chain(mf, None)?;
            c.minifat = bytes.chunks_exact(4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]])).collect();
        }
        Some(c)
    }

    fn sector_bytes(&self, s: u32) -> Option<&'a [u8]> {
        let at = (s as usize + 1).checked_mul(self.sector)?;
        self.b.get(at..at + self.sector).or_else(|| self.b.get(at..)).filter(|x| !x.is_empty())
    }

    /// Sektorkette ab `start` aneinanderhängen (optional auf `size` gekürzt).
    fn chain(&self, start: u32, size: Option<u64>) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut s = start;
        let mut steps = 0usize;
        while s != ENDOFCHAIN {
            if s > MAX_REGULAR || steps > self.fat.len() {
                return None;
            }
            out.extend_from_slice(self.sector_bytes(s)?);
            s = *self.fat.get(s as usize)?;
            steps += 1;
            if size.is_some_and(|n| out.len() as u64 >= n) {
                break;
            }
        }
        if let Some(n) = size {
            out.truncate(n as usize);
        }
        Some(out)
    }

    fn mini_chain(&self, start: u32, size: u64) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut s = start;
        let mut steps = 0usize;
        while s != ENDOFCHAIN && (out.len() as u64) < size {
            if s > MAX_REGULAR || steps > self.minifat.len() {
                return None;
            }
            let at = s as usize * self.mini_sector;
            out.extend_from_slice(self.mini.get(at..at + self.mini_sector)?);
            s = *self.minifat.get(s as usize)?;
            steps += 1;
        }
        out.truncate(size as usize);
        Some(out)
    }

    pub fn stream(&self, name: &str) -> Option<Vec<u8>> {
        let (_, start, size) = self.streams.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(name))?;
        if *size == 0 {
            return Some(Vec::new());
        }
        if *size < self.cutoff {
            self.mini_chain(*start, *size)
        } else {
            self.chain(*start, Some(*size))
        }
    }
}

// ---------------------------------------------------------------- DestList

#[derive(Clone, Debug, PartialEq)]
pub struct DestEntry {
    /// Nummer = Name des Datenstroms mit der Verknüpfung (hexadezimal).
    pub id: u32,
    /// Rang für "Häufig" (je höher, desto häufiger/jünger).
    pub score: f32,
    /// Letzter Zugriff (FILETIME).
    pub last: u64,
    /// Anheft-Position (0-basiert) oder None.
    pub pin: Option<u32>,
    pub count: u32,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DestList {
    pub version: u32,
    pub entries: Vec<DestEntry>,
}

impl DestList {
    /// Angeheftete Einträge in Anheft-Reihenfolge.
    pub fn pinned(&self) -> Vec<&DestEntry> {
        let mut v: Vec<&DestEntry> = self.entries.iter().filter(|e| e.pin.is_some()).collect();
        v.sort_by_key(|e| e.pin);
        v
    }
    /// Nicht angeheftete, jüngste zuerst ("Zuletzt verwendet").
    pub fn recent(&self) -> Vec<&DestEntry> {
        let mut v: Vec<&DestEntry> = self.entries.iter().filter(|e| e.pin.is_none()).collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.last));
        v
    }
    /// Nicht angeheftete, höchster Rang zuerst ("Häufig").
    pub fn frequent(&self) -> Vec<&DestEntry> {
        let mut v: Vec<&DestEntry> = self.entries.iter().filter(|e| e.pin.is_none()).collect();
        v.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(b.last.cmp(&a.last)));
        v
    }
}

/// Feld-Offsets eines Eintrags je DestList-Version (ab Eintragsbeginn).
struct Layout {
    score: usize,
    last: usize,
    pin: usize,
    count: Option<usize>,
    path: usize,
    tail: usize,
}

impl Layout {
    fn of(version: u32) -> Layout {
        match version {
            // Windows 7/8.
            0..=2 => Layout { score: 92, last: 96, pin: 104, count: None, path: 108, tail: 0 },
            // Windows 10 / frühe Windows-11-Versionen.
            3..=4 => Layout { score: 92, last: 96, pin: 104, count: Some(112), path: 124, tail: 4 },
            // Neuere Windows-11-Versionen (Version 6 auf Build 26xxx): hinter der
            // Nummer 4 Byte mehr (Nummer 64-bit), alles Weitere verschoben.
            _ => Layout { score: 96, last: 100, pin: 108, count: Some(112), path: 128, tail: 4 },
        }
    }
}

/// `DestList`-Datenstrom auswerten. Kaputtes Ende → die bis dahin gelesenen Einträge.
pub fn parse_destlist(b: &[u8]) -> Option<DestList> {
    let version = u32_at(b, 0)?;
    let count = u32_at(b, 4)? as usize;
    let l = Layout::of(version);
    let mut entries = Vec::new();
    let mut pos = 32;
    for _ in 0..count.min(10_000) {
        let e = b.get(pos..)?;
        let Some(len) = u16_at(e, l.path) else { break };
        let end = l.path + 2 + len as usize * 2;
        let Some(raw) = e.get(l.path + 2..end) else { break };
        let units: Vec<u16> = raw.chunks_exact(2).map(|x| u16::from_le_bytes([x[0], x[1]])).collect();
        let (Some(id), Some(score), Some(last), Some(pin)) = (u32_at(e, 88), f32_at(e, l.score), u64_at(e, l.last), i32_at(e, l.pin)) else { break };
        entries.push(DestEntry {
            id,
            score,
            last,
            pin: (pin >= 0).then_some(pin as u32),
            count: l.count.and_then(|at| u32_at(e, at)).unwrap_or(0),
            path: String::from_utf16_lossy(&units),
        });
        let tail = l.tail;
        pos += end + tail;
    }
    Some(DestList { version, entries })
}

/// Datenstrom-Name zur Eintragsnummer.
fn stream_name(id: u32) -> String {
    format!("{id:x}")
}

// ---------------------------------------------------------------- Shell-Einträge

/// Ein anzeigbarer Eintrag: Name, Shell-Pfad und die Element-ID-Liste
/// (zum Öffnen genau wie Explorer).
#[derive(Clone, Debug)]
pub struct Dest {
    pub name: String,
    pub parsing: String,
    pidl: Vec<u8>,
}

impl Dest {
    #[cfg(test)]
    /// Eintrag nur aus Name und Shell-Pfad (geöffnet wird dann über den Pfad).
    pub fn from_path(name: String, parsing: String) -> Dest {
        Dest { name, parsing, pidl: Vec::new() }
    }
}

#[derive(Default, Debug)]
pub struct AutoList {
    pub pinned: Vec<Dest>,
    pub recent: Vec<Dest>,
    pub frequent: Vec<Dest>,
    /// Gesamtzahl Einträge in der Datei (Diagnose).
    pub total: usize,
}

pub fn auto_path(app_id: &str) -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(Path::new(&appdata).join("Microsoft\\Windows\\Recent\\AutomaticDestinations").join(format!("{}.automaticDestinations-ms", crate::jumplist::app_id_hash(app_id))))
}

type CacheKey = (SystemTime, u64, usize);
type Cache = HashMap<PathBuf, (CacheKey, Arc<AutoList>)>;
static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// Automatische Liste der ersten Kennung, zu der es eine Datei gibt; je
/// Abschnitt höchstens `limit` Einträge (angeheftete: alle, wie bei Windows).
/// Aus dem Zwischenspeicher, solange sich die Datei nicht geändert hat.
pub fn load(app_ids: &[String], limit: usize) -> Option<Arc<AutoList>> {
    let (id, path) = app_ids.iter().filter_map(|id| Some((id, auto_path(id)?))).find(|(_, p)| p.exists())?;
    let meta = std::fs::metadata(&path).ok()?;
    let key: CacheKey = (meta.modified().ok()?, meta.len(), limit);
    if let Some(hit) = CACHE.lock().ok()?.get_or_insert_with(HashMap::new).get(&path).filter(|(k, _)| *k == key) {
        return Some(hit.1.clone());
    }
    let bytes = std::fs::read(&path).ok()?;
    let list = Arc::new(unsafe { build(&bytes, id, limit, true) }?);
    if let Ok(mut c) = CACHE.lock() {
        c.get_or_insert_with(HashMap::new).insert(path, (key, list.clone()));
    }
    Some(list)
}

/// Angeheftete aus der DestList (die Windows-Schnittstelle kennt sie nicht).
/// "Zuletzt verwendet"/"Häufig" in der Reihenfolge der Windows-Schnittstelle
/// (sie rechnet die Häufigkeit mit Zeitabfall, wie das Taskleisten-Menü),
/// ohne die angehefteten – die zeigt Windows nur oben. Liefert die
/// Schnittstelle nichts, die eigene Reihenfolge aus der DestList.
/// Wie `load`, aber ohne Zwischenspeicher und nur mit der eigenen
/// DestList-Reihenfolge (Diagnose).
pub fn load_own(app_ids: &[String], limit: usize) -> Option<AutoList> {
    let (id, path) = app_ids.iter().filter_map(|id| Some((id, auto_path(id)?))).find(|(_, p)| p.exists())?;
    unsafe { build(&std::fs::read(path).ok()?, id, limit, false) }
}

unsafe fn build(bytes: &[u8], app_id: &str, limit: usize, use_api: bool) -> Option<AutoList> {
    let cfb = Cfb::parse(bytes)?;
    let dl = parse_destlist(&cfb.stream("DestList")?)?;
    let mut resolved: HashMap<u32, Option<Dest>> = HashMap::new();
    let mut take = |entries: Vec<&DestEntry>, max: usize| -> Vec<Dest> {
        let mut out = Vec::new();
        for e in entries {
            if out.len() >= max {
                break;
            }
            let d = resolved.entry(e.id).or_insert_with(|| resolve(&cfb, e)).clone();
            out.extend(d);
        }
        out
    };
    let pinned = take(dl.pinned(), 100);
    let is_pinned = |d: &Dest| pinned.iter().any(|p| p.parsing.eq_ignore_ascii_case(&d.parsing));
    let mut lists = [Vec::new(), Vec::new()];
    for (i, frequent) in [false, true].into_iter().enumerate() {
        let mut l: Vec<Dest> = if use_api { api_list(app_id, frequent, limit + pinned.len()).into_iter().filter(|d| !is_pinned(d)).collect() } else { Vec::new() };
        if l.is_empty() {
            l = take(if frequent { dl.frequent() } else { dl.recent() }, limit);
        }
        l.truncate(limit);
        lists[i] = l;
    }
    let [recent, frequent] = lists;
    Some(AutoList { pinned, recent, frequent, total: dl.entries.len() })
}

/// Eine Liste der Windows-Schnittstelle `IApplicationDocumentLists`
/// ("Häufig" oder "Zuletzt verwendet"), angeheftete eingeschlossen.
pub fn api_list(app_id: &str, frequent: bool, max: usize) -> Vec<Dest> {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::Common::IObjectArray;
    use windows::Win32::UI::Shell::*;
    unsafe {
        let run = || -> windows::core::Result<Vec<Dest>> {
            let lists: IApplicationDocumentLists = CoCreateInstance(&ApplicationDocumentLists, None, CLSCTX_INPROC_SERVER)?;
            lists.SetAppID(&HSTRING::from(app_id))?;
            let array = lists.GetList::<IObjectArray>(if frequent { ADLT_FREQUENT } else { ADLT_RECENT }, max as u32)?;
            let mut out = Vec::new();
            for i in 0..array.GetCount().unwrap_or(0) {
                let Ok(item) = array.GetAt::<IShellItem>(i) else { continue };
                let Ok(pidl) = SHGetIDListFromObject(&item) else { continue };
                let bytes = std::slice::from_raw_parts(pidl as *const u8, ILGetSize(Some(pidl)) as usize).to_vec();
                CoTaskMemFree(Some(pidl as _));
                if let Some(d) = dest_of(&item, bytes, String::new()) {
                    out.push(d);
                }
            }
            Ok(out)
        };
        run().unwrap_or_default()
    }
}

/// Name und Shell-Pfad eines Elements.
unsafe fn dest_of(item: &windows::Win32::UI::Shell::IShellItem, pidl: Vec<u8>, fallback: String) -> Option<Dest> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::*;
    let text = |kind| {
        let p = item.GetDisplayName(kind).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s.filter(|s| !s.is_empty())
    };
    let name = text(SIGDN_NORMALDISPLAY)?;
    let parsing = text(SIGDN_DESKTOPABSOLUTEPARSING).unwrap_or(fallback);
    Some(Dest { name, parsing, pidl })
}

/// Verknüpfung des Eintrags laden und als Shell-Element auflösen; ohne
/// Verknüpfung über den Pfad aus der DestList.
unsafe fn resolve(cfb: &Cfb, e: &DestEntry) -> Option<Dest> {
    use windows::core::{Interface, HSTRING};
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, IPersistStream, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::*;
    let from_link = || -> Option<*mut ITEMIDLIST> {
        let data = cfb.stream(&stream_name(e.id))?;
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let stream = SHCreateMemStream(Some(&data))?;
        link.cast::<IPersistStream>().ok()?.Load(&stream).ok()?;
        let pidl = link.GetIDList().ok()?;
        (!pidl.is_null()).then_some(pidl)
    };
    let pidl = from_link().or_else(|| {
        let mut p: *mut ITEMIDLIST = std::ptr::null_mut();
        SHParseDisplayName(&HSTRING::from(e.path.as_str()), None, &mut p, 0, None).ok()?;
        (!p.is_null()).then_some(p)
    })?;
    let size = ILGetSize(Some(pidl)) as usize;
    let bytes = std::slice::from_raw_parts(pidl as *const u8, size).to_vec();
    let item: Option<IShellItem> = SHCreateItemFromIDList(pidl).ok();
    CoTaskMemFree(Some(pidl as _));
    dest_of(&item?, bytes, e.path.clone())
}

/// Eintrag öffnen wie Explorer: Standardaktion des Shell-Elements
/// (Ordner im Explorer, Dateien mit ihrer Standard-App).
pub fn open(d: &Dest) {
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_IDLIST, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    unsafe {
        let mut pidl = d.pidl.clone();
        if pidl.is_empty() {
            // Nur Shell-Pfad bekannt: über ihn das Element bestimmen.
            use windows::Win32::UI::Shell::{ILGetSize, SHParseDisplayName};
            let mut p = std::ptr::null_mut();
            if SHParseDisplayName(&windows::core::HSTRING::from(d.parsing.as_str()), None, &mut p, 0, None).is_ok() && !p.is_null() {
                pidl = std::slice::from_raw_parts(p as *const u8, ILGetSize(Some(p)) as usize).to_vec();
                windows::Win32::System::Com::CoTaskMemFree(Some(p as _));
            }
        }
        if !pidl.is_empty() {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_IDLIST | SEE_MASK_INVOKEIDLIST;
            info.lpIDList = pidl.as_ptr() as *mut _;
            info.nShow = SW_SHOWNORMAL;
            if ShellExecuteExW(&mut info) != 0 {
                return;
            }
        }
        crate::util::shell_open(&d.parsing, "", SW_SHOWNORMAL);
    }
}

// ---------------------------------------------------------------- Kennungen

/// Standard-App-Kennung, die Windows für Programme ohne eigene AppUserModelID
/// bildet: der exe-Pfad, bei dem ein bekannter Ordner (Programme, System32 …)
/// durch seine GUID ersetzt ist, z. B. `{1AC14E77-…}\notepad.exe`.
pub fn default_app_id(exe: &str) -> Option<String> {
    use windows::Win32::UI::Shell::*;
    let folders = [
        FOLDERID_ProgramFilesX64,
        FOLDERID_ProgramFilesX86,
        FOLDERID_ProgramFilesCommonX64,
        FOLDERID_ProgramFilesCommonX86,
        FOLDERID_System,
        FOLDERID_SystemX86,
        FOLDERID_Windows,
        FOLDERID_UserProgramFiles,
        FOLDERID_LocalAppData,
        FOLDERID_RoamingAppData,
        FOLDERID_Profile,
    ];
    let lower = exe.to_lowercase();
    let mut best: Option<(usize, String)> = None;
    for id in folders {
        let Ok(p) = (unsafe { SHGetKnownFolderPath(&id, KNOWN_FOLDER_FLAG(0), None) }) else { continue };
        let path = unsafe { p.to_string() }.ok();
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _)) };
        let Some(path) = path else { continue };
        let prefix = path.trim_end_matches('\\').to_lowercase() + "\\";
        if lower.starts_with(&prefix) && best.as_ref().is_none_or(|(n, _)| prefix.len() > *n) {
            best = Some((prefix.len(), format!("{}\\{}", guid_string(&id), &exe[prefix.len()..])));
        }
    }
    best.map(|(_, s)| s)
}

/// GUID in Klammerform, Großbuchstaben: "{1AC14E77-02E7-…}".
fn guid_string(g: &windows::core::GUID) -> String {
    let d = g.data4;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    )
}

// ---------------------------------------------------------------- Tests

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetischer DestList-Eintrag im Aufbau der jeweiligen Version
    /// (1 = Windows 7, 3/4 = Windows 10, 6 = Windows 11 Build 26xxx).
    fn entry(version: u32, id: u32, score: f32, last: u64, pin: i32, path: &str) -> Vec<u8> {
        let mut e = vec![0u8; 88];
        e[72..76].copy_from_slice(b"host");
        e.extend(id.to_le_bytes());
        if version >= 5 {
            e.extend(0u32.to_le_bytes());
        }
        e.extend(score.to_le_bytes());
        e.extend(last.to_le_bytes());
        e.extend(pin.to_le_bytes());
        match version {
            0..=2 => {}
            3..=4 => {
                e.extend((-1i32).to_le_bytes());
                e.extend(7u32.to_le_bytes());
                e.extend(0u64.to_le_bytes());
            }
            _ => {
                e.extend(7u32.to_le_bytes());
                e.extend(1u32.to_le_bytes());
                e.extend(0u64.to_le_bytes());
            }
        }
        let units: Vec<u16> = path.encode_utf16().collect();
        e.extend((units.len() as u16).to_le_bytes());
        units.iter().for_each(|u| e.extend(u.to_le_bytes()));
        if version >= 3 {
            e.extend(0u32.to_le_bytes());
        }
        e
    }

    fn destlist(version: u32, entries: &[Vec<u8>], pinned: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(version.to_le_bytes());
        b.extend((entries.len() as u32).to_le_bytes());
        b.extend(pinned.to_le_bytes());
        b.extend(1.0f32.to_le_bytes());
        b.extend((entries.len() as u32).to_le_bytes());
        b.extend([0u8; 12]);
        entries.iter().for_each(|e| b.extend(e));
        b
    }

    #[test]
    fn pin_flag_and_order() {
        for version in [6, 4, 3, 1] {
            let b = destlist(
                version,
                &[
                    entry(version, 1, 0.5, 100, -1, "C:\\a"),
                    entry(version, 2, 2.0, 300, 1, "C:\\pin-second"),
                    entry(version, 3, 9.0, 200, -1, "C:\\b"),
                    entry(version, 0x1a, 1.0, 50, 0, "C:\\pin-first"),
                    entry(version, 5, 3.0, 400, -1, "C:\\c"),
                ],
                2,
            );
            let dl = parse_destlist(&b).unwrap();
            assert_eq!(dl.version, version);
            assert_eq!(dl.entries.len(), 5);
            let pinned: Vec<&str> = dl.pinned().iter().map(|e| e.path.as_str()).collect();
            assert_eq!(pinned, ["C:\\pin-first", "C:\\pin-second"]);
            assert_eq!(dl.pinned()[0].id, 0x1a);
            assert_eq!(stream_name(dl.pinned()[0].id), "1a");
            let recent: Vec<u32> = dl.recent().iter().map(|e| e.id).collect();
            assert_eq!(recent, [5, 3, 1]);
            let frequent: Vec<u32> = dl.frequent().iter().map(|e| e.id).collect();
            assert_eq!(frequent, [3, 5, 1]);
            if version >= 3 {
                assert_eq!(dl.entries[0].count, 7);
            }
        }
    }

    #[test]
    fn truncated_keeps_complete_entries() {
        let mut b = destlist(6, &[entry(6, 1, 1.0, 1, -1, "C:\\x"), entry(6, 2, 1.0, 2, 0, "C:\\y")], 1);
        b.truncate(b.len() - 6);
        let dl = parse_destlist(&b).unwrap();
        assert_eq!(dl.entries.len(), 1);
        assert!(dl.pinned().is_empty());
        assert!(parse_destlist(&[1, 0]).is_none());
    }

    /// Kleines Verbunddokument (512-Byte-Sektoren): ein großer Datenstrom
    /// "DestList" (≥ 4096, normale Sektoren) und ein kleiner "1" im Mini-Strom.
    #[test]
    fn cfb_streams() {
        let big: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let small: Vec<u8> = b"link-bytes".to_vec();
        // Sektoren: 0 = FAT, 1 = Verzeichnis, 2 = MiniFAT, 3 = Mini-Strom, 4.. = big.
        let big_sectors = big.len().div_ceil(512);
        let mut fat = vec![0xFFFF_FFFDu32, ENDOFCHAIN, ENDOFCHAIN, ENDOFCHAIN];
        for i in 0..big_sectors {
            fat.push(if i + 1 == big_sectors { ENDOFCHAIN } else { 5 + i as u32 });
        }
        fat.resize(128, 0xFFFF_FFFF);
        let mut file = vec![0u8; 512];
        file[..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
        file[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes());
        file[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
        file[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
        file[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
        file[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes());
        file[0x30..0x34].copy_from_slice(&1u32.to_le_bytes());
        file[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes());
        file[0x3C..0x40].copy_from_slice(&2u32.to_le_bytes());
        file[0x40..0x44].copy_from_slice(&1u32.to_le_bytes());
        file[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
        for i in 0..109 {
            let v = if i == 0 { 0u32 } else { 0xFFFF_FFFF };
            file[0x4C + i * 4..0x50 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        // FAT
        fat.iter().for_each(|v| file.extend(v.to_le_bytes()));
        // Verzeichnis
        let dirent = |name: &str, kind: u8, start: u32, size: u64| {
            let mut e = vec![0u8; 128];
            let units: Vec<u16> = name.encode_utf16().chain([0]).collect();
            for (i, u) in units.iter().enumerate() {
                e[i * 2..i * 2 + 2].copy_from_slice(&u.to_le_bytes());
            }
            e[64..66].copy_from_slice(&((units.len() * 2) as u16).to_le_bytes());
            e[66] = kind;
            e[116..120].copy_from_slice(&start.to_le_bytes());
            e[120..128].copy_from_slice(&size.to_le_bytes());
            e
        };
        file.extend(dirent("Root Entry", 5, 3, 64));
        file.extend(dirent("DestList", 2, 4, big.len() as u64));
        file.extend(dirent("1", 2, 0, small.len() as u64));
        file.extend(vec![0u8; 128]);
        // MiniFAT
        let mut mf = vec![ENDOFCHAIN];
        mf.resize(128, 0xFFFF_FFFF);
        mf.iter().for_each(|v| file.extend(v.to_le_bytes()));
        // Mini-Strom
        let mut mini = small.clone();
        mini.resize(512, 0);
        file.extend(mini);
        // big
        let mut bb = big.clone();
        bb.resize(big_sectors * 512, 0);
        file.extend(bb);

        let cfb = Cfb::parse(&file).unwrap();
        assert_eq!(cfb.stream("DestList").unwrap(), big);
        assert_eq!(cfb.stream("1").unwrap(), small);
        assert!(cfb.stream("2").is_none());
        assert!(Cfb::parse(&file[..100]).is_none());
    }

    #[test]
    fn braces() {
        let g = windows::core::GUID::from_u128(0x1ac14e77_02e7_4e5d_b744_2eb1ae5198b7);
        assert_eq!(guid_string(&g), "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}");
    }
}

/// Diagnose: Kopf und Rohfelder je Eintrag (ohne Pfade).
pub fn dump(app_id: &str) -> String {
    let Some(p) = auto_path(app_id) else { return String::new() };
    let Ok(bytes) = std::fs::read(&p) else { return "keine Datei\n".into() };
    let Some(cfb) = Cfb::parse(&bytes) else { return "kein CFB\n".into() };
    let names: Vec<String> = cfb.streams.iter().map(|(n, _, s)| format!("{n}:{s}")).collect();
    let Some(raw) = cfb.stream("DestList") else { return format!("kein DestList; Ströme {names:?}\n") };
    let head: Vec<u32> = (0..8).filter_map(|i| u32_at(&raw, i * 4)).collect();
    let mut out = format!("Ströme {}: {:?}\nKopf {head:?} Länge {}\n", names.len(), names.iter().take(40).collect::<Vec<_>>(), raw.len());
    if let Some(dl) = parse_destlist(&raw) {
        for e in &dl.entries {
            out += &format!("id {:x} score {} last {} pin {:?} count {} pathlen {} kind {}\n", e.id, e.score, e.last, e.pin, e.count, e.path.len(), e.path.chars().take(3).filter(|c| !c.is_alphanumeric()).collect::<String>());
        }
    }
    out
}
