//! App-Katalog über `shell:AppsFolder` – dieselbe Quelle wie "Alle Apps" im
//! Windows-Startmenü, inklusive Store-Apps. Icons kommen über
//! IShellItemImageFactory in exakt der gewünschten Größe.

use std::mem::{size_of, zeroed};
use std::ptr::null_mut;

use windows::core::{w, Interface, HSTRING};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Storage::EnhancedStorage::PKEY_Link_TargetParsingPath;
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Shell::*;
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, GetObjectW, BITMAP, HBITMAP};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO, SW_SHOWNORMAL};

use crate::util::shell_open;

pub struct App {
    pub name: String,
    pub lower: String,
    /// Parsing-Name im AppsFolder (AUMID bei Store-Apps, Pfad/Known-Folder bei Desktop-Apps).
    pub id: String,
    item: IShellItem,
}

pub fn enumerate() -> Vec<App> {
    unsafe { enumerate_inner().unwrap_or_default() }
}

unsafe fn enumerate_inner() -> windows::core::Result<Vec<App>> {
    let folder: IShellItem = SHCreateItemFromParsingName(w!("shell:AppsFolder"), None::<&IBindCtx>)?;
    let items: IEnumShellItems = folder.BindToHandler(None::<&IBindCtx>, &BHID_EnumItems)?;
    let mut apps = Vec::new();
    loop {
        let mut batch: [Option<IShellItem>; 1] = [None];
        let mut fetched = 0u32;
        let _ = items.Next(&mut batch, Some(&mut fetched));
        if fetched == 0 {
            break;
        }
        let Some(item) = batch[0].take() else { break };
        let (Some(name), Some(id)) =
            (display(&item, SIGDN_NORMALDISPLAY), display(&item, SIGDN_PARENTRELATIVEPARSING))
        else {
            continue;
        };
        apps.push(App { lower: name.to_lowercase(), name, id, item });
    }
    apps.sort_by(|a, b| a.lower.cmp(&b.lower));
    apps.dedup_by(|a, b| a.id == b.id);
    Ok(apps)
}

unsafe fn display(item: &IShellItem, kind: SIGDN) -> Option<String> {
    let p = item.GetDisplayName(kind).ok()?;
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as _));
    s.filter(|s| !s.is_empty())
}

pub fn launch(id: &str) {
    unsafe { shell_open("explorer.exe", &format!("shell:AppsFolder\\{id}"), SW_SHOWNORMAL) }
}

/// exe hinter einer Desktop-App (für Anheften, Admin-Start, Speicherort).
/// Store-Apps haben keine.
pub unsafe fn target_path(app: &App) -> Option<String> {
    let is_exe = |s: &String| s.to_lowercase().ends_with(".exe") && s.contains(":\\");
    if is_exe(&app.id) {
        return Some(app.id.clone());
    }
    let item: IShellItem2 = app.item.cast().ok()?;
    let p = item.GetString(&PKEY_Link_TargetParsingPath).ok()?;
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as _));
    s.filter(is_exe)
}

/// Dateien und Ordner aus dem Windows-Suchindex – derselbe Weg wie die
/// Explorer-Suche: Abfrage parsen (mit automatischem Platzhalter, "rus" findet
/// "rust"), als Bedingung an einen Suchordner über dem Benutzerordner hängen
/// und aufzählen. Das Aufzählen wartet auf die Index-Abfrage, blockiert also –
/// nur aus einem Hintergrund-Thread mit eigenem COM-Apartment aufrufen.
/// Rückgabe: (Anzeigename, Pfad).
pub fn search_index(query: &str, max: usize) -> Vec<(String, String)> {
    search_index_checked(query, max).unwrap_or_default()
}

/// Wie search_index, aber mit Fehlertext (für --search-test).
pub fn search_index_checked(query: &str, max: usize) -> Result<Vec<(String, String)>, String> {
    unsafe { search_index_inner(query, max).map_err(|e| format!("{e}")) }
}

unsafe fn search_index_inner(query: &str, max: usize) -> windows::core::Result<Vec<(String, String)>> {
    let step = |n: &'static str| move |e: windows::core::Error| windows::core::Error::new(e.code(), format!("{n}: {}", e.message()));
    use windows::Win32::System::Com::{CoCreateInstance, IEnumUnknown, CLSCTX_INPROC_SERVER};
    use windows::Win32::System::Search::*;

    let manager: IQueryParserManager = CoCreateInstance(&QueryParserManager, None, CLSCTX_INPROC_SERVER).map_err(step("QueryParserManager"))?;
    // 0x0400 = Sprache des Benutzers für Schlüsselwörter wie "art:".
    let parser: IQueryParser = manager.CreateLoadedParser(w!("SystemIndex"), 0x0400).map_err(step("CreateLoadedParser"))?;
    manager.InitializeOptions(false, true, &parser).map_err(step("InitializeOptions"))?;
    // Nur im Dateinamen suchen (nicht im Inhalt), mit automatischem Platzhalter.
    let cleaned = query.replace(['"', '(', ')'], " ");
    let aqs = format!("System.FileName:({cleaned})");
    let solution = parser.Parse(&HSTRING::from(aqs), None::<&IEnumUnknown>).map_err(step("Parse"))?;
    let mut condition: Option<ICondition> = None;
    solution.GetQuery(Some(&mut condition), None).map_err(step("GetQuery"))?;
    let Some(condition) = condition else { return Ok(Vec::new()) };
    // Bezugszeit ist Pflicht (für relative Angaben wie "gestern").
    let now = windows::Win32::System::SystemInformation::GetLocalTime();
    let resolved = solution.Resolve(&condition, SQRO_DONT_SPLIT_WORDS, Some(&now)).map_err(step("Resolve"))?;

    let factory: ISearchFolderItemFactory = CoCreateInstance(&SearchFolderItemFactory, None, CLSCTX_INPROC_SERVER).map_err(step("SearchFolderItemFactory"))?;
    factory.SetDisplayName(w!("rtaskbar"))?;
    let home = HSTRING::from(std::env::var("USERPROFILE").unwrap_or_default());
    let scope_item: IShellItem = SHCreateItemFromParsingName(&home, None::<&IBindCtx>)?;
    let scope: IShellItemArray = SHCreateShellItemArrayFromShellItem(&scope_item)?;
    factory.SetScope(&scope).map_err(step("SetScope"))?;
    factory.SetCondition(&resolved).map_err(step("SetCondition"))?;
    let folder: IShellItem = factory.GetShellItem().map_err(step("GetShellItem"))?;
    let items: IEnumShellItems = folder.BindToHandler(None::<&IBindCtx>, &BHID_EnumItems).map_err(step("BindToHandler"))?;

    let mut out = Vec::new();
    let mut scanned = 0usize;
    while out.len() < max {
        let mut batch: [Option<IShellItem>; 1] = [None];
        let mut fetched = 0u32;
        let _ = items.Next(&mut batch, Some(&mut fetched));
        let Some(item) = (fetched > 0).then(|| batch[0].take()).flatten() else { break };
        if let (Some(name), Some(path)) = (display(&item, SIGDN_NORMALDISPLAY), display(&item, SIGDN_FILESYSPATH)) {
            if !is_noise(&path) {
                out.push((name, path));
            }
        }
        scanned += 1;
        if scanned > max * 20 {
            break; // Nicht ewig durch Rauschen blättern.
        }
    }
    Ok(out)
}
/// Ordner, deren Inhalt fast nie gemeint ist (Abhängigkeiten, Caches,
/// Versionsverwaltung, App-Daten).
fn is_noise(path: &str) -> bool {
    const NOISE: [&str; 9] = ["\\node_modules", "\\.git\\", "\\appdata\\", "\\go\\pkg\\", "\\.cargo\\", "\\target\\", "\\.venv\\", "\\__pycache__", "\\.nuget\\"];
    let p = path.to_lowercase();
    NOISE.iter().any(|n| p.contains(n))
}

/// AppUserModelID eines Fensters – danach gruppiert Windows die Taskleiste.
/// Viele klassische Programme setzen keine; dann gilt deren exe.
pub fn window_app_id(hwnd: *mut core::ffi::c_void) -> Option<String> {
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(windows::Win32::Foundation::HWND(hwnd)).ok()?;
        let value = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let p = PropVariantToStringAlloc(&value).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s.filter(|s| !s.is_empty())
    }
}

/// Icon eines Shell-Elements (z. B. `shell:AppsFolder\<AUMID>`) als HICON in
/// exakt `size` Pixeln. Null, wenn die Shell keins liefert.
pub unsafe fn shell_hicon(parsing: &str, size: i32) -> HICON {
    let p = HSTRING::from(parsing);
    let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&p, None::<&IBindCtx>) else { return null_mut() };
    let Ok(factory) = item.cast::<IShellItemImageFactory>() else { return null_mut() };
    let Ok(bmp) = factory.GetImage(SIZE { cx: size, cy: size }, SIIGBF_ICONONLY) else { return null_mut() };
    let color = bmp.0 as HBITMAP;
    let mask = CreateBitmap(size, size, 1, 1, std::ptr::null());
    let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&info);
    DeleteObject(mask);
    DeleteObject(color);
    icon
}

pub unsafe fn app_icon(app: &App, size: i32) -> HBITMAP {
    image(&app.item, size)
}

pub unsafe fn path_icon(path: &str, size: i32) -> HBITMAP {
    let p = HSTRING::from(path);
    match SHCreateItemFromParsingName::<_, _, IShellItem>(&p, None::<&IBindCtx>) {
        Ok(item) => image(&item, size),
        Err(_) => null_mut(),
    }
}

unsafe fn image(item: &IShellItem, size: i32) -> HBITMAP {
    let Ok(factory) = item.cast::<IShellItemImageFactory>() else { return null_mut() };
    match factory.GetImage(SIZE { cx: size, cy: size }, SIIGBF_ICONONLY) {
        Ok(bmp) => {
            let bmp = bmp.0 as HBITMAP;
            premultiply(bmp);
            bmp
        }
        Err(_) => null_mut(),
    }
}

/// AlphaBlend erwartet vormultipliziertes Alpha; die Shell liefert je nach
/// Quelle beides. Erkennen statt raten: Farbe > Alpha geht nur unmultipliziert.
unsafe fn premultiply(bmp: HBITMAP) {
    let mut bm: BITMAP = zeroed();
    if GetObjectW(bmp, size_of::<BITMAP>() as i32, &mut bm as *mut _ as _) == 0
        || bm.bmBits.is_null()
        || bm.bmBitsPixel != 32
    {
        return;
    }
    let len = (bm.bmWidthBytes * bm.bmHeight) as usize;
    let px = std::slice::from_raw_parts_mut(bm.bmBits as *mut u8, len);
    if px.chunks(4).all(|p| p[3] == 0) {
        // Kein Alphakanal: komplett deckend.
        px.chunks_mut(4).for_each(|p| p[3] = 255);
        return;
    }
    if px.chunks(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]) {
        return;
    }
    for p in px.chunks_mut(4) {
        let a = p[3] as u32;
        for c in &mut p[..3] {
            *c = (*c as u32 * a / 255) as u8;
        }
    }
}
