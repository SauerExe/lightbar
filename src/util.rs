//! Gemeinsame Helfer: Farben, DPI, GDI-Zeichnen, Eingaben.

use std::mem::{size_of, zeroed};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}
pub const BG: COLORREF = rgb(32, 32, 32);
pub const PANEL: COLORREF = rgb(36, 36, 36);
pub const FIELD: COLORREF = rgb(28, 28, 28);
pub const BORDER: COLORREF = rgb(50, 50, 50);
pub const HOVER: COLORREF = rgb(48, 48, 48);
pub const ACTIVE: COLORREF = rgb(56, 56, 56);
const FALLBACK_ACCENT: COLORREF = rgb(118, 185, 237);
static ACCENT_CACHE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);

/// Windows-Akzentfarbe in der Variante für dunkles Design (wie Windows sie
/// auf dunklem Grund zeigt). Gecacht; `reload_accent` nach Farbwechsel.
pub fn accent() -> COLORREF {
    let c = ACCENT_CACHE.load(std::sync::atomic::Ordering::Relaxed);
    if c != u32::MAX {
        return c;
    }
    let c = unsafe { read_accent() }.unwrap_or(FALLBACK_ACCENT);
    ACCENT_CACHE.store(c, std::sync::atomic::Ordering::Relaxed);
    c
}

pub fn reload_accent() {
    ACCENT_CACHE.store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
}

/// `AccentPalette` enthält 8 RGBA-Abstufungen der Akzentfarbe; Index 1 ist
/// die helle Variante, die Windows im dunklen Design verwendet.
unsafe fn read_accent() -> Option<COLORREF> {
    use windows_sys::Win32::System::Registry::*;
    let mut buf = [0u8; 32];
    let mut size = buf.len() as u32;
    let ok = RegGetValueW(
        HKEY_CURRENT_USER,
        w16(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent").as_ptr(),
        w16("AccentPalette").as_ptr(),
        RRF_RT_REG_BINARY,
        null_mut(),
        buf.as_mut_ptr() as _,
        &mut size,
    );
    (ok == 0 && size >= 8).then(|| rgb(buf[4], buf[5], buf[6]))
}
pub const DIM: COLORREF = rgb(140, 140, 140);
pub const TEXT: COLORREF = rgb(255, 255, 255);
pub const SUBTEXT: COLORREF = rgb(190, 190, 190);

/// Lineare Mischung zweier Farben (t = 0 → a, t = 1 → b).
pub fn mix(a: COLORREF, b: COLORREF, t: f32) -> COLORREF {
    let t = t.clamp(0.0, 1.0);
    let ch = |s: u32| {
        let (x, y) = (((a >> s) & 0xff) as f32, ((b >> s) & 0xff) as f32);
        ((x + (y - x) * t).round() as u32) << s
    };
    ch(0) | ch(8) | ch(16)
}

/// Datenordner `%APPDATA%\Lightbar` (Einstellungen, Anheftungen, Zustand).
/// Zieht beim ersten Aufruf einmalig vom früheren Namen `rtaskbar` um.
pub fn data_dir() -> Option<std::path::PathBuf> {
    let appdata = std::path::PathBuf::from(std::env::var("APPDATA").ok()?);
    let dir = appdata.join("Lightbar");
    let legacy = appdata.join("rtaskbar");
    if !dir.exists() && legacy.exists() {
        let _ = std::fs::rename(&legacy, &dir);
    }
    Some(dir)
}

/// Eine Zeile ins Protokoll `%APPDATA%\Lightbar\lightbar.log` (mit Zeit und
/// Prozess-ID). Für Ereignisse, die man im Nachhinein verstehen will: Start,
/// Ende, Neustarts, Korrekturen. Wächst die Datei über 256 KB, bleibt die
/// jüngere Hälfte. Keine persönlichen Daten hineinschreiben.
pub fn log(msg: &str) {
    use std::io::Write;
    let Some(dir) = data_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("lightbar.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024) {
        // Byteweise am nächsten Zeilenende schneiden (nie mitten in einem Umlaut).
        if let Ok(bytes) = std::fs::read(&path) {
            let half = bytes.len() / 2;
            let cut = bytes[half..].iter().position(|&b| b == b'\n').map_or(half, |n| half + n + 1);
            let _ = std::fs::write(&path, &bytes[cut..]);
        }
    }
    let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} [{}] {msg}\n",
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond,
        std::process::id()
    );
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn w16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn px(dpi: u32, v: i32) -> i32 {
    v * dpi as i32 / 96
}

/// Stammt die aktuelle Mausnachricht von Touch oder Stift? Windows markiert
/// emulierte Mausnachrichten mit einer Signatur in den Extra-Infos.
pub fn from_touch() -> bool {
    const MI_WP_SIGNATURE: usize = 0xFF51_5700;
    (unsafe { GetMessageExtraInfo() } as usize & 0xFFFF_FF00) == MI_WP_SIGNATURE
}

pub fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

pub fn xy(lp: LPARAM) -> (i32, i32) {
    ((lp & 0xffff) as i16 as i32, ((lp >> 16) & 0xffff) as i16 as i32)
}

pub unsafe fn send_keys(keys: &[(VIRTUAL_KEY, bool)]) {
    let inputs: Vec<INPUT> = keys
        .iter()
        .map(|&(vk, up)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    // Win-Tasten sind "erweiterte" Tasten; ohne das Kennzeichen erkennt
                    // Windows simulierte Kürzel wie Win+Tab nicht zuverlässig.
                    dwFlags: (if up { KEYEVENTF_KEYUP } else { 0 }) | if matches!(vk, VK_LWIN | VK_RWIN) { KEYEVENTF_EXTENDEDKEY } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        })
        .collect();
    SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32);
}

/// SetForegroundWindow trotz Foreground-Lock. Erst über eine kurz gekoppelte
/// Eingabe-Queue des aktuellen Vordergrund-Threads, als letzter Ausweg ein
/// eigener Alt-Tipper (der kann in klassischen Apps die Menüleiste antippen).
pub unsafe fn force_foreground(h: HWND) {
    if SetForegroundWindow(h) != 0 {
        return;
    }
    let fg_thread = GetWindowThreadProcessId(GetForegroundWindow(), null_mut());
    let me = GetCurrentThreadId();
    if fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, 1) != 0 {
        let ok = SetForegroundWindow(h);
        // Asynchron: Hängt die Ziel-App, darf die Leiste nicht mit hängen.
        SetWindowPos(h, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
        AttachThreadInput(me, fg_thread, 0);
        if ok != 0 {
            return;
        }
    }
    send_keys(&[(VK_MENU, false), (VK_MENU, true)]);
    SetForegroundWindow(h);
}

pub unsafe fn make_font(dpi: u32, size: i32, weight: u32) -> HFONT {
    font_face(dpi, size, weight, "Segoe UI")
}

/// Symbolschrift von Windows 11 (Suche, Zahnrad, Ein/Aus …).
pub unsafe fn icon_font(dpi: u32, size: i32) -> HFONT {
    font_face(dpi, size, FW_NORMAL, "Segoe Fluent Icons")
}

/// Feste Breite für Code: Cascadia Mono, sonst Consolas, sonst Courier New.
pub unsafe fn mono_font(dpi: u32, size: i32) -> HFONT {
    for face in ["Cascadia Mono", "Consolas"] {
        let f = font_face(dpi, size, FW_NORMAL, face);
        // GDI liefert für unbekannte Namen stillschweigend eine Ersatzschrift.
        let dc = CreateCompatibleDC(null_mut());
        let old = SelectObject(dc, f);
        let mut buf = [0u16; 64];
        let n = GetTextFaceW(dc, buf.len() as i32, buf.as_mut_ptr()).max(0) as usize;
        SelectObject(dc, old);
        DeleteDC(dc);
        let got = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
        if got.trim_end_matches('\0').eq_ignore_ascii_case(face) {
            return f;
        }
        DeleteObject(f);
    }
    font_face(dpi, size, FW_NORMAL, "Courier New")
}

unsafe fn font_face(dpi: u32, size: i32, weight: u32, face: &str) -> HFONT {
    CreateFontW(
        -px(dpi, size),
        0,
        0,
        0,
        weight as _,
        0,
        0,
        0,
        DEFAULT_CHARSET as _,
        OUT_DEFAULT_PRECIS as _,
        CLIP_DEFAULT_PRECIS as _,
        CLEARTYPE_QUALITY as _,
        (DEFAULT_PITCH | FF_DONTCARE) as _,
        w16(face).as_ptr(),
    )
}

pub unsafe fn fill(dc: HDC, r: &RECT, c: COLORREF) {
    let b = CreateSolidBrush(c);
    FillRect(dc, r, b);
    DeleteObject(b);
}

pub unsafe fn round(dc: HDC, r: &RECT, c: COLORREF, radius: i32) {
    let b = CreateSolidBrush(c);
    let ob = SelectObject(dc, b);
    let op = SelectObject(dc, GetStockObject(NULL_PEN));
    // Mit NULL_PEN lässt RoundRect rechts/unten ein Pixel weg.
    RoundRect(dc, r.left, r.top, r.right + 1, r.bottom + 1, radius, radius);
    SelectObject(dc, op);
    SelectObject(dc, ob);
    DeleteObject(b);
}

/// Abgerundeter Rahmen ohne Füllung.
pub unsafe fn outline(dc: HDC, r: &RECT, c: COLORREF, radius: i32) {
    let pen = CreatePen(PS_SOLID, 1, c);
    let op = SelectObject(dc, pen);
    let ob = SelectObject(dc, GetStockObject(NULL_BRUSH));
    RoundRect(dc, r.left, r.top, r.right, r.bottom, radius, radius);
    SelectObject(dc, ob);
    SelectObject(dc, op);
    DeleteObject(pen);
}

pub unsafe fn text(dc: HDC, s: &str, r: &RECT, font: HFONT, color: COLORREF, flags: u32) {
    let w = w16(s);
    let of = SelectObject(dc, font);
    SetBkMode(dc, TRANSPARENT as _);
    SetTextColor(dc, color);
    let mut rc = *r;
    DrawTextW(dc, w.as_ptr(), -1, &mut rc, flags | DT_NOPREFIX);
    SelectObject(dc, of);
}

pub unsafe fn text_width(dc: HDC, s: &str, font: HFONT) -> i32 {
    let w = w16(s);
    let of = SelectObject(dc, font);
    let mut size: SIZE = zeroed();
    GetTextExtentPoint32W(dc, w.as_ptr(), w.len() as i32 - 1, &mut size);
    SelectObject(dc, of);
    size.cx
}

/// 32-bit-Bitmap mit vormultipliziertem Alpha zeichnen.
pub unsafe fn draw_alpha(dc: HDC, bmp: HBITMAP, x: i32, y: i32, size: i32) {
    let src = CreateCompatibleDC(dc);
    let old = SelectObject(src, bmp);
    let bf = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    AlphaBlend(dc, x, y, size, size, src, 0, 0, size, size, bf);
    SelectObject(src, old);
    DeleteDC(src);
}

/// Doppelpuffer für WM_PAINT: zeichnet in `f` auf einen Speicher-DC und blittet.
pub unsafe fn paint_buffered(hwnd: HWND, f: impl FnOnce(HDC, i32, i32)) {
    let mut ps: PAINTSTRUCT = zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = zeroed();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right, rc.bottom);
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old = SelectObject(mem, bmp);
    // Nur der ungültige Bereich wird wirklich gerastert; Zeichenaufrufe
    // außerhalb kosten fast nichts (Teil-Neuzeichnen z. B. nur der Uhr).
    let r = ps.rcPaint;
    IntersectClipRect(mem, r.left, r.top, r.right, r.bottom);
    f(mem, w, h);
    BitBlt(hdc, r.left, r.top, r.right - r.left, r.bottom - r.top, mem, r.left, r.top, SRCCOPY);
    SelectObject(mem, old);
    DeleteObject(bmp);
    DeleteDC(mem);
    EndPaint(hwnd, &ps);
}

/// Datei, URI (ms-settings:, shell:, https:) oder Programm öffnen.
pub unsafe fn shell_open(target: &str, args: &str, show: i32) {
    shell_exec_show("open", target, args, show);
}

/// ShellExecute mit Verb ("open", "runas" …); `true` bei Erfolg.
pub unsafe fn shell_exec(verb: &str, target: &str, args: &str) -> bool {
    shell_exec_show(verb, target, args, SW_SHOWNORMAL)
}

unsafe fn shell_exec_show(verb: &str, target: &str, args: &str, show: i32) -> bool {
    let (t, a, v) = (w16(target), w16(args), w16(verb));
    let r = ShellExecuteW(
        null_mut(),
        v.as_ptr(),
        t.as_ptr(),
        if args.is_empty() { std::ptr::null() } else { a.as_ptr() },
        std::ptr::null(),
        show,
    );
    r as isize > 32
}

/// Prozent-Kodierung für Such-URLs.
pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub unsafe fn register_class(inst: HINSTANCE, name: &[u16], proc_: WNDPROC, style: u32) {
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style,
        lpfnWndProc: proc_,
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: inst,
        hIcon: null_mut(),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: name.as_ptr(),
        hIconSm: null_mut(),
    };
    RegisterClassExW(&wc);
}

/// Arbeitsbereich des Bildschirms an einem Punkt (für Popups über einer
/// Zweitleiste: nicht auf den Hauptbildschirm begrenzen).
pub unsafe fn work_area_at(x: i32, y: i32) -> RECT {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST), &mut mi);
    mi.rcMonitor
}

const CF_UNICODETEXT: u32 = 13;

/// Text aus der Zwischenablage (nur Unicode-Text), sonst None.
pub unsafe fn clipboard_text(owner: HWND) -> Option<String> {
    use windows_sys::Win32::System::DataExchange::*;
    use windows_sys::Win32::System::Memory::*;
    if OpenClipboard(owner) == 0 {
        return None;
    }
    let mut out = None;
    let h = GetClipboardData(CF_UNICODETEXT);
    if !h.is_null() {
        let p = GlobalLock(h) as *const u16;
        if !p.is_null() {
            // Höchstens bis zum Ende des Speicherblocks lesen.
            let max = GlobalSize(h) / 2;
            let len = (0..max).take_while(|&i| *p.add(i) != 0).count();
            out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)));
            GlobalUnlock(h);
        }
    }
    CloseClipboard();
    out
}

/// Text in die Zwischenablage legen.
pub unsafe fn set_clipboard_text(owner: HWND, s: &str) {
    use windows_sys::Win32::System::DataExchange::*;
    use windows_sys::Win32::System::Memory::*;
    let w = w16(s);
    let mem = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
    if mem.is_null() {
        return;
    }
    let p = GlobalLock(mem) as *mut u16;
    if p.is_null() {
        GlobalFree(mem);
        return;
    }
    std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
    GlobalUnlock(mem);
    if OpenClipboard(owner) == 0 {
        GlobalFree(mem);
        return;
    }
    EmptyClipboard();
    // Gehört danach der Zwischenablage; nur bei Fehler selbst freigeben.
    if SetClipboardData(CF_UNICODETEXT, mem as HANDLE).is_null() {
        GlobalFree(mem);
    }
    CloseClipboard();
}

/// Rechteck in Client-Koordinaten von `hwnd` → Bildschirmkoordinaten.
pub unsafe fn rect_to_screen(hwnd: HWND, r: RECT) -> RECT {
    let mut a = POINT { x: r.left, y: r.top };
    let mut b = POINT { x: r.right, y: r.bottom };
    ClientToScreen(hwnd, &mut a);
    ClientToScreen(hwnd, &mut b);
    RECT { left: a.x, top: a.y, right: b.x, bottom: b.y }
}
