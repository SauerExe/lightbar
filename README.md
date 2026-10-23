# Lightbar

A lightweight replacement for the Windows 11 taskbar and Start menu, written in Rust.
Plain Win32 + GDI, no UI framework, a single ~650 KB executable using about 7 MB of RAM.

![Lightbar demo](docs/demo.gif)

<p>
  <img src="docs/screenshots/start-search.png" width="420" alt="Start menu search">
  <img src="docs/screenshots/quick-settings.png" width="300" alt="Quick settings">
  <img src="docs/screenshots/calendar.png" width="280" alt="Calendar">
</p>

> **Status: experimental hobby project.** It runs as a daily driver on one machine, but it is young and not widely tested. The user interface is currently in German.

## Why Lightbar?

Honest answer, based on what was measured and built – including where it is *not* better.

**1. Search – clearly better**
- Windows 11 search is a web page in an embedded Edge browser (WebView2): **~134 MB**.
- Lightbar's search is drawn natively and opens instantly. Taskbar, Start menu and search together use **~7.6 MB**.
- No Bing web results, no ads. Web search goes to DuckDuckGo – and only when you click it.

**2. A Start menu without Microsoft's extras**
- No promoted apps or "recommended" ads – "Recommended" shows only your own recently opened files.
- **Presentation mode** hides your name, files and history with one click – handy for streaming. Windows has nothing like it.

**3. Features Windows doesn't have**
- Claude / Codex usage right in the taskbar
- Task Manager button with a live CPU meter
- Mouse wheel over the speaker icon changes the volume; over an app it cycles through its windows
- ISO week numbers in the calendar
- Tray icons in the accent colour for a calm look
- Optional animated glow edge

**4. You're in control**
- Microsoft changes the taskbar with updates and has removed a lot in Windows 11. Lightbar only changes when you want it to, and anything missing can be added.
- Open source, with honestly measured numbers.

**Where Lightbar is *not* better**
- **Idle CPU:** on par with Windows (≈1.0 vs ≈1.4 ms/s), no real lead.
- **Total memory:** Windows' shell processes keep running in the background, so Lightbar's ~7 MB come on top.
- **Maturity:** Windows is tested on millions of PCs; Lightbar is brand new. Multi-monitor and touch are not yet tested on real hardware.
- **Missing:** its own notification centre, widgets, app-defined tasks in jump lists.

**In one sentence:** Lightbar is not leaner than the Windows taskbar – but its search is lighter and faster, and it gives you features and freedom that Windows doesn't.

## Features

- **Taskbar** – apps grouped by AppUserModelID like Windows 11, pinning, drag & drop reordering, live window previews (DWM thumbnails), Win+1…9, middle-click for a new instance, mouse wheel to cycle a group's windows, jump lists (recent and frequent files) in the right-click menu
- **Start menu** – all apps including Store apps, pinned apps, recently used files, search across apps, settings and files (Windows Search index), web search fallback, power and account menus
- **Keyboard** – Win opens the Start menu; Win+S search, Win+X power-user menu, Win+A quick settings, Win+T moves keyboard focus into the taskbar (arrow keys, Enter, context-menu key, Esc)
- **Screen readers** – UI Automation provider: the taskbar is exposed as a toolbar whose buttons have names, descriptions and can be invoked (Narrator, NVDA)
- **System tray** – takes over the notification-area icons, overflow flyout that respects Windows' "show icon" settings, optional icons in the accent colour
- **Quick settings** – Wi-Fi and Bluetooth toggles, project, volume slider, battery
- **Calendar flyout** with ISO week numbers
- **Task Manager button** with a live CPU meter
- **Glow edge** above the taskbar (Aurora, accent colour or neon) and subtle animations – optional
- **Claude / Codex usage** (5-hour and weekly limits) – optional, off by default, see notes
- **Presentation mode** for streaming or screen sharing – hides your name, recent files, file search results and usage
- **Follows your Windows settings** – taskbar alignment (left or centred, also where the Start menu opens) and "Show my taskbar on all displays"
- **Multiple monitors** – taskbars on other displays show Start, apps and the clock, like Windows; choose per display which ones get a taskbar and whether new displays get one (right-click → Bildschirme); flyouts open on the display you clicked; full-screen apps hide only the taskbar on their display
- **Small and portrait screens** – when space runs out, tray icons move into the overflow, the usage display hides, buttons get narrower, and remaining apps go behind a "…" button instead of overlapping
- **Size and clock style** – right-click → Größe: small, normal, large or extra large (Automatic follows Windows' "small taskbar buttons"); right-click → Uhr or right-click the clock: time and date, time only, with weekday, or without year, seconds, font (Segoe UI Variable, Segoe UI, Bahnschrift, Cascadia Mono, Consolas – only installed ones are offered) and bold time
- Hides the Windows taskbar while running and restores it on exit; maximised windows and full-screen apps are handled
- **Watchdog** – a small companion process restarts Lightbar if it crashes or stops responding, and brings back the Windows taskbar if that keeps failing, so you are never left without a taskbar
- Start button: Lightbar orb, Windows logo or Tux (right-click → Startsymbol)
- Your Windows account picture in the Start menu, with the initial as fallback (right-click → Startmenü → Profilbild anzeigen; hidden in presentation mode)

## Performance

Lightbar is event-driven: when nothing happens it does almost nothing (no polling of volume, network, battery or the window list). Measured idle on the author's machine, 90 s per scenario, all other apps closed, clock with seconds enabled. CPU is reported in milliseconds of CPU time per second; Windows counts CPU time in 15.6 ms steps, so treat small differences as noise.

| | CPU (Explorer + Start/Search hosts + Lightbar) | Lightbar itself |
|---|---|---|
| Windows 11 taskbar | 1.4 ms/s | – |
| Lightbar, glow off | 1.7 ms/s | 1.0 ms/s · 7 MB |
| Lightbar, neon glow | 22.6 ms/s | 20.5 ms/s · 8 MB |

Without the glow, Lightbar is on par with the Windows taskbar. The animated glow costs about 20 ms of CPU per second. Windows' own shell processes (Explorer, Start menu host, search host) keep running in the background, so total memory goes up by Lightbar's ~7 MB rather than down.

### What Windows 11 is built with

A common assumption is that the Windows 11 taskbar is written in JavaScript. Checked on Windows 11 24H2 (build 26100):

| Windows 11 component | Technology | Private memory |
|---|---|---|
| Taskbar (`explorer.exe`, `Taskbar.View.dll`) | native C++ with XAML | part of Explorer |
| Start menu (`StartMenuExperienceHost.exe`) | native C++ with XAML | ~150 MB |
| **Search** (`SearchHost.exe`) | **web UI in an embedded Edge browser (WebView2, HTML/JavaScript)** | **~134 MB** (94 MB host + 41 MB WebView2) |
| **Lightbar** (taskbar + Start menu + search) | native Rust, Win32 + GDI | **~7.6 MB** |

So the taskbar itself was never JavaScript – which is why Lightbar is only on par there. Search is where it differs most: Lightbar replaces a web page in an embedded browser with a natively drawn list that opens instantly. Windows keeps `SearchHost.exe` running in the background regardless, so that memory is not freed – it just goes unused.

## Install

Download `lightbar.exe` from the [releases](../../releases) or build it yourself:

```
cargo build --release
target\release\lightbar.exe --install
```

`--install` copies the executable to `%LOCALAPPDATA%\Programs\Lightbar`, adds it to autostart and starts it (a running instance is closed first). Autostart is a scheduled task "Lightbar" that runs when you sign in, with the Run registry key as a fallback.

| Command | Effect |
|---|---|
| `lightbar.exe --uninstall` | stop, remove autostart (task and Run key), restore the Windows taskbar |
| `lightbar.exe --restore` | only restore the Windows taskbar (e.g. after a crash) |
| `lightbar.exe --usage-test` | query usage once, write the result to `%APPDATA%\Lightbar\usage-test.txt` |
| `lightbar.exe --search-test <term>` | run an index search, write the result to `%APPDATA%\Lightbar\search-test.txt` |

Settings (right-click the taskbar) are stored in `%APPDATA%\Lightbar\settings.txt`. A custom clock format can only be set there, e.g. `clock_format=HH:mm|ddd dd.MM.`: the picture syntax of Windows' `GetTimeFormatEx`/`GetDateFormatEx` (`H`/`HH` hours, `h` 12-hour, `mm` minutes, `ss` seconds, `tt` AM/PM, `d`/`dd` day, `ddd`/`dddd` weekday, `M`/`MM`/`MMM` month, `yy`/`yyyy` year, text in single quotes). The part after `|` is the second line (empty = one line; without `|` the date follows the menu choice); an empty value uses the regional format. The clock menu shows "Eigenes Format (settings.txt)" checked while one is set and opens the file. Edits are picked up the next time a taskbar menu opens, or within 30 seconds. Starts, exits, watchdog restarts and work-area corrections are logged to `%APPDATA%\Lightbar\lightbar.log` (no personal data). To quit Lightbar for good, use right-click → "Lightbar beenden" or `--uninstall`; killing the process makes the watchdog restart it.

## Notes

- **Antivirus / SmartScreen:** the executable is not code-signed and installs a low-level keyboard hook to take over the Windows key. Some antivirus products flag that pattern. The source is all here.
- **Usage display:** reads the local logins of Claude Code (`~/.claude/.credentials.json`) and Codex (`~/.codex/auth.json`) and calls their *undocumented* usage endpoints. Tokens are never stored, logged or sent anywhere else. These endpoints may change at any time, and using them may not be covered by the providers' terms – which is why the feature is off by default.
- **Proxy:** the usage requests use `HTTPS_PROXY` / `https_proxy` if set (respecting `NO_PROXY`; credentials in the URL like `http://user:pass@proxy:3128` are supported), otherwise the WinHTTP proxy (`netsh winhttp set proxy`), otherwise Windows' proxy settings (including PAC/WPAD).
- Explorer keeps running (desktop, file windows); only its taskbar is hidden.
- **Started from a packaged app** (e.g. the terminal of an MSIX app such as the Claude desktop app): Windows would redirect Lightbar's settings, log and registry writes into that app's private container. Lightbar detects this and relaunches itself outside the container. Writing the Run key from there still lands in the container, so in that case only the scheduled task provides autostart.

## Known limitations

- Multi-monitor support is new and only lightly tested on real multi-display hardware. If Explorer does not reserve a taskbar's space (reported on multi-monitor setups), Lightbar sets that display's work area itself
- Jump lists show recent/frequent items only (no app-defined tasks); notification centre and widgets are Windows' own
- Touch and pen work through Windows' mouse emulation (tap = click, press-and-hold = right-click); untested on real touch hardware
- German UI

## License

[MIT](LICENSE). Not affiliated with or endorsed by Microsoft. Windows is a trademark of Microsoft Corporation; the optional Windows-logo start button is off by default.
