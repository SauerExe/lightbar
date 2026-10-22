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
    factory.SetDisplayName(w!("Lightbar"))?;
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

/// Kontobild des angemeldeten Nutzers, kreisrund in `size` Pixeln (vormultipliziertes
/// Alpha). Windows legt es in mehreren Größen ab und trägt die Pfade unter
/// `HKLM\…\AccountPicture\Users\<SID>` ein. Null, wenn es keins gibt.
pub unsafe fn account_picture(size: i32) -> HBITMAP {
    const SIZES: [i32; 11] = [32, 40, 48, 64, 96, 192, 208, 240, 424, 448, 1080];
    let Some(sid) = user_sid() else { return null_mut() };
    let key = format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\AccountPicture\Users\{sid}");
    // Kleinste Ablage, die mindestens so groß ist; sonst die größte vorhandene.
    let mut order: Vec<i32> = SIZES.iter().copied().filter(|&s| s >= size).collect();
    order.extend(SIZES.iter().rev().copied().filter(|&s| s < size));
    let Some(path) = order.into_iter().find_map(|s| reg_string(&key, &format!("Image{s}"))) else { return null_mut() };
    let p = HSTRING::from(path.as_str());
    let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&p, None::<&IBindCtx>) else { return null_mut() };
    let Ok(factory) = item.cast::<IShellItemImageFactory>() else { return null_mut() };
    let Ok(bmp) = factory.GetImage(SIZE { cx: size, cy: size }, SIIGBF_THUMBNAILONLY) else { return null_mut() };
    let bmp = bmp.0 as HBITMAP;
    if !circle_mask(bmp, size) {
        DeleteObject(bmp);
        return null_mut();
    }
    bmp
}

/// SID des angemeldeten Nutzers als Zeichenkette (`S-1-5-21-…`).
unsafe fn user_sid() -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = null_mut();
    if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
        return None;
    }
    let mut buf = [0u64; 64];
    let mut len = 0u32;
    let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as _, size_of::<[u64; 64]>() as u32, &mut len);
    CloseHandle(token);
    if ok == 0 {
        return None;
    }
    let user = &*(buf.as_ptr() as *const TOKEN_USER);
    let mut s: windows_sys::core::PWSTR = null_mut();
    if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
        return None;
    }
    let len = (0..).take_while(|&i| *s.add(i) != 0).count();
    let sid = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
    LocalFree(s as _);
    Some(sid)
}

/// REG_SZ unter HKLM lesen.
unsafe fn reg_string(key: &str, name: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buf = [0u16; 1024];
    let mut bytes = size_of::<[u16; 1024]>() as u32;
    let (k, n) = (crate::util::w16(key), crate::util::w16(name));
    if RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr() as _, &mut bytes) != 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len])).filter(|s| !s.is_empty())
}

/// Bild deckend machen und auf einen Kreis zuschneiden, Rand geglättet.
/// `false`, wenn es keine 32-bit-Bitmap in `size` ist.
unsafe fn circle_mask(bmp: HBITMAP, size: i32) -> bool {
    let mut bm: BITMAP = zeroed();
    if GetObjectW(bmp, size_of::<BITMAP>() as i32, &mut bm as *mut _ as _) == 0
        || bm.bmBits.is_null()
        || bm.bmBitsPixel != 32
        || bm.bmWidth != size
        || bm.bmHeight.abs() != size
    {
        return false;
    }
    let stride = bm.bmWidthBytes as usize;
    let px = std::slice::from_raw_parts_mut(bm.bmBits as *mut u8, stride * size as usize);
    let (c, r) = (size as f32 / 2.0, size as f32 / 2.0);
    for y in 0..size as usize {
        for x in 0..size as usize {
            let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
            let cover = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            let a = (cover * 255.0).round() as u32;
            let p = &mut px[y * stride + x * 4..][..4];
            for ch in &mut p[..3] {
                *ch = (*ch as u32 * a / 255) as u8;
            }
            p[3] = a as u8;
        }
    }
    true
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

/// Elemente eines Shell-Ordners (z. B. Schnellzugriff), höchstens `max`.
/// Rückgabe: (Anzeigename, Shell-Pfad).
pub fn folder_items(parsing: &str, max: usize) -> Vec<(String, String)> {
    unsafe {
        let run = || -> windows::core::Result<Vec<(String, String)>> {
            let folder: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(parsing), None::<&IBindCtx>)?;
            let items: IEnumShellItems = folder.BindToHandler(None::<&IBindCtx>, &BHID_EnumItems)?;
            let mut out = Vec::new();
            while out.len() < max {
                let mut batch: [Option<IShellItem>; 1] = [None];
                let mut fetched = 0u32;
                let _ = items.Next(&mut batch, Some(&mut fetched));
                let Some(item) = (fetched > 0).then(|| batch[0].take()).flatten() else { break };
                if let (Some(n), Some(p)) = (display(&item, SIGDN_NORMALDISPLAY), display(&item, SIGDN_DESKTOPABSOLUTEPARSING)) {
                    out.push((n, p));
                }
            }
            Ok(out)
        };
        run().unwrap_or_default()
    }
}
