//! Lightbar – schlanker Windows-11-Taskleisten- und Startmenü-Nachbau.
//!
//! Reines Win32 + GDI, kein UI-Framework. COM nur dort, wo die Shell es
//! verlangt (App-Katalog, Icons).

#![windows_subsystem = "windows"]

#[macro_use]
mod i18n;
mod ai;
mod ai_tools;
mod anim;
mod apps;
mod bar;
mod calendar;
mod clockfmt;
mod chat;
mod destlist;
mod display;
mod glow;
mod jumplist;
mod kbd;
mod keyhook;
mod menus;
mod preview;
mod quick;
mod settings;
mod shell;
mod start;
mod taskband;
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

/// RegisterApplicationRestart (kernel32). Ohne Befehlszeile: normaler Start.
unsafe fn register_restart() {
    #[link(name = "kernel32")]
    extern "system" {
        fn RegisterApplicationRestart(command_line: *const u16, flags: u32) -> i32;
    }
    const RESTART_NO_CRASH: u32 = 1;
    const RESTART_NO_HANG: u32 = 2;
    let hr = RegisterApplicationRestart(null(), RESTART_NO_CRASH | RESTART_NO_HANG);
    if hr != 0 {
        util::log(&format!("RegisterApplicationRestart fehlgeschlagen (0x{hr:X})"));
    }
}

fn main() {
    unsafe {
        // Aus einem Paket-Container (z. B. Terminal einer paketierten App) heraus
        // gestartet: außerhalb neu starten, sonst landen Daten im Container.
        if shell::escape_package() {
            return;
        }
        // Merker des Ausbruchs nicht an von hier gestartete Programme vererben.
        std::env::remove_var(shell::MARKER);
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
        // Diagnose: Sprunglisten-Abschnitte (Namen, Anzahlen, Zeiten – keine
        // Dateinamen) einer App-Kennung oder exe; ohne Argument alle Apps des
        // Katalogs mit Sprungliste plus Abgleich mit Windows-Schnittstelle und
        // Schnellzugriff. Ergebnis nach %APPDATA%\Lightbar\jumplist-test.txt.
        if std::env::args().nth(1).as_deref() == Some("--jumplist-test") {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let args: Vec<String> = std::env::args().skip(2).collect();
            let text = if args.is_empty() { jumplist::diagnose(None) } else { args.iter().map(|a| jumplist::diagnose(Some(a))).collect() };
            if let Some(dir) = util::data_dir() {
                let _ = std::fs::write(dir.join("jumplist-test.txt"), text);
            }
            return;
        }
        // KI einrichten: --ai-setup <url> <key> [model] (Schlüssel landet nur in ai.txt).
        if std::env::args().nth(1).as_deref() == Some("--ai-setup") {
            let a: Vec<String> = std::env::args().skip(2).collect();
            if a.len() >= 2 {
                ai::setup(&a[0], &a[1], a.get(2).map(|s| s.as_str()));
            }
            return;
        }
        // Diagnose: eine Frage stellen, Antwort nach %APPDATA%\Lightbar\ai-test.txt
        // (oder `--ai-test <frage> <name>.txt`).
        // Werkzeuge, die etwas verändern (Apps öffnen, Lautstärke …), laufen
        // dabei nur als Testlauf; lesende (Systeminfo, Dateisuche) wirklich.
        if std::env::args().nth(1).as_deref() == Some("--ai-test") {
            ai_tools::set_dry_run(true);
            ai::set_trace(true);
            let q = std::env::args().nth(2).unwrap_or_else(|| "Sag kurz Hallo.".into());
            let (tx, rx) = std::sync::mpsc::channel();
            ai::ask(q, move || {
                let _ = tx.send(());
            });
            let t0 = std::time::Instant::now();
            while t0.elapsed() < std::time::Duration::from_secs(600) {
                let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
                if ai::current().is_some_and(|a| a.done) {
                    break;
                }
            }
            let text = match ai::current() {
                Some(a) => format!(
                    "Frage: {}\nnach {} ms, fertig={}, fehler={:?}\n{}{}\n{}",
                    a.question,
                    t0.elapsed().as_millis(),
                    a.done,
                    a.error,
                    a.steps.iter().map(|s| format!("Schritt: {s}\n")).collect::<String>(),
                    a.trace.iter().map(|s| format!("Werkzeug: {s}\n")).collect::<String>(),
                    a.text
                ),
                None => "nicht eingerichtet".into(),
            };
            if let Some(dir) = util::data_dir() {
                // Optional eigener Dateiname (nur Name, kein Pfad), z. B. für parallele Tests.
                let name = std::env::args().nth(3).filter(|n| n.ends_with(".txt") && !n.contains(['/', '\\', ':'])).unwrap_or_else(|| "ai-test.txt".into());
                let _ = std::fs::write(dir.join(name), text);
            }
            return;
        }
        if std::env::args().nth(1).as_deref() == Some("--taskbarlist-test") {
            taskband::self_test();
            return;
        }
        // Diagnose: Welches Shell_TrayWnd findet eine App (FindWindow)?
        if std::env::args().nth(1).as_deref() == Some("--findwindow-test") {
            let h = FindWindowW(util::w16("Shell_TrayWnd").as_ptr(), null());
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, &mut pid);
            util::log(&format!("FindWindow(Shell_TrayWnd) = {h:?}, Prozess {pid} (Lightbar: {})", pid != 0 && std::process::id() != pid && h == FindWindowW(util::w16("Shell_TrayWnd").as_ptr(), null())));
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
        // Eigener Exit-Code: Kam der Start vom Wächter, wartet der darauf, dass
        // die alte Instanz den Mutex freigibt, und versucht es dann erneut.
        if GetLastError() == ERROR_ALREADY_EXISTS {
            std::process::exit(watchdog::EXIT_MUTEX_BUSY);
        }
        // Nach einem Update oder Neustart durch den Restart Manager (Installer,
        // Windows Update) Lightbar wieder starten. Abstürze und Hänger
        // übernimmt der Wächter, deshalb dafür kein Neustart durch Windows.
        register_restart();
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
        taskband::create(inst, bar::hwnd());
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
