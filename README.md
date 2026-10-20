# rtaskbar

Schlanker Nachbau der Windows-11-Taskleiste samt Startmenü in Rust – reines Win32 + GDI, kein UI-Framework. COM/WinRT nur dort, wo Windows es verlangt (App-Katalog, Icons, Audio, Funk).

## Funktionen

- **Leiste:** Apps nach AppUserModelID gruppiert, Anheften, Live-Fenstervorschau (DWM), Win+1…9, Mittelklick = neue Instanz, Mausrad blättert durch Fenster
- **Startmenü:** alle Apps inkl. Store-Apps, Angeheftet, Empfohlen, Suche (Apps, Einstellungen, Dateien, Web), Energie- und Kontomenü
- **Win-Taste:** Win, Win+S, Win+X (eigenes Win+X-Menü), Win+A (Schnelleinstellungen)
- **Infobereich:** übernimmt die Tray-Icons, Überlauf wie bei Windows, optional einfarbige Icons
- **Schnelleinstellungen:** WLAN, Bluetooth, Projizieren, Lautstärke, Akku
- **Kalender** beim Klick auf die Uhr
- **Claude-/Codex-Usage** unten links (5-Stunden- und Wochenlimit)
- **Task-Manager-Knopf** mit Live-CPU-Anzeige
- **Leuchtrand** über der Leiste (Aurora, Akzentfarbe, Neon) und Animationen
- Blendet die Windows-Taskleiste aus und stellt sie beim Beenden wieder her

## Bauen & installieren

```
cargo build --release
target\release\rtaskbar.exe --install
```

`--install` kopiert die exe nach `%LOCALAPPDATA%\Programs\rtaskbar`, trägt sie in den Autostart ein und startet sie (eine laufende Instanz wird vorher beendet).

| Befehl | Wirkung |
|---|---|
| `rtaskbar.exe --uninstall` | beenden, Autostart entfernen, Windows-Taskleiste wiederherstellen |
| `rtaskbar.exe --restore` | nur die Windows-Taskleiste wiederherstellen (z. B. nach einem Absturz) |
| `rtaskbar.exe --usage-test` | Usage einmal abfragen, Ergebnis nach `%APPDATA%\rtaskbar\usage-test.txt` |

Einstellungen (Rechtsklick auf die Leiste) liegen in `%APPDATA%\rtaskbar\settings.txt`.

## Hinweise

- Die Usage-Anzeige liest die lokalen Logins von Claude Code (`~/.claude/.credentials.json`) und Codex (`~/.codex/auth.json`) und fragt deren nicht offiziell dokumentierte Usage-Schnittstellen ab. Tokens werden nirgends gespeichert oder geloggt.
- Explorer läuft weiter (Desktop, Dateifenster); nur dessen Leiste wird ausgeblendet.
