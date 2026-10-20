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

## Features

- **Taskbar** – apps grouped by AppUserModelID like Windows 11, pinning, drag & drop reordering, live window previews (DWM thumbnails), Win+1…9, middle-click for a new instance, mouse wheel to cycle a group's windows, jump lists (recent and frequent files) in the right-click menu
- **Start menu** – all apps including Store apps, pinned apps, recently used files, search across apps, settings and files (Windows Search index), web search fallback, power and account menus
- **Keyboard** – Win opens the Start menu; Win+S search, Win+X power-user menu, Win+A quick settings, Win+T moves keyboard focus into the taskbar (arrow keys, Enter, context-menu key, Esc)
- **Screen readers** – UI Automation provider: the taskbar is exposed as a toolbar whose buttons have names, descriptions and can be invoked (Narrator, NVDA)
- **System tray** – takes over the notification-area icons, overflow flyout that respects Windows' "show icon" settings, optional monochrome icons
- **Quick settings** – Wi-Fi and Bluetooth toggles, project, volume slider, battery
- **Calendar flyout** with ISO week numbers
- **Task Manager button** with a live CPU meter
- **Glow edge** above the taskbar (Aurora, accent colour or neon) and subtle animations – optional
- **Claude / Codex usage** (5-hour and weekly limits) – optional, off by default, see notes
- **Presentation mode** for streaming or screen sharing – hides your name, recent files, file search results and usage
- **Multiple monitors** – a taskbar on every display (apps, Start, clock); flyouts open on the display you clicked; full-screen apps hide only the taskbar on their display
- Hides the Windows taskbar while running and restores it on exit; maximised windows and full-screen apps are handled

## Performance

Lightbar is event-driven: when nothing happens it does almost nothing (no polling of volume, network, battery or the window list). Measured idle on the author's machine, 90 s per scenario, all other apps closed, clock with seconds enabled. CPU is reported in milliseconds of CPU time per second; Windows counts CPU time in 15.6 ms steps, so treat small differences as noise.

| | CPU (Explorer + Start/Search hosts + Lightbar) | Lightbar itself |
|---|---|---|
| Windows 11 taskbar | 1.4 ms/s | – |
| Lightbar, glow off | 1.7 ms/s | 1.0 ms/s · 7 MB |
| Lightbar, neon glow | 22.6 ms/s | 20.5 ms/s · 8 MB |

Without the glow, Lightbar is on par with the Windows taskbar. The animated glow costs about 20 ms of CPU per second. Windows' own shell processes (Explorer, Start menu host, search host) keep running in the background, so total memory goes up by Lightbar's ~7 MB rather than down.

## Install

Download `lightbar.exe` from the [releases](../../releases) or build it yourself:

```
cargo build --release
target\release\lightbar.exe --install
```

`--install` copies the executable to `%LOCALAPPDATA%\Programs\Lightbar`, adds it to autostart and starts it (a running instance is closed first).

| Command | Effect |
|---|---|
| `lightbar.exe --uninstall` | stop, remove autostart, restore the Windows taskbar |
| `lightbar.exe --restore` | only restore the Windows taskbar (e.g. after a crash) |
| `lightbar.exe --usage-test` | query usage once, write the result to `%APPDATA%\Lightbar\usage-test.txt` |
| `lightbar.exe --search-test <term>` | run an index search, write the result to `%APPDATA%\Lightbar\search-test.txt` |

Settings (right-click the taskbar) are stored in `%APPDATA%\Lightbar\settings.txt`.

## Notes

- **Antivirus / SmartScreen:** the executable is not code-signed and installs a low-level keyboard hook to take over the Windows key. Some antivirus products flag that pattern. The source is all here.
- **Usage display:** reads the local logins of Claude Code (`~/.claude/.credentials.json`) and Codex (`~/.codex/auth.json`) and calls their *undocumented* usage endpoints. Tokens are never stored, logged or sent anywhere else. These endpoints may change at any time, and using them may not be covered by the providers' terms – which is why the feature is off by default.
- Explorer keeps running (desktop, file windows); only its taskbar is hidden.

## Known limitations

- Multi-monitor support is new and not yet tested on real multi-display hardware; secondary taskbars use the primary display's scaling
- Jump lists show recent/frequent items only (no app-defined tasks); notification centre and widgets are Windows' own
- Touch and pen work through Windows' mouse emulation (tap = click, press-and-hold = right-click); untested on real touch hardware
- German UI

## License

[MIT](LICENSE). Not affiliated with or endorsed by Microsoft. Windows is a trademark of Microsoft Corporation; the optional Windows-logo start button is off by default.
