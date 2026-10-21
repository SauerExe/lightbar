//! Lightbar – schlanker Windows-11-Taskleisten- und Startmenü-Nachbau.
//!
//! Reines Win32 + GDI, kein UI-Framework. COM nur dort, wo die Shell es
//! verlangt (App-Katalog, Icons).

#![windows_subsystem = "windows"]

mod anim;
mod apps;
mod bar;
mod calendar;
mod display;
mod glow;
mod keyhook;
mod menus;
mod preview;
mod quick;
mod settings;
mod shell;
mod start;
mod status;
mod tooltip;
mod tray;
mod uia;
mod usage;
mod util;
mod watchdog;

use std::mem::zeroed;
use std::ptr::{null, null_mut};

use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Kontextmenüs im dunklen Win11-Stil. Die Funktionen sind in uxtheme.dll nur
/// per Ordinal exportiert (SetPreferredAppMode = 135, FlushMenuThemes = 136);
/// Explorer, Notepad++ & Co. nutzen denselben Weg. Fehlen sie, bleibt es hell.
unsafe fn dark_menus() {
    let lib = LoadLibraryW(util::w16("uxtheme.dll").as_ptr());
    if lib.is_null() {
        return;
    }
    if let Some(f) = GetProcAddress(lib, 135usize as *const u8) {
        let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
        set_mode(2); // ForceDark
    }
    if let Some(f) = GetProcAddress(lib, 136usize as *const u8) {
        let flush: unsafe extern "system" fn() = std::mem::transmute(f);
        flush();
    }
}

fn main() {
    unsafe {
        // Aus einem Paket-Container (z. B. Terminal einer paketierten App) heraus
        // gestartet: außerhalb neu starten, sonst landen Daten im Container.
        if shell::escape_package() {
            return;
        }
        // Diagnose: Index-Suche ohne Oberfläche, Ergebnis nach %APPDATA%\Lightbar\search-test.txt.
        if std::env::args().nth(1).as_deref() == Some("--search-test") {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let q = std::env::args().nth(2).unwrap_or_default();
            let t0 = std::time::Instant::now();
            let text = match apps::search_index_checked(&q, 12) {
                Ok(hits) => format!("{} Treffer in {} ms\n{}", hits.len(), t0.elapsed().as_millis(), hits.iter().map(|(n, p)| format!("{n} | {p}")).collect::<Vec<_>>().join("\n")),
                Err(e) => format!("Fehler nach {} ms: {e}", t0.elapsed().as_millis()),
            };
            if let Ok(a) = std::env::var("APPDATA") {
                let _ = std::fs::write(std::path::Path::new(&a).join("Lightbar").join("search-test.txt"), text);
            }
            return;
        }
        // Diagnose: Anzahl der Sprunglisten-Einträge einer App-Kennung (ohne Namen).
        if std::env::args().nth(1).as_deref() == Some("--jumplist-test") {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let id = std::env::args().nth(2).unwrap_or_default();
            let n = apps::jump_list(&id, 8).len();
            if let Some(dir) = util::data_dir() {
                let _ = std::fs::write(dir.join("jumplist-test.txt"), format!("{id}: {n} Einträge\n"));
            }
            return;
        }
        if std::env::args().nth(1).as_deref() == Some("--usage-test") {
            usage::self_test();
            return;
        }
        // Wächter für eine laufende Instanz (siehe `watchdog`).
        if std::env::args().nth(1).as_deref() == Some("--watchdog") {
            if let Some(pid) = std::env::args().nth(2).and_then(|p| p.parse().ok()) {
                watchdog::run(pid);
            }
            return;
        }
        // --install / --uninstall / --restore
        if shell::handle_args() {
            return;
        }
        // Nur eine Instanz: zwei Leisten würden sich gegenseitig Platz reservieren.
        CreateMutexW(null(), 0, util::w16("lightbar-single-instance").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        // Wächter gleich zu Beginn, damit auch ein Absturz beim Hochfahren
        // auffällt. Vom Wächter neu gestartet: Der bewacht uns schon. Den Merker
        // entfernen, damit ihn von hier gestartete Programme nicht erben.
        if std::env::var_os(watchdog::WATCHED).is_none() {
            watchdog::spawn();
        }
        std::env::remove_var(watchdog::WATCHED);
        let t0 = std::time::Instant::now();
        shell::wait_for_explorer(std::time::Duration::from_secs(30));
        util::log(&format!("Start {} (auf Explorer gewartet: {} ms)", env!("CARGO_PKG_VERSION"), t0.elapsed().as_millis()));
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        dark_menus();
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let inst = GetModuleHandleW(null());
        if bar::create(inst).is_null() {
            util::log("Leiste konnte nicht angelegt werden – Ende, Windows-Taskleiste wiederhergestellt");
            // Evtl. hat eine abgestürzte Vorgängerin Explorers Leiste versteckt.
            shell::restore();
            return;
        }
        start::create(inst);
        quick::create(inst);
        tooltip::create(inst);
        preview::create(inst);
        glow::create(inst);
        calendar::create(inst);
        tray::create_overflow(inst);
        keyhook::spawn();
        shell::takeover();
        bar::schedule_rehide();

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // Tray-Icons zurück an Explorer.
        tray::announce();
        // Sofort beenden: Das Freigeben der Shell-Objekte (App-Katalog, COM)
        // beim regulären Prozessende kann blockieren – dann hinge ein
        // fensterloser Prozess herum und hielte die exe gesperrt. Zu sichern
        // gibt es nichts mehr (Einstellungen werden bei jeder Änderung geschrieben).
        windows_sys::Win32::System::Threading::TerminateProcess(windows_sys::Win32::System::Threading::GetCurrentProcess(), 0);
    }
}
