//! Claude- und Codex-Limits (5-Stunden-Fenster und Woche) für die Anzeige
//! unten links.
//!
//! Codex: bevorzugt der letzte Limit-Stand aus Codex' eigenen
//! Sitzungsprotokollen (`~/.codex/sessions`) – ohne Netz und ohne Token. Sind
//! sie älter als 15 Minuten oder leer (Codex Desktop), fragt Lightbar den
//! App-Server des offiziellen Codex-CLI (wie CodexBar), zuletzt wie bei
//! Claude die Usage-Schnittstelle, die auch `/usage` in Claude Code und Codex nutzt.
//! Die Zugangsdaten kommen bei jeder Abfrage frisch aus den lokalen
//! Login-Dateien der beiden Tools; sie werden nirgends gespeichert, geloggt
//! oder an andere Hosts geschickt. Lightbar erneuert Tokens nie selbst (das
//! würde die Refresh-Tokens der CLIs rotieren) – ist eins abgelaufen, wartet
//! die Anzeige, bis das CLI die Login-Datei erneuert.
//! Schnittstellen und Protokolle sind nicht offiziell dokumentiert – ändert
//! sich das Format, zeigt die Leiste einen Fehler statt falscher Zahlen.

use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use windows_sys::Win32::Networking::WinHttp::*;

use crate::util::w16;

#[derive(Clone, Copy, Debug)]
pub struct Window {
    /// Verbraucht in Prozent (0–100).
    pub used: f32,
    /// Unix-Sekunden des Resets, falls bekannt.
    pub resets_at: Option<i64>,
}

impl Window {
    /// Stand zum Zeitpunkt `t`: Ist der Reset schon vorbei, ist das Fenster
    /// leer (wann das nächste endet, ist dann offen).
    pub fn at(self, t: i64) -> Window {
        match self.resets_at {
            Some(r) if r <= t => Window { used: 0.0, resets_at: None },
            _ => self,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub five_hour: Option<Window>,
    pub week: Option<Window>,
}

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub claude: Option<Result<Limits, String>>,
    pub codex: Option<Result<Limits, String>>,
    /// Zeitpunkt des angezeigten Werts (für "Stand vor …").
    pub claude_at: i64,
    pub codex_at: i64,
    /// Nächster geplanter Versuch, falls gerade pausiert (429), sonst 0.
    pub claude_paused_until: i64,
    pub codex_paused_until: i64,
    /// Warum der angezeigte Wert gerade nicht neu ist (für den Tooltip).
    pub claude_note: Option<String>,
    pub codex_note: Option<String>,
    /// Kurzer, ruhiger Text für die Leiste statt "nicht verfügbar".
    pub claude_label: Option<&'static str>,
    pub codex_label: Option<&'static str>,
    /// Wert aus Codex' lokalen Protokollen.
    pub claude_local: bool,
    pub codex_local: bool,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn home() -> Option<PathBuf> {
    std::env::var("USERPROFILE").ok().map(PathBuf::from)
}

// ------------------------------------------------------------------ HTTP

struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// Proxy aus der Umgebung (`HTTPS_PROXY` / `https_proxy`), z. B.
/// `http://proxy:3128` oder `http://user:pass@proxy:3128`.
#[derive(Debug, PartialEq)]
struct EnvProxy {
    /// `host:port` für WinHTTP (ohne Schema und Zugangsdaten).
    server: String,
    user: Option<String>,
    pass: Option<String>,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Proxy-URL zerlegen. Die Verbindung zum Proxy läuft bei WinHTTP immer per
/// HTTP CONNECT, daher wird ein `https://`-Schema wie `http://` behandelt.
fn parse_proxy(url: &str) -> Option<EnvProxy> {
    let url = url.trim();
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split('/').next().unwrap_or_default();
    let (cred, server) = match rest.rsplit_once('@') {
        Some((c, s)) => (Some(c), s),
        None => (None, rest),
    };
    if server.is_empty() {
        return None;
    }
    let (user, pass) = match cred {
        Some(c) => match c.split_once(':') {
            Some((u, p)) => (Some(percent_decode(u)), Some(percent_decode(p))),
            None => (Some(percent_decode(c)), None),
        },
        None => (None, None),
    };
    Some(EnvProxy { server: server.to_string(), user, pass })
}

/// Soll `host` laut NO_PROXY-Liste (Komma-getrennt, `*`, `.domain`,
/// `domain`) am Umgebungs-Proxy vorbei?
fn no_proxy_matches(list: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    list.split(',').map(|e| e.trim().to_ascii_lowercase()).filter(|e| !e.is_empty()).any(|e| {
        if e == "*" {
            return true;
        }
        let e = e.trim_start_matches("*.").trim_start_matches('.');
        host == e || host.ends_with(&format!(".{e}"))
    })
}

fn env_proxy(host: &str) -> Option<EnvProxy> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let url = get("HTTPS_PROXY").or_else(|| get("https_proxy"))?;
    if get("NO_PROXY").or_else(|| get("no_proxy")).is_some_and(|l| no_proxy_matches(&l, host)) {
        return None;
    }
    parse_proxy(&url)
}

/// Ist ein WinHTTP-Proxy gesetzt (`netsh winhttp set proxy`)?
unsafe fn winhttp_proxy_set() -> bool {
    let mut info: WINHTTP_PROXY_INFO = std::mem::zeroed();
    if WinHttpGetDefaultProxyConfiguration(&mut info) == 0 {
        return false;
    }
    let named = info.dwAccessType == WINHTTP_ACCESS_TYPE_NAMED_PROXY && !info.lpszProxy.is_null();
    for p in [info.lpszProxy, info.lpszProxyBypass] {
        if !p.is_null() {
            windows_sys::Win32::Foundation::GlobalFree(p as _);
        }
    }
    named
}

/// HTTPS-GET über WinHTTP (Systemzertifikate). Proxy in dieser Reihenfolge:
/// `HTTPS_PROXY`/`https_proxy` (mit `NO_PROXY`), der WinHTTP-Proxy
/// (`netsh winhttp set proxy`), sonst die Windows-Proxy-Einstellungen
/// (automatisch, inkl. PAC/WPAD).
/// Rückgabe: (Status, Body, Retry-After in Sekunden, falls der Server eins nennt).
fn https_get(host: &str, path: &str, headers: &[(&str, String)]) -> Result<(u32, String, Option<i64>), String> {
    unsafe {
        let agent = w16("lightbar/0.1");
        let proxy = env_proxy(host);
        let session = match &proxy {
            Some(p) => Handle(WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_NAMED_PROXY, w16(&p.server).as_ptr(), null(), 0)),
            None if winhttp_proxy_set() => Handle(WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, null(), null(), 0)),
            None => Handle(WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0)),
        };
        if session.0.is_null() {
            return Err(t!("WinHTTP nicht verfügbar", "WinHTTP not available").into());
        }
        WinHttpSetTimeouts(session.0, 5000, 5000, 10000, 10000);
        let conn = Handle(WinHttpConnect(session.0, w16(host).as_ptr(), INTERNET_DEFAULT_HTTPS_PORT as u16, 0));
        if conn.0.is_null() {
            return Err(tf!("Keine Verbindung zu {host}", "No connection to {host}"));
        }
        let req = Handle(WinHttpOpenRequest(conn.0, w16("GET").as_ptr(), w16(path).as_ptr(), null(), null(), null(), WINHTTP_FLAG_SECURE));
        if req.0.is_null() {
            return Err(t!("Anfrage nicht möglich", "Request not possible").into());
        }
        // Keine Weiterleitungen folgen: WinHTTP schickte dabei den Authorization-
        // Header mit, das Token ginge so womöglich an einen anderen Host.
        let mut no_redirect: u32 = WINHTTP_DISABLE_REDIRECTS;
        WinHttpSetOption(req.0, WINHTTP_OPTION_DISABLE_FEATURE, &mut no_redirect as *mut u32 as _, size_of::<u32>() as u32);
        // Zugangsdaten aus der Proxy-URL: nur an WinHTTP, nie in Fehlermeldungen.
        if let Some(p) = &proxy {
            for (option, value) in [(WINHTTP_OPTION_PROXY_USERNAME, &p.user), (WINHTTP_OPTION_PROXY_PASSWORD, &p.pass)] {
                if let Some(v) = value {
                    let w = w16(v);
                    WinHttpSetOption(req.0, option, w.as_ptr() as _, (w.len() - 1) as u32);
                }
            }
        }
        let via = proxy.as_ref().map(|p| tf!(" (über Proxy {})", " (via proxy {})", p.server)).unwrap_or_default();
        let header_text: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        let h = w16(&header_text);
        if WinHttpSendRequest(req.0, h.as_ptr(), (h.len() - 1) as u32, null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(req.0, null_mut()) == 0
        {
            return Err(tf!("{host} nicht erreichbar{via}", "{host} not reachable{via}"));
        }
        let mut status: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut _ as _,
            &mut size,
            null_mut(),
        );
        let mut retry: u32 = 0;
        let mut rsize = std::mem::size_of::<u32>() as u32;
        let has_retry = WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_RETRY_AFTER | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut retry as *mut _ as _,
            &mut rsize,
            null_mut(),
        ) != 0;
        let mut body = Vec::new();
        loop {
            let mut avail = 0u32;
            if WinHttpQueryDataAvailable(req.0, &mut avail) == 0 || avail == 0 {
                break;
            }
            let mut buf = vec![0u8; avail as usize];
            let mut read = 0u32;
            if WinHttpReadData(req.0, buf.as_mut_ptr() as _, avail, &mut read) == 0 || read == 0 {
                break;
            }
            body.extend_from_slice(&buf[..read as usize]);
            if body.len() > 1 << 20 {
                break; // Antworten sind wenige KB; mehr ist kaputt.
            }
        }
        Ok((status, String::from_utf8_lossy(&body).into_owned(), has_retry.then_some(retry as i64)))
    }
}

// ------------------------------------------------------------- Parsing

/// ISO 8601 ("2026-10-20T15:00:00.123+00:00" / "…Z") → Unix-Sekunden.
fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Tage seit 1970 (Algorithmus nach H. Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut t = days * 86400 + h * 3600 + mi * 60 + se;
    // Zeitzonen-Offset am Ende (+hh:mm / -hh:mm), "Z" = UTC.
    if let Some(pos) = s[19..].find(['+', '-']) {
        let off = &s[19 + pos..];
        if off.len() >= 6 {
            let sign = if off.starts_with('-') { -1 } else { 1 };
            let oh = off.get(1..3)?.parse::<i64>().ok()?;
            let om = off.get(4..6)?.parse::<i64>().ok()?;
            t -= sign * (oh * 3600 + om * 60);
        }
    }
    Some(t)
}

fn claude_window(v: &Value) -> Option<Window> {
    let used = v.get("utilization")?.as_f64()? as f32;
    let resets_at = v.get("resets_at").and_then(Value::as_str).and_then(parse_iso);
    Some(Window { used, resets_at })
}

fn codex_window(v: &Value, fetched: i64) -> Option<Window> {
    let used = v.get("used_percent")?.as_f64()? as f32;
    let resets_at = v
        .get("reset_at")
        .and_then(Value::as_i64)
        .or_else(|| v.get("reset_after_seconds").and_then(Value::as_i64).map(|s| fetched + s));
    Some(Window { used, resets_at })
}

/// Base64url ohne Padding (JWT-Teile) → Bytes.
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').bytes() {
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Ablaufzeit (`exp`, Unix-Sekunden) eines JWT – nur das Feld, das Token
/// selbst wird nirgends ausgegeben.
fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let v: Value = serde_json::from_slice(&base64url_decode(payload)?).ok()?;
    v.get("exp")?.as_i64()
}

// ------------------------------------------------- Codex: Sitzungsprotokolle
//
// Codex schreibt nach jeder Antwort ein `token_count`-Ereignis in sein
// Sitzungsprotokoll (`~/.codex/sessions/JJJJ/MM/TT/rollout-*.jsonl`), darin
// der letzte Limit-Stand vom Server:
//
//   {"timestamp":"…Z","type":"event_msg","payload":{"type":"token_count",
//    "info":{…},"rate_limits":{"limit_id":"codex","primary":{"used_percent":12.0,
//    "window_minutes":300,"resets_at":1761000000},"secondary":{…}}}}
//
// Ältere Codex-Versionen schreiben `resets_in_seconds` (relativ zum Ereignis)
// statt `resets_at`. Codex Desktop schreibt `"rate_limits":null` – dann gibt es
// lokal nichts, und es bleibt die Usage-Schnittstelle. Lesen braucht weder
// Netz noch Token und geht deshalb jede Minute.

#[derive(Clone, Debug)]
struct Snapshot {
    limits: Limits,
    /// Zeitpunkt des Ereignisses (Unix-Sekunden).
    at: i64,
}

/// Ältere Protokolle als das zählen nicht: beide Fenster wären längst
/// zurückgesetzt, und ob seitdem anderswo (Mac, Web) gearbeitet wurde, weiß
/// das Protokoll nicht.
const LOCAL_MAX_AGE: i64 = 8 * 86400;

fn snapshot_window(v: &Value, at: i64) -> Option<(Window, Option<i64>)> {
    let used = v.get("used_percent")?.as_f64()? as f32;
    let resets_at = v
        .get("resets_at")
        .and_then(|r| r.as_i64().or_else(|| r.as_str().and_then(parse_iso)))
        .or_else(|| v.get("resets_in_seconds").and_then(Value::as_i64).map(|s| at + s));
    let minutes = v.get("window_minutes").and_then(Value::as_i64);
    Some((Window { used, resets_at }, minutes))
}

/// `rate_limits` eines `token_count`-Ereignisses → Limits. Fenster werden über
/// `window_minutes` zugeordnet (fehlt es: primary = 5 Stunden, secondary = Woche).
/// Modellspezifische Zusatzlimits (`limit_id` ≠ `codex`) zählen nicht.
fn parse_rate_limits(rl: &Value, at: i64) -> Option<Limits> {
    if rl.get("limit_id").and_then(Value::as_str).is_some_and(|id| id != "codex") {
        return None;
    }
    let mut limits = Limits { five_hour: None, week: None };
    for (key, default_week) in [("primary", false), ("secondary", true)] {
        let Some((w, minutes)) = rl.get(key).and_then(|v| snapshot_window(v, at)) else { continue };
        let week = minutes.map_or(default_week, |m| m > 24 * 60);
        let slot = if week { &mut limits.week } else { &mut limits.five_hour };
        if slot.is_none() {
            *slot = Some(w);
        }
    }
    (limits.five_hour.is_some() || limits.week.is_some()).then_some(limits)
}

/// Letzter Limit-Stand in einem Sitzungsprotokoll (von hinten gesucht).
fn parse_rollout(text: &str) -> Option<Snapshot> {
    for line in text.lines().rev() {
        if !line.contains("\"rate_limits\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let payload = v.get("payload").unwrap_or(&v);
        if payload.get("type").and_then(Value::as_str) != Some("token_count") {
            continue;
        }
        let Some(rl) = payload.get("rate_limits").filter(|r| r.is_object()) else { continue };
        let Some(at) = v.get("timestamp").and_then(Value::as_str).and_then(parse_iso) else { continue };
        if let Some(limits) = parse_rate_limits(rl, at) {
            return Some(Snapshot { limits, at });
        }
    }
    None
}

fn codex_home() -> Option<PathBuf> {
    match std::env::var("CODEX_HOME") {
        Ok(h) if !h.trim().is_empty() => Some(PathBuf::from(h)),
        _ => Some(home()?.join(".codex")),
    }
}

fn unix(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Änderungszeit einer Datei in Unix-Sekunden (0, wenn es sie nicht gibt).
fn mtime(path: &std::path::Path) -> i64 {
    std::fs::metadata(path).and_then(|m| m.modified()).map(unix).unwrap_or(0)
}

/// Kalenderdatum (Jahr, Monat, Tag, UTC) zu Unix-Sekunden.
fn ymd(unix: i64) -> (u32, u32, u32) {
    // Nach Howard Hinnant, "civil_from_days".
    let z = unix.div_euclid(86400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    (y as u32, m as u32, d as u32)
}

/// Liegt ein Datumsordner (`JJJJ`, `JJJJ/MM` oder `JJJJ/MM/TT`, als bisher
/// gelesene Zahlen) ganz vor dem Stichtag? Dann enthält er nichts Neues.
fn dated_dir_too_old(date: &[u32], since: (u32, u32, u32)) -> bool {
    let s = [since.0, since.1, since.2];
    let n = date.len().min(3);
    date[..n] < s[..n]
}

/// Alle `rollout-*.jsonl` unter `dir` (rekursiv) mit Änderungszeit ab `since`.
/// Datumsordner (`JJJJ/MM/TT`), die ganz vor `since` liegen, werden gar nicht
/// erst betreten – über Monate wächst `sessions` auf Tausende Dateien.
fn collect_rollouts(dir: &std::path::Path, since: i64, out: &mut Vec<(i64, u64, PathBuf)>) {
    // Ordner sind nach lokalem Datum benannt: einen Tag Spielraum.
    collect_rollouts_in(dir, since, ymd(since - 86400), &mut Vec::new(), out);
}

fn collect_rollouts_in(dir: &std::path::Path, since: i64, cutoff: (u32, u32, u32), date: &mut Vec<u32>, out: &mut Vec<(i64, u64, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        let path = e.path();
        if meta.is_dir() {
            // Nur Zahlen-Ordner bis Tiefe 3 als Datum lesen (Jahr, Monat, Tag).
            let part = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()).filter(|_| date.len() < 3 && date.len() == depth_of_dates(date));
            match part {
                Some(p) => {
                    date.push(p);
                    if !dated_dir_too_old(date, cutoff) {
                        collect_rollouts_in(&path, since, cutoff, date, out);
                    }
                    date.pop();
                }
                None => {
                    // Kein Datumsordner: normal hineinsehen, Datum dort nicht mehr werten.
                    date.push(u32::MAX);
                    collect_rollouts_in(&path, since, cutoff, date, out);
                    date.pop();
                }
            }
            continue;
        }
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
            continue;
        }
        let modified = meta.modified().map(unix).unwrap_or(0);
        if modified >= since {
            out.push((modified, meta.len(), path));
        }
    }
}

/// Wie viele führende Einträge von `date` echte Datumsteile sind (u32::MAX
/// markiert einen Ordner, der kein Datum ist – darunter wird nicht gefiltert).
fn depth_of_dates(date: &[u32]) -> usize {
    date.iter().take_while(|&&p| p != u32::MAX).count()
}

/// Bereits gelesene Protokolle: (Änderungszeit, Größe, Ergebnis). Unveränderte
/// Dateien werden nicht noch einmal gelesen.
type MemoEntry = (PathBuf, i64, u64, Option<Snapshot>);
static ROLLOUT_MEMO: Mutex<Vec<MemoEntry>> = Mutex::new(Vec::new());

/// Vom Protokoll wird nur das Ende gelesen: Das aktive wächst auf viele MB und
/// ändert sich nach jeder Antwort; der letzte Limit-Stand steht ganz hinten.
const ROLLOUT_TAIL: u64 = 256 * 1024;

/// Die letzten `max` Bytes einer Datei, ab der ersten vollständigen Zeile.
fn read_tail(path: &std::path::Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max);
    // Ein Byte davor mitlesen: Ist es ein Zeilenende, beginnt bei `start` eine
    // ganze Zeile.
    let from = start.saturating_sub(1);
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::with_capacity((len - from) as usize);
    f.take(len - from).read_to_end(&mut buf)?;
    if start == 0 {
        return Ok(buf);
    }
    // Angeschnittene erste Zeile verwerfen.
    match buf.iter().position(|&b| b == b'\n') {
        Some(n) => Ok(buf.split_off(n + 1)),
        None => Ok(Vec::new()),
    }
}

/// Neuester Limit-Stand aus den lokalen Codex-Protokollen.
fn local_codex() -> Option<Snapshot> {
    let base = codex_home()?;
    let since = now() - LOCAL_MAX_AGE;
    let mut files = Vec::new();
    collect_rollouts(&base.join("sessions"), since, &mut files);
    collect_rollouts(&base.join("archived_sessions"), since, &mut files);
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut memo = ROLLOUT_MEMO.lock().ok()?;
    memo.retain(|m| files.iter().any(|f| f.2 == m.0));
    let mut best: Option<Snapshot> = None;
    // Die neueste Datei hat fast immer den neuesten Stand; ein paar mehr
    // ansehen, weil parallele Sitzungen sich überholen können.
    for (modified, len, path) in files.into_iter().take(40) {
        if best.as_ref().is_some_and(|b| b.at > modified) {
            break; // ältere Dateien können nichts Neueres enthalten
        }
        let cached = memo.iter().find(|m| m.0 == path && m.1 == modified && m.2 == len).map(|m| m.3.clone());
        let snap = match cached {
            Some(s) => s,
            None => {
                let s = read_tail(&path, ROLLOUT_TAIL).ok().and_then(|b| parse_rollout(&String::from_utf8_lossy(&b)));
                memo.retain(|m| m.0 != path);
                memo.push((path, modified, len, s.clone()));
                s
            }
        };
        if let Some(s) = snap {
            if best.as_ref().is_none_or(|b| s.at > b.at) {
                best = Some(s);
            }
        }
    }
    best
}

/// Warum eine Abfrage scheiterte – bestimmt, wann es erneut versucht wird.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum FailKind {
    #[default]
    None,
    Other,
    /// Login fehlt/abgelaufen: warten, bis das CLI seine Login-Datei erneuert.
    Auth,
    /// HTTP 429: Pause einlegen (ggf. so lange, wie der Server verlangt).
    RateLimited,
    /// Codex-CLI nicht installiert: kein Fehler des App-Servers, die
    /// Schnittstelle übernimmt.
    NotFound,
}

impl FailKind {
    fn key(self) -> Option<&'static str> {
        match self {
            FailKind::None => None,
            FailKind::Other | FailKind::NotFound => Some("other"),
            FailKind::Auth => Some("auth"),
            FailKind::RateLimited => Some("rate"),
        }
    }

    fn from_key(s: &str) -> Self {
        match s {
            "auth" => FailKind::Auth,
            "rate" => FailKind::RateLimited,
            "other" => FailKind::Other,
            _ => FailKind::None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub message: String,
    kind: FailKind,
    /// Retry-After des Servers (nur bei 429).
    retry_after: Option<i64>,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure { message, kind: FailKind::Other, retry_after: None }
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

fn auth_failure(message: String) -> Failure {
    Failure { message, kind: FailKind::Auth, retry_after: None }
}

/// Ruhige Meldung, wenn das gespeicherte Token abgelaufen ist (typisch nach
/// einem Neustart): Lightbar erneuert keine Tokens – das macht das jeweilige
/// CLI beim nächsten Start, und Lightbar fragt dann von selbst wieder.
fn expired(service: &str, cli: &str) -> Failure {
    auth_failure(tf!("{service}: Anmeldung abgelaufen – die Anzeige aktualisiert sich, sobald {cli} wieder läuft", "{service}: sign-in expired - the display updates as soon as {cli} runs again"))
}

fn http_failure(service: &str, cli: &str, status: u32, retry_after: Option<i64>) -> Failure {
    match status {
        401 => expired(service, cli),
        403 => auth_failure(tf!("{service}: Login hat keine Berechtigung für die Usage-Abfrage (HTTP 403)", "{service}: sign-in is not allowed to query usage (HTTP 403)")),
        429 => Failure {
            message: tf!("{service}: vom Server gedrosselt (HTTP 429) – Lightbar fragt höchstens alle 10 Minuten und wartet jetzt länger", "{service}: rate limited by the server (HTTP 429) - Lightbar asks at most every 10 minutes and now waits longer"),
            kind: FailKind::RateLimited,
            retry_after,
        },
        _ => format!("{service}: HTTP {status}").into(),
    }
}

// ------------------------------------------------------------- Abfragen

/// Ein erfolgreich gelesener Stand.
struct Fetched {
    limits: Limits,
    /// Zeitpunkt der Daten (bei lokalen Protokollen: des Ereignisses).
    at: i64,
    /// Aus den lokalen Codex-Protokollen statt vom Server.
    local: bool,
}

/// Login-Datei je Dienst (0 = Claude, 1 = Codex) – ihre Änderungszeit zeigt,
/// dass das CLI sich neu angemeldet oder sein Token erneuert hat.
fn cred_path(i: usize) -> Option<PathBuf> {
    if i == 0 {
        Some(home()?.join(".claude").join(".credentials.json"))
    } else {
        Some(codex_home()?.join("auth.json"))
    }
}

fn fetch_claude() -> Result<Fetched, Failure> {
    let path = cred_path(0).ok_or(t!("kein Benutzerordner", "no user folder"))?;
    let text = std::fs::read_to_string(path).map_err(|_| auth_failure(t!("Claude: nicht angemeldet", "Claude: not signed in").into()))?;
    let creds: Value = serde_json::from_str(&text).map_err(|_| Failure::from(t!("Claude: Login-Datei unlesbar", "Claude: sign-in file unreadable")))?;
    let oauth = creds.get("claudeAiOauth").ok_or(t!("Claude: kein Abo-Login (API-Key-Nutzung hat keine Limits)", "Claude: no subscription sign-in (API key usage has no limits)"))?;
    let token = oauth.get("accessToken").and_then(Value::as_str).ok_or(t!("Claude: kein Abo-Login (API-Key-Nutzung hat keine Limits)", "Claude: no subscription sign-in (API key usage has no limits)"))?;
    // Abgelaufenes Token gar nicht erst schicken: gibt nur 401 und zählt
    // womöglich gegen das Abfragelimit.
    if oauth.get("expiresAt").and_then(Value::as_i64).is_some_and(|ms| ms / 1000 <= now()) {
        return Err(expired("Claude", "Claude Code"));
    }
    let (status, body, retry) = https_get(
        "api.anthropic.com",
        "/api/oauth/usage",
        &[("Authorization", format!("Bearer {token}")), ("anthropic-beta", "oauth-2025-04-20".into()), ("Accept", "application/json".into())],
    )?;
    if status != 200 {
        return Err(http_failure("Claude", "Claude Code", status, retry));
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| Failure::from(t!("Claude: Antwort unlesbar", "Claude: response unreadable")))?;
    let limits = Limits {
        five_hour: v.get("five_hour").and_then(claude_window),
        week: v.get("seven_day").and_then(claude_window),
    };
    if limits.five_hour.is_none() && limits.week.is_none() {
        return Err(t!("Claude: Antwortformat unbekannt", "Claude: unknown response format").into());
    }
    Ok(Fetched { limits, at: now(), local: false })
}

/// Codex über die Usage-Schnittstelle – nur, wenn die lokalen Protokolle
/// nichts hergeben.
fn fetch_codex_http() -> Result<Fetched, Failure> {
    let path = cred_path(1).ok_or(t!("kein Benutzerordner", "no user folder"))?;
    let text = std::fs::read_to_string(path).map_err(|_| auth_failure(t!("Codex: nicht angemeldet", "Codex: not signed in").into()))?;
    let auth: Value = serde_json::from_str(&text).map_err(|_| Failure::from(t!("Codex: Login-Datei unlesbar", "Codex: sign-in file unreadable")))?;
    let token = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .ok_or(t!("Codex: kein ChatGPT-Login (API-Key-Nutzung hat keine Limits)", "Codex: no ChatGPT sign-in (API key usage has no limits)"))?;
    if jwt_exp(token).is_some_and(|exp| exp <= now()) {
        return Err(expired("Codex", "Codex"));
    }
    let mut headers = vec![("Authorization", format!("Bearer {token}")), ("Accept", "application/json".into())];
    if let Some(account) = auth.pointer("/tokens/account_id").and_then(Value::as_str) {
        headers.push(("ChatGPT-Account-Id", account.to_string()));
    }
    let fetched = now();
    let (status, body, retry) = https_get("chatgpt.com", "/backend-api/wham/usage", &headers)?;
    if status != 200 {
        return Err(http_failure("Codex", "Codex", status, retry));
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| Failure::from(t!("Codex: Antwort unlesbar", "Codex: response unreadable")))?;
    let rl = v.get("rate_limit").ok_or(t!("Codex: Antwortformat unbekannt", "Codex: unknown response format"))?;
    let limits = Limits {
        five_hour: rl.get("primary_window").and_then(|w| codex_window(w, fetched)),
        week: rl.get("secondary_window").and_then(|w| codex_window(w, fetched)),
    };
    if limits.five_hour.is_none() && limits.week.is_none() {
        return Err(t!("Codex: Antwortformat unbekannt", "Codex: unknown response format").into());
    }
    Ok(Fetched { limits, at: fetched, local: false })
}

// ------------------------------------------------- Codex: App-Server
//
// Wie CodexBar: das offizielle Codex-CLI kurz als App-Server starten
// (`codex -s read-only -a never app-server`, JSON-RPC zeilenweise über
// stdin/stdout) und nach den Limits fragen:
//
//   → {"id":1,"method":"initialize","params":{"clientInfo":{…}}}
//   ← {"id":1,"result":{…}}
//   → {"method":"initialized"}
//   → {"id":2,"method":"account/rateLimits/read","params":{…}}
//   ← {"id":2,"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":12,
//      "windowDurationMins":300,"resetsAt":1792000000},"secondary":{…}},
//      "rateLimitsByLimitId":{"codex":{…}}}}
//
// Danach stdin schließen; Codex beendet sich dann selbst (sonst wird es
// beendet). Codex erneuert dabei bei Bedarf sein eigenes Token – das macht
// Codex selbst, Lightbar fasst `auth.json` nicht an. Gestartet wird ohne
// Fenster (CREATE_NO_WINDOW), nur im Hintergrund-Thread.

/// Paketfamilie von Codex Desktop (Microsoft Store / MSIX).
const CODEX_DESKTOP_FAMILY: &str = "OpenAI.Codex_2p2nqsd0c76g0";
/// Höchstdauer einer App-Server-Abfrage insgesamt.
const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(20);
/// Nicht gefundenes CLI erst nach dieser Zeit erneut suchen.
const CODEX_SEARCH_RETRY: i64 = 3600;

/// Wie das Codex-CLI gestartet wird: Programm und vorangestellte Argumente
/// (bei der npm-Installation ohne native Datei: `node codex.js`).
#[derive(Clone, Debug)]
struct CodexCli {
    program: PathBuf,
    prefix: Vec<PathBuf>,
}

impl CodexCli {
    fn exe(p: PathBuf) -> Self {
        CodexCli { program: p, prefix: Vec::new() }
    }

    /// Existiert noch (z. B. nach einem Update von Codex Desktop liegt es woanders).
    fn exists(&self) -> bool {
        self.program.is_file() && self.prefix.iter().all(|p| p.is_file())
    }
}

/// Datei `name` unter `dir` suchen (höchstens `depth` Ebenen tief).
fn find_file(dir: &std::path::Path, name: &str, depth: u32) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if e.file_name().to_string_lossy().eq_ignore_ascii_case(name) {
            return Some(path);
        }
    }
    if depth == 0 {
        return None;
    }
    subdirs.into_iter().find_map(|d| find_file(&d, name, depth - 1))
}

/// `node.exe` im Ordner `dir` oder auf dem PATH.
fn find_node(dir: &std::path::Path) -> Option<PathBuf> {
    let local = dir.join("node.exe");
    if local.is_file() {
        return Some(local);
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join("node.exe")).find(|p| p.is_file())
}

/// npm-Installation im Ordner `dir` (dort liegt `codex.cmd`): bevorzugt die
/// mitgelieferte `codex.exe`, sonst `node …\bin\codex.js` – nie das .cmd
/// selbst (das bräuchte cmd.exe).
fn npm_codex(dir: &std::path::Path) -> Option<CodexCli> {
    let openai = dir.join("node_modules").join("@openai");
    if !openai.is_dir() {
        return None;
    }
    if let Some(exe) = find_file(&openai, "codex.exe", 6) {
        return Some(CodexCli::exe(exe));
    }
    let script = openai.join("codex").join("bin").join("codex.js");
    if !script.is_file() {
        return None;
    }
    Some(CodexCli { program: find_node(dir)?, prefix: vec![script] })
}

/// Installationsordner von Codex Desktop (MSIX) über die Paket-API – der
/// Ordner unter `WindowsApps` wechselt mit jeder Version und ist nicht lesbar.
fn codex_desktop_dir() -> Option<PathBuf> {
    use windows_sys::Win32::Storage::Packaging::Appx::{GetPackagePathByFullName, GetPackagesByPackageFamily};
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    let family = w16(CODEX_DESKTOP_FAMILY);
    unsafe {
        let (mut count, mut len) = (0u32, 0u32);
        let r = GetPackagesByPackageFamily(family.as_ptr(), &mut count, null_mut(), &mut len, null_mut());
        if r != ERROR_INSUFFICIENT_BUFFER || count == 0 {
            return None;
        }
        let mut names = vec![null_mut::<u16>(); count as usize];
        let mut buf = vec![0u16; len as usize];
        if GetPackagesByPackageFamily(family.as_ptr(), &mut count, names.as_mut_ptr(), &mut len, buf.as_mut_ptr()) != 0 {
            return None;
        }
        // Mehrere Einträge (z. B. mitten im Update): der erste mit vorhandenem CLI.
        names.iter().take(count as usize).find_map(|&name| {
            let mut plen = 0u32;
            if GetPackagePathByFullName(name, &mut plen, null_mut()) != ERROR_INSUFFICIENT_BUFFER {
                return None;
            }
            let mut path = vec![0u16; plen as usize];
            if GetPackagePathByFullName(name, &mut plen, path.as_mut_ptr()) != 0 {
                return None;
            }
            let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
            let dir = PathBuf::from(String::from_utf16_lossy(&path[..end]));
            dir.join("app").join("resources").join("codex.exe").is_file().then_some(dir)
        })
    }
}

/// Codex-CLI suchen: PATH (codex.exe oder npm-`codex.cmd`), npm global,
/// Codex Desktop (MSIX), Codex Desktop als normale Installation.
fn find_codex_cli() -> Option<CodexCli> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let exe = dir.join("codex.exe");
            if exe.is_file() {
                return Some(CodexCli::exe(exe));
            }
            if dir.join("codex.cmd").is_file() {
                if let Some(c) = npm_codex(&dir) {
                    return Some(c);
                }
            }
        }
    }
    if let Some(c) = std::env::var_os("APPDATA").and_then(|a| npm_codex(&PathBuf::from(a).join("npm"))) {
        return Some(c);
    }
    if let Some(dir) = codex_desktop_dir() {
        return Some(CodexCli::exe(dir.join("app").join("resources").join("codex.exe")));
    }
    let programs = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Programs");
    ["Codex", "codex", "OpenAI Codex"]
        .into_iter()
        .map(|d| programs.join(d).join("resources").join("codex.exe"))
        .find(|p| p.is_file())
        .map(CodexCli::exe)
}

/// Gefundenes CLI (mit Zeitpunkt der Suche), damit nicht jedes Mal gesucht wird.
static CODEX_CLI: Mutex<Option<(i64, Option<CodexCli>)>> = Mutex::new(None);

fn codex_cli() -> Option<CodexCli> {
    let t = now();
    let mut cache = CODEX_CLI.lock().ok()?;
    let stale = match &*cache {
        None => true,
        Some((_, Some(c))) => !c.exists(),
        Some((at, None)) => t - at >= CODEX_SEARCH_RETRY,
    };
    if stale {
        *cache = Some((t, find_codex_cli()));
    }
    cache.as_ref()?.1.clone()
}

/// `account/rateLimits/read`-Ergebnis → Limits. Bevorzugt der Eintrag `codex`
/// aus `rateLimitsByLimitId`, sonst `rateLimits`; Fenster wie in den
/// Sitzungsprotokollen, nur in camelCase.
fn parse_app_server_limits(result: &Value, at: i64) -> Option<Limits> {
    let snap = result.pointer("/rateLimitsByLimitId/codex").filter(|v| v.is_object()).or_else(|| result.get("rateLimits"))?;
    let window = |w: &Value| -> Value {
        if !w.is_object() {
            return Value::Null;
        }
        serde_json::json!({
            "used_percent": w.get("usedPercent"),
            "window_minutes": w.get("windowDurationMins"),
            "resets_at": w.get("resetsAt"),
        })
    };
    let rl = serde_json::json!({
        "limit_id": snap.get("limitId"),
        "primary": snap.get("primary").map(window),
        "secondary": snap.get("secondary").map(window),
    });
    parse_rate_limits(&rl, at)
}

/// Antwort mit der Kennung `id` abwarten (Benachrichtigungen dazwischen
/// werden übergangen): Ergebnis oder Fehlermeldung des Servers.
fn rpc_wait(rx: &std::sync::mpsc::Receiver<String>, id: i64, deadline: std::time::Instant) -> Result<Value, Failure> {
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let line = match rx.recv_timeout(left) {
            Ok(l) => l,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Err(t!("Codex: App-Server antwortet nicht (Zeitüberschreitung)", "Codex: app server not responding (timeout)").into()),
            Err(_) => return Err(t!("Codex: App-Server unerwartet beendet", "Codex: app server exited unexpectedly").into()),
        };
        if let Some(r) = rpc_response(&line, id) {
            return r;
        }
    }
}

/// Eine Zeile vom App-Server: Antwort auf `id`? (`None` = andere Nachricht.)
fn rpc_response(line: &str, id: i64) -> Option<Result<Value, Failure>> {
    let mut v = serde_json::from_str::<Value>(line).ok()?;
    if v.get("id").and_then(Value::as_i64) != Some(id) {
        return None;
    }
    if let Some(e) = v.get("error") {
        let msg = e.get("message").and_then(Value::as_str).unwrap_or("unbekannter Fehler");
        let lower = msg.to_ascii_lowercase();
        let text = format!("Codex: App-Server meldet „{}“", msg.chars().take(160).collect::<String>());
        return Some(Err(if lower.contains("auth") || lower.contains("login") || lower.contains("logged") {
            auth_failure(text)
        } else {
            text.into()
        }));
    }
    Some(v.get_mut("result").map(Value::take).ok_or_else(|| t!("Codex: App-Server-Antwort unlesbar", "Codex: unreadable app server response").into()))
}

/// Codex' Limits über den App-Server des offiziellen CLI.
fn fetch_codex_app_server() -> Result<Fetched, Failure> {
    use std::io::{BufRead, Write};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let cli = codex_cli().ok_or_else(|| Failure {
        message: t!("Codex: Codex-CLI nicht gefunden", "Codex: Codex CLI not found").into(),
        kind: FailKind::NotFound,
        retry_after: None,
    })?;
    let mut child = Command::new(&cli.program)
        .args(&cli.prefix)
        .args(["-s", "read-only", "-a", "never", "app-server"])
        // Kein Projektordner: keine projektbezogene Codex-Konfiguration.
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| Failure::from(format!("Codex: App-Server startet nicht ({e})")))?;
    // Codex startet je nach Installation weitere Prozesse (node → codex.exe),
    // die die Ausgabe-Pipe erben. `kill()` träfe nur den ersten; alle zusammen
    // kommen in ein Job-Objekt, das beim Schließen den ganzen Baum beendet.
    let job = KillJob::for_child(&child);
    let deadline = std::time::Instant::now() + APP_SERVER_TIMEOUT;
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = child.stdout.take().map(|out| {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
    });
    let mut stdin = child.stdin.take();
    let result = (|| {
        let mut send = |msg: Value| -> Result<(), Failure> {
            let w = stdin.as_mut().ok_or(t!("Codex: App-Server nicht erreichbar", "Codex: app server unreachable"))?;
            writeln!(w, "{msg}").and_then(|_| w.flush()).map_err(|_| Failure::from(t!("Codex: App-Server unerwartet beendet", "Codex: app server exited unexpectedly")))
        };
        let version = env!("CARGO_PKG_VERSION");
        send(serde_json::json!({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "lightbar", "title": "Lightbar", "version": version}}}))?;
        rpc_wait(&rx, 1, deadline)?;
        send(serde_json::json!({"method": "initialized"}))?;
        send(serde_json::json!({"id": 2, "method": "account/rateLimits/read", "params": {"excludeResetCreditDetails": true}}))?;
        let fetched = now();
        let r = rpc_wait(&rx, 2, deadline)?;
        let limits = parse_app_server_limits(&r, fetched).ok_or(t!("Codex: App-Server-Antwort ohne Limits", "Codex: app server response has no limits"))?;
        Ok(Fetched { limits, at: fetched, local: false })
    })();
    // stdin schließen → Codex beendet sich; sonst nach kurzer Frist beenden.
    drop(stdin);
    let quit = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < quit => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                if let Some(j) = &job {
                    j.terminate();
                }
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    // Übrig gebliebene Kindprozesse beenden (Job schließen) – erst dann schließt
    // sich die Pipe und der Lese-Thread endet.
    drop(job);
    drop(rx);
    if let Some(reader) = reader {
        let until = std::time::Instant::now() + Duration::from_secs(2);
        while !reader.is_finished() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        if reader.is_finished() {
            let _ = reader.join();
        } else {
            crate::util::log("Codex-App-Server: Lese-Thread endet nicht (Pipe noch offen)");
        }
    }
    result
}

/// Job-Objekt, das beim Schließen alle zugeordneten Prozesse beendet
/// (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE).
struct KillJob(windows_sys::Win32::Foundation::HANDLE);

impl KillJob {
    /// Job anlegen und `child` zuordnen; Prozesse, die es danach startet,
    /// gehören automatisch dazu. `None`, wenn das nicht geht (dann bleibt `kill()`).
    fn for_child(child: &std::process::Child) -> Option<KillJob> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let h = CreateJobObjectW(null(), null());
            if h.is_null() {
                return None;
            }
            let job = KillJob(h);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0
                && AssignProcessToJobObject(h, child.as_raw_handle() as _) != 0;
            ok.then_some(job)
        }
    }

    fn terminate(&self) {
        unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1) };
    }
}

impl Drop for KillJob {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Nur so junge Protokolle gelten als aktuell; ältere zeigt die Leiste zwar
/// an, fragt aber trotzdem den App-Server.
const LOCAL_FRESH: i64 = 15 * 60;

/// Codex ohne frische lokale Protokolle: App-Server, sonst als letzter
/// Ausweg die Usage-Schnittstelle mit dem gespeicherten Token.
/// Den App-Server höchstens alle 10 Minuten starten, auch bei Fehlschlägen
/// und Klicks – dazwischen bleibt die Schnittstelle (mit dem Token, das
/// Codex dabei womöglich gerade erneuert hat).
static APP_SERVER_AT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Darf der App-Server jetzt (`t`) starten? Zeitpunkt in der Zukunft (Uhr
/// zurückgestellt) gilt als fällig. Nur wer den Zeitstempel per
/// compare_exchange setzt, startet ihn – zwei Threads nie gleichzeitig.
fn claim_app_server(t: i64) -> bool {
    use std::sync::atomic::Ordering;
    let last = APP_SERVER_AT.load(Ordering::Acquire);
    let due = last > t || t - last >= INTERVAL;
    due && APP_SERVER_AT.compare_exchange(last, t, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

fn fetch_codex_remote() -> Result<Fetched, Failure> {
    let app = if claim_app_server(now()) {
        match fetch_codex_app_server() {
            Ok(f) => return Ok(f),
            Err(e) if e.kind == FailKind::NotFound => None,
            Err(e) => Some(e),
        }
    } else {
        None
    };
    match fetch_codex_http() {
        Ok(f) => Ok(f),
        // App-Server nicht gefragt (kein CLI, gerade erst): Fehler der Schnittstelle.
        Err(http) => match app {
            None => Err(http),
            // Abgelaufener Login: ruhig warten (wie bisher), Grund des App-Servers dazu.
            Some(app) if http.kind == FailKind::Auth => {
                let why = app.message.trim_start_matches("Codex: ");
                Err(Failure { message: format!("{} ({why})", http.message), ..http })
            }
            Some(app) => Err(app),
        },
    }
}

/// Codex: zuerst frische lokale Protokolle, dann App-Server, dann Schnittstelle.
fn fetch_codex() -> Result<Fetched, Failure> {
    match local_codex() {
        Some(s) if s.at >= now() - LOCAL_FRESH => Ok(Fetched { limits: s.limits, at: s.at, local: true }),
        _ => fetch_codex_remote(),
    }
}

// --------------------------------------------------------- Zeitplanung
//
// Rücksichtsvoll abfragen: je Dienst höchstens alle 10 Minuten, bei HTTP 429
// Pause (Retry-After des Servers, sonst 10 → 20 → 40 → 60 Minuten). Ein
// abgelaufener Login (401) ist keine Drosselung: Lightbar wartet, bis das CLI
// seine Login-Datei erneuert (Änderungszeit), und fragt dann sofort – sonst
// nur im normalen Takt, und mit abgelaufenem Token ganz ohne Netz.
// Codex' lokale Protokolle kosten nichts und werden jede Minute gelesen.
// Werte und Zeitplan liegen in `usage-cache.json`, damit ein Neustart nicht
// sofort neu fragt, sondern die letzten Werte zeigt.

const INTERVAL: i64 = 600;
const MAX_BACKOFF: i64 = 3600;
const MIN_MANUAL: i64 = 60;

#[derive(Clone, Default)]
struct Slot {
    value: Option<Result<Limits, String>>,
    /// Zeitpunkt des letzten erfolgreichen Werts.
    fetched_at: i64,
    next_at: i64,
    backoff: i64,
    /// Letzter Fehlschlag (Art und Zeitpunkt), auch wenn der alte Wert bleibt.
    fail: FailKind,
    failed_at: i64,
    /// Hinweis zum Fehlschlag, während der alte Wert stehen bleibt.
    note: Option<String>,
    /// Wert stammt aus den lokalen Codex-Protokollen.
    local: bool,
    /// Eine Abfrage läuft gerade (nicht gespeichert): kein zweiter Thread
    /// fragt denselben Dienst parallel.
    in_flight: bool,
}

const EMPTY_SLOT: Slot = Slot {
    value: None,
    fetched_at: 0,
    next_at: 0,
    backoff: 0,
    fail: FailKind::None,
    failed_at: 0,
    note: None,
    local: false,
    in_flight: false,
};

/// Index 0 = Claude, 1 = Codex.
static SLOTS: Mutex<[Slot; 2]> = Mutex::new([EMPTY_SLOT, EMPTY_SLOT]);

fn fetch_slot(i: usize) -> Result<Fetched, Failure> {
    if i == 0 {
        fetch_claude()
    } else {
        fetch_codex_remote()
    }
}

/// Ergebnis einer Abfrage in den Zeitplan einarbeiten.
fn apply(slot: &mut Slot, result: Result<Fetched, Failure>, t: i64) {
    slot.in_flight = false;
    match result {
        Ok(f) => {
            slot.value = Some(Ok(f.limits));
            slot.fetched_at = f.at;
            slot.local = f.local;
            slot.backoff = 0;
            slot.next_at = t + INTERVAL;
            slot.fail = FailKind::None;
            slot.failed_at = 0;
            slot.note = None;
        }
        Err(f) => {
            slot.fail = f.kind;
            slot.failed_at = t;
            let wait = match f.kind {
                FailKind::RateLimited => {
                    slot.backoff = f.retry_after.unwrap_or(if slot.backoff == 0 { INTERVAL } else { slot.backoff * 2 }).clamp(INTERVAL, MAX_BACKOFF);
                    slot.backoff
                }
                // Kein Backoff: erneut wird ohnehin sofort gefragt, wenn sich die
                // Login-Datei ändert; mit abgelaufenem Token geht dabei nichts ins Netz.
                FailKind::Auth => {
                    slot.backoff = 0;
                    INTERVAL
                }
                _ => {
                    slot.backoff = 0;
                    INTERVAL / 2
                }
            };
            slot.next_at = t + wait;
            let msg = if f.kind == FailKind::RateLimited {
                tf!("{} – nächster Versuch {}", "{} - next attempt {}", f.message, until(slot.next_at))
            } else {
                f.message
            };
            // Letzten guten Wert behalten (Resets rechnet `current` heraus), den
            // Grund nur als Hinweis dazu.
            if matches!(slot.value, Some(Ok(_))) {
                slot.note = Some(msg);
            } else {
                slot.value = Some(Err(msg));
                slot.note = None;
            }
        }
    }
}

/// Hat sich die Login-Datei seit dem letzten Fehlschlag geändert (neu
/// angemeldet, Token vom CLI erneuert)? Dann gilt ein 401/429 nicht mehr.
fn login_changed(slot: &Slot, cred_mtime: i64) -> bool {
    matches!(slot.fail, FailKind::Auth | FailKind::RateLimited) && cred_mtime > slot.failed_at
}

/// Ist ein Dienst jetzt fällig? `manual`: auf Klick, aber nicht öfter als
/// einmal pro Minute und nie während einer 429-Pause.
fn is_due(slot: &Slot, t: i64, manual: bool, cred_mtime: i64) -> bool {
    // Wanduhr: Zeitpunkte in der Zukunft (Uhr zurückgestellt, Cache von einem
    // Rechner mit falscher Uhr) gelten als "nie" – sonst stünde die Anzeige
    // tagelang. `next_at` begrenzt `claim_due` bzw. `slot_from`.
    let past = |x: i64| if x > t { 0 } else { x };
    let (fetched_at, failed_at) = (past(slot.fetched_at), past(slot.failed_at));
    // Nach neuem Login sofort – aber nicht öfter als einmal pro Minute.
    let login_changed = matches!(slot.fail, FailKind::Auth | FailKind::RateLimited) && cred_mtime > failed_at;
    if login_changed && t - failed_at >= MIN_MANUAL {
        return true;
    }
    if manual {
        slot.backoff == 0 && t - fetched_at >= MIN_MANUAL && t - failed_at >= MIN_MANUAL
    } else {
        t >= slot.next_at
    }
}

/// Fällige Dienste bestimmen und (unter der Sperre der Slots) als laufend
/// belegen; `apply` gibt sie wieder frei. Läuft schon eine Abfrage (Klick
/// während des Takts, mehrere Klicks), wird nicht parallel ein zweites Mal
/// gefragt.
fn claim_due(s: &mut [Slot; 2], wanted: [bool; 2], t: i64, manual: bool, cred_mtimes: [i64; 2]) -> Vec<usize> {
    // Uhr zurückgestellt: nächster Versuch höchstens MAX_BACKOFF entfernt –
    // gespeichert, sonst schöbe sich die Grenze mit jedem Durchlauf mit.
    for sl in s.iter_mut() {
        sl.next_at = sl.next_at.min(t + MAX_BACKOFF);
    }
    let due: Vec<usize> = (0..2).filter(|&i| wanted[i] && !s[i].in_flight && is_due(&s[i], t, manual, cred_mtimes[i])).collect();
    for &i in &due {
        s[i].in_flight = true;
    }
    due
}

/// Fällige Dienste abfragen.
fn run_due(manual: bool) -> bool {
    // Ausgeschaltet: keine Login-Dateien lesen, nichts abfragen – auch nicht für
    // einen einzeln abgeschalteten Dienst.
    let settings = crate::settings::get();
    if !settings.show_usage {
        return false;
    }
    let enabled = [settings.usage_claude, settings.usage_codex];
    // Codex lokal: ohne Netz, daher bei jedem Durchlauf.
    let local = if enabled[1] { local_codex() } else { None };
    let cred_mtimes = [0, 1].map(|i| cred_path(i).map_or(0, |p| mtime(&p)));
    let t = now();
    let mut changed = false;
    let due: Vec<usize> = match SLOTS.lock() {
        Ok(mut s) => {
            if let Some(snap) = &local {
                let sl = &mut s[1];
                let has_value = matches!(sl.value, Some(Ok(_)));
                if snap.at >= t - LOCAL_FRESH {
                    if !(sl.local && sl.fetched_at == snap.at && has_value) {
                        // Eine laufende Abfrage bleibt belegt (sie ruft selbst `apply`).
                        let busy = sl.in_flight;
                        apply(sl, Ok(Fetched { limits: snap.limits.clone(), at: snap.at, local: true }), t);
                        sl.in_flight = busy;
                        changed = true;
                    }
                } else if !has_value || snap.at > sl.fetched_at {
                    // Älterer Stand: anzeigen, solange nichts Neueres da ist, den
                    // Zeitplan für den App-Server aber nicht anfassen.
                    if let Some(Err(e)) = sl.value.take() {
                        sl.note = Some(e);
                    }
                    sl.value = Some(Ok(snap.limits.clone()));
                    sl.fetched_at = snap.at;
                    sl.local = true;
                    changed = true;
                }
            }
            let wanted = [enabled[0], enabled[1] && !local.as_ref().is_some_and(|l| l.at >= t - LOCAL_FRESH)];
            claim_due(&mut s, wanted, t, manual, cred_mtimes)
        }
        Err(_) => return false,
    };
    if due.is_empty() {
        if changed {
            if let Ok(s) = SLOTS.lock() {
                save_cache(&s);
            }
        }
        return changed;
    }
    // Netzwerk außerhalb der Sperre.
    let results: Vec<(usize, Result<Fetched, Failure>)> = due.into_iter().map(|i| (i, fetch_slot(i))).collect();
    if let Ok(mut s) = SLOTS.lock() {
        let t = now();
        for (i, r) in results {
            apply(&mut s[i], r, t);
        }
        save_cache(&s);
    }
    true
}

/// Limits zum Zeitpunkt `t`: abgelaufene Fenster sind leer.
fn limits_at(l: &Limits, t: i64) -> Limits {
    Limits { five_hour: l.five_hour.map(|w| w.at(t)), week: l.week.map(|w| w.at(t)) }
}

pub fn current() -> Option<Usage> {
    let s = SLOTS.lock().ok()?;
    let t = now();
    let value = |sl: &Slot| sl.value.as_ref().map(|r| r.as_ref().map(|l| limits_at(l, t)).map_err(Clone::clone));
    let label = |sl: &Slot| match sl.fail {
        FailKind::Auth => Some(t!("wartet auf Anmeldung", "waiting for sign-in")),
        _ => None,
    };
    Some(Usage {
        claude: value(&s[0]),
        codex: value(&s[1]),
        claude_at: s[0].fetched_at,
        codex_at: s[1].fetched_at,
        claude_paused_until: if s[0].backoff > 0 { s[0].next_at } else { 0 },
        codex_paused_until: if s[1].backoff > 0 { s[1].next_at } else { 0 },
        claude_note: s[0].note.clone(),
        codex_note: s[1].note.clone(),
        claude_label: label(&s[0]),
        codex_label: label(&s[1]),
        claude_local: s[0].local,
        codex_local: s[1].local,
    })
}

// ------------------------------------------------------------- Cache

fn cache_path() -> Option<PathBuf> {
    Some(crate::util::data_dir()?.join("usage-cache.json"))
}

fn window_json(w: &Option<Window>) -> Value {
    match w {
        Some(w) => serde_json::json!({ "used": w.used, "resets_at": w.resets_at }),
        None => Value::Null,
    }
}

fn window_from(v: &Value) -> Option<Window> {
    Some(Window { used: v.get("used")?.as_f64()? as f32, resets_at: v.get("resets_at").and_then(Value::as_i64) })
}

fn slot_json(s: &Slot) -> Value {
    let (five, week, error) = match &s.value {
        Some(Ok(l)) => (window_json(&l.five_hour), window_json(&l.week), Value::Null),
        Some(Err(e)) => (Value::Null, Value::Null, Value::from(e.as_str())),
        None => (Value::Null, Value::Null, Value::Null),
    };
    serde_json::json!({
        "fetched_at": s.fetched_at, "next_at": s.next_at, "backoff": s.backoff,
        "five_hour": five, "week": week, "error": error,
        "fail": s.fail.key(), "failed_at": s.failed_at, "note": s.note, "local": s.local,
    })
}

/// Cache-Eintrag → Slot. `cred_mtime`: Änderungszeit der Login-Datei jetzt,
/// `t`: jetzt (Zeitpunkte in der Zukunft gelten als "nie", siehe `is_due`).
fn slot_from(e: &Value, cred_mtime: i64, t: i64) -> Slot {
    let num = |k: &str| e.get(k).and_then(Value::as_i64).unwrap_or(0);
    let mut s = Slot::default();
    let limits = Limits { five_hour: e.get("five_hour").and_then(window_from), week: e.get("week").and_then(window_from) };
    let fetched_at = num("fetched_at");
    if limits.five_hour.is_some() || limits.week.is_some() {
        s.value = Some(Ok(limits));
        s.fetched_at = fetched_at;
    } else if let Some(err) = e.get("error").and_then(Value::as_str) {
        s.value = Some(Err(err.to_string()));
    }
    s.failed_at = num("failed_at");
    s.note = e.get("note").and_then(Value::as_str).map(str::to_string);
    s.local = e.get("local").and_then(Value::as_bool).unwrap_or(false);
    s.fail = match e.get("fail").and_then(Value::as_str) {
        Some(k) => FailKind::from_key(k),
        // Cache älterer Versionen: Art aus der Meldung, Zeitpunkt unbekannt.
        None => match e.get("error").and_then(Value::as_str) {
            Some(m) if m.contains("HTTP 401") || m.contains("HTTP 403") || m.contains("nicht angemeldet") => FailKind::Auth,
            Some(m) if m.contains("HTTP 429") => FailKind::RateLimited,
            Some(_) => FailKind::Other,
            None => FailKind::None,
        },
    };
    // Pause nur für eine echte Drosselung (ältere Versionen ließen sie nach
    // einem 429 auch bei einem späteren 401 stehen).
    if s.fail == FailKind::RateLimited {
        s.backoff = num("backoff");
    }
    // Nach einem Neustart frühestens zum geplanten Zeitpunkt wieder fragen …
    // Nie weiter als die längste Pause entfernt (Uhr zurückgestellt).
    s.next_at = num("next_at").max(fetched_at + INTERVAL).min(t + MAX_BACKOFF);
    if s.failed_at > t {
        s.failed_at = 0;
    }
    // … außer der gespeicherte 401/429 ist durch einen neuen Login überholt.
    if login_changed(&s, cred_mtime) {
        s.next_at = 0;
        s.backoff = 0;
        s.failed_at = 0;
    }
    s
}

fn save_cache(slots: &[Slot; 2]) {
    let doc = serde_json::json!({ "claude": slot_json(&slots[0]), "codex": slot_json(&slots[1]) });
    if let Some(p) = cache_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Atomar: erst eine Nebendatei schreiben, dann ersetzen (rename = MoveFileExW
        // mit MOVEFILE_REPLACE_EXISTING). Ein Absturz mittendrin hinterlässt so nie
        // eine halbe Datei.
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, doc.to_string()).is_ok() && std::fs::rename(&tmp, &p).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

fn load_cache() {
    let Some(text) = cache_path().and_then(|p| std::fs::read_to_string(p).ok()) else { return };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else { return };
    let Ok(mut s) = SLOTS.lock() else { return };
    for (i, key) in ["claude", "codex"].into_iter().enumerate() {
        let Some(e) = doc.get(key) else { continue };
        s[i] = slot_from(e, cred_path(i).map_or(0, |p| mtime(&p)), now());
    }
}

// ----------------------------------------------------------- Threads

/// Hintergrund-Thread: prüft jede Minute, ob ein Dienst fällig ist, und
/// meldet sich per `notify` (PostMessage an die Leiste) nach jeder Abfrage.
pub fn spawn(notify: impl Fn() + Send + 'static) {
    load_cache();
    notify();
    std::thread::spawn(move || loop {
        if run_due(false) {
            notify();
        }
        std::thread::park_timeout(Duration::from_secs(60));
    });
}

/// Klick auf die Anzeige: neu abfragen, soweit erlaubt.
pub fn refresh_now(notify: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        if run_due(true) {
            notify();
        }
    });
}

// --------------------------------------------------------------- Anzeige

/// "in 2 Std. 10 Min." / "in 3 Tagen"
pub fn until(t: i64) -> String {
    let s = (t - now()).max(0);
    match s {
        0..=59 => t!("gleich", "soon").into(),
        60..=3599 => tf!("in {} Min.", "in {} min", s / 60),
        3600..=86399 => tf!("in {} Std. {} Min.", "in {} h {} min", s / 3600, (s % 3600) / 60),
        _ => tf!("in {} Tagen", "in {} days", (s + 43200) / 86400),
    }
}

/// "5 Min." / "3 Std." / "2 Tagen" – für "Stand vor …".
fn age_text(s: i64) -> String {
    match s {
        ..=3599 => tf!("{} Min.", "{} min", s / 60),
        3600..=86399 => tf!("{} Std.", "{} h", s / 3600),
        86400..=172799 => t!("1 Tag", "1 day").into(),
        _ => tf!("{} Tagen", "{} days", s / 86400),
    }
}

/// Tooltip-Text für einen Dienst. `local`: Wert aus Codex' Protokollen,
/// `note`: Grund, warum der Wert gerade nicht neu ist.
pub fn describe(name: &str, r: &Option<Result<Limits, String>>, fetched_at: i64, local: bool, note: Option<&str>) -> String {
    match r {
        None => tf!("{name}: wird abgefragt …", "{name}: checking ..."),
        Some(Err(e)) => e.clone(),
        Some(Ok(l)) => {
            // Fenster ohne Limit (z. B. kein Wochenlimit im Abo) erscheinen gar nicht.
            let line = |label: &str, w: &Option<Window>| {
                w.map(|w| {
                    let reset = w.resets_at.map(|t| format!(" · Reset {}", until(t))).unwrap_or_default();
                    format!("\n  {label}: {:.0} %{reset}", w.used)
                })
                .unwrap_or_default()
            };
            let age = now() - fetched_at;
            let src = if local { t!("letzte Codex-Sitzung", "last Codex session") } else { t!("Stand", "as of") };
            let stand = if fetched_at > 0 && age >= 120 {
                tf!(" ({src} vor {})", " ({src} {} ago)", age_text(age))
            } else if local {
                t!(" (letzte Codex-Sitzung)", " (last Codex session)").to_string()
            } else {
                String::new()
            };
            let note = note.map(|n| format!("\n  {n}")).unwrap_or_default();
            format!("{name}{stand}{}{}{note}", line(t!("5 Stunden", "5 hours"), &l.five_hour), line(t!("Woche", "Week"), &l.week))
        }
    }
}

/// `--usage-test`: einmal abfragen und nur Prozentwerte/Fehler in eine Datei
/// schreiben – zum Prüfen ohne Tokens anzuzeigen.
pub fn self_test() {
    let t = now();
    let d = |r: Result<Fetched, Failure>| match r {
        Ok(f) => (Some(Ok(limits_at(&f.limits, t))), f.at, f.local),
        Err(f) => (Some(Err(f.message)), t, false),
    };
    let (c, c_at, c_local) = d(fetch_claude());
    let (x, x_at, x_local) = d(fetch_codex());
    let text = format!("{}\n\n{}\n", describe("Claude", &c, c_at, c_local, None), describe("Codex", &x, x_at, x_local, None));
    if let Ok(appdata) = std::env::var("APPDATA") {
        let dir = PathBuf::from(appdata).join("Lightbar");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("usage-test.txt"), text);
    }
}

#[cfg(test)]
mod proxy_tests {
    use super::*;

    #[test]
    fn parses_proxy_urls() {
        assert_eq!(parse_proxy("http://proxy.local:3128"), Some(EnvProxy { server: "proxy.local:3128".into(), user: None, pass: None }));
        assert_eq!(parse_proxy("proxy:8080/"), Some(EnvProxy { server: "proxy:8080".into(), user: None, pass: None }));
        assert_eq!(
            parse_proxy("https://us%40er:p%3Ass@10.0.0.1:8443"),
            Some(EnvProxy { server: "10.0.0.1:8443".into(), user: Some("us@er".into()), pass: Some("p:ss".into()) })
        );
        assert_eq!(parse_proxy("http://"), None);
    }

    #[test]
    fn no_proxy_list() {
        assert!(no_proxy_matches("localhost, .anthropic.com", "api.anthropic.com"));
        assert!(no_proxy_matches("chatgpt.com", "chatgpt.com"));
        assert!(no_proxy_matches("*", "chatgpt.com"));
        assert!(!no_proxy_matches("openai.com", "chatgpt.com"));
        assert!(!no_proxy_matches("pt.com", "chatgpt.com"));
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    // Gekürzte, erfundene Zeilen im Format der Codex-Sitzungsprotokolle.
    const META: &str = r#"{"timestamp":"2026-10-21T05:03:51.761Z","type":"session_meta","payload":{"id":"x","originator":"codex_cli_rs"}}"#;
    const NULL_LIMITS: &str = r#"{"timestamp":"2026-10-21T05:04:00.000Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":null}}"#;
    const NEW_FORMAT: &str = r#"{"timestamp":"2026-10-21T05:05:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10}},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":12.5,"window_minutes":300,"resets_at":1792000000},"secondary":{"used_percent":40.0,"window_minutes":10080,"resets_at":1792500000},"credits":null,"plan_type":"plus"}}}"#;
    const SPARK: &str = r#"{"timestamp":"2026-10-21T05:06:00.000Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex_spark","primary":{"used_percent":99.0,"window_minutes":300,"resets_at":1792000000},"secondary":null}}}"#;
    const OLD_FORMAT: &str = r#"{"timestamp":"2026-10-21T05:00:00Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"primary":{"used_percent":5.0,"window_minutes":299,"resets_in_seconds":600},"secondary":{"used_percent":20.0,"window_minutes":10079,"resets_in_seconds":3600}}}}"#;
    const MESSAGE: &str = r#"{"timestamp":"2026-10-21T05:07:00.000Z","type":"response_item","payload":{"type":"message","content":[{"type":"output_text","text":"\"rate_limits\" im Text"}]}}"#;

    fn join(lines: &[&str]) -> String {
        lines.join("\n")
    }

    #[test]
    fn newest_snapshot_wins_and_others_are_skipped() {
        // Neuester gültiger Stand ist NEW_FORMAT: SPARK (anderes Limit), MESSAGE
        // (kein token_count) und null-Limits danach zählen nicht.
        let s = parse_rollout(&join(&[META, OLD_FORMAT, NEW_FORMAT, SPARK, NULL_LIMITS, MESSAGE])).unwrap();
        assert_eq!(s.at, parse_iso("2026-10-21T05:05:00Z").unwrap());
        let five = s.limits.five_hour.unwrap();
        let week = s.limits.week.unwrap();
        assert_eq!((five.used, five.resets_at), (12.5, Some(1792000000)));
        assert_eq!((week.used, week.resets_at), (40.0, Some(1792500000)));
    }

    #[test]
    fn old_format_resets_relative_to_event() {
        let s = parse_rollout(&join(&[META, OLD_FORMAT])).unwrap();
        let at = parse_iso("2026-10-21T05:00:00Z").unwrap();
        assert_eq!(s.at, at);
        assert_eq!(s.limits.five_hour.unwrap().resets_at, Some(at + 600));
        assert_eq!(s.limits.week.unwrap().resets_at, Some(at + 3600));
    }

    #[test]
    fn no_snapshot_without_rate_limits() {
        assert!(parse_rollout(&join(&[META, NULL_LIMITS, MESSAGE])).is_none());
        assert!(parse_rollout("").is_none());
        assert!(parse_rollout("kaputt\n{\"rate_limits\":").is_none());
    }

    #[test]
    fn windows_assigned_by_length() {
        // Nur ein Wochenfenster, als primary geliefert.
        let rl: Value = serde_json::from_str(r#"{"primary":{"used_percent":7,"window_minutes":10080,"resets_at":5}}"#).unwrap();
        let l = parse_rate_limits(&rl, 0).unwrap();
        assert!(l.five_hour.is_none());
        assert_eq!(l.week.unwrap().used, 7.0);
    }

    #[test]
    fn passed_reset_means_empty_window() {
        let w = Window { used: 80.0, resets_at: Some(1000) };
        assert_eq!(w.at(999).used, 80.0);
        let after = w.at(1000);
        assert_eq!((after.used, after.resets_at), (0.0, None));
        let unknown = Window { used: 30.0, resets_at: None };
        assert_eq!(unknown.at(i64::MAX).used, 30.0);
    }

    #[test]
    fn jwt_expiry() {
        // {"exp":1700000000} als base64url, Signatur egal.
        let token = format!("e30.{}.sig", "eyJleHAiOjE3MDAwMDAwMDB9");
        assert_eq!(jwt_exp(&token), Some(1700000000));
        assert_eq!(jwt_exp("kein-jwt"), None);
    }

    fn failed(kind: FailKind) -> Failure {
        Failure { message: "x".into(), kind, retry_after: None }
    }

    #[test]
    fn auth_failure_has_no_backoff_and_retries_after_login() {
        let mut s = Slot::default();
        apply(&mut s, Err(failed(FailKind::Auth)), 1000);
        assert_eq!(s.backoff, 0);
        assert!(s.next_at <= 1000 + INTERVAL);
        // Login-Datei unverändert: nicht vor dem Takt fällig.
        assert!(!is_due(&s, 1100, false, 900));
        // Neu angemeldet: sofort (ab einer Minute nach dem Fehlschlag).
        assert!(is_due(&s, 1100, false, 1050));
        assert!(!is_due(&s, 1030, false, 1050));
    }

    #[test]
    fn rate_limit_backs_off_but_new_login_retries() {
        let mut s = Slot::default();
        apply(&mut s, Err(failed(FailKind::RateLimited)), 1000);
        assert_eq!(s.backoff, INTERVAL);
        assert!(!is_due(&s, 1200, true, 900));
        assert!(is_due(&s, 1200, false, 1100));
    }

    #[test]
    fn auth_failure_keeps_last_value_with_note() {
        let mut s = Slot::default();
        let limits = Limits { five_hour: Some(Window { used: 10.0, resets_at: None }), week: None };
        apply(&mut s, Ok(Fetched { limits, at: 500, local: false }), 500);
        apply(&mut s, Err(failed(FailKind::Auth)), 1000);
        assert!(matches!(s.value, Some(Ok(_))));
        assert_eq!(s.fetched_at, 500);
        assert!(s.note.is_some());
    }

    #[test]
    fn stale_cached_errors_clear_after_login() {
        // Cache einer älteren Version: 401 nach einem 429, Pause stehen geblieben.
        let legacy: Value = serde_json::from_str(
            r#"{"fetched_at":0,"next_at":5000,"backoff":3600,"five_hour":null,"week":null,"error":"Claude: Login abgelaufen – Claude einmal starten (HTTP 401)"}"#,
        )
        .unwrap();
        let s = slot_from(&legacy, 4000, 10_000);
        assert_eq!(s.fail, FailKind::Auth);
        assert_eq!((s.backoff, s.next_at), (0, 0));

        // Neuer Cache: 429 um 2000, Login-Datei seitdem unverändert → Pause bleibt.
        let rate: Value = serde_json::from_str(
            r#"{"fetched_at":0,"next_at":2600,"backoff":600,"error":"x","fail":"rate","failed_at":2000}"#,
        )
        .unwrap();
        let s = slot_from(&rate, 1500, 10_000);
        assert_eq!((s.backoff, s.next_at), (600, 2600));
        // … nach neuem Login (Datei jünger als der Fehler) sofort wieder.
        let s = slot_from(&rate, 2500, 10_000);
        assert_eq!((s.backoff, s.next_at), (0, 0));
    }

    #[test]
    fn in_flight_slot_is_not_claimed_twice() {
        let mut s = [Slot::default(), Slot::default()];
        let t = 100_000;
        assert_eq!(claim_due(&mut s, [true, true], t, true, [0, 0]), [0, 1]);
        // Zweiter Klick, während beide noch laufen: nichts.
        assert!(claim_due(&mut s, [true, true], t + 1, false, [0, 0]).is_empty());
        // Ergebnis eingearbeitet → wieder frei (und dann im Takt fällig).
        apply(&mut s[0], Err(failed(FailKind::Other)), t + 2);
        assert!(!s[0].in_flight);
        assert!(s[1].in_flight);
        assert_eq!(claim_due(&mut s, [true, true], t + 2 + INTERVAL, false, [0, 0]), [0]);
    }

    #[test]
    fn future_timestamps_do_not_block() {
        let t = 1_000_000;
        // Wert und Fehlschlag "aus der Zukunft" (Uhr zurückgestellt).
        let s = Slot { fetched_at: t + 86_400, failed_at: t + 86_400, next_at: t + 30 * 86_400, ..Slot::default() };
        assert!(is_due(&s, t, true, 0));
        let mut slots = [s.clone(), Slot::default()];
        assert!(claim_due(&mut slots, [true, false], t, false, [0, 0]).is_empty());
        assert_eq!(slots[0].next_at, t + MAX_BACKOFF);
        assert!(claim_due(&mut slots, [true, false], t + 1800, false, [0, 0]).is_empty());
        assert_eq!(claim_due(&mut slots, [true, false], t + MAX_BACKOFF, false, [0, 0]), [0]);
        // Cache: nächster Versuch höchstens MAX_BACKOFF entfernt.
        let e: Value = serde_json::from_str(&format!(r#"{{"fetched_at":{},"next_at":{},"failed_at":{},"fail":"other","error":"x"}}"#, t + 9_999_999, t + 9_999_999, t + 50)).unwrap();
        let s = slot_from(&e, 0, t);
        assert_eq!(s.next_at, t + MAX_BACKOFF);
        assert_eq!(s.failed_at, 0);
    }

    #[test]
    fn app_server_claimed_once() {
        use std::sync::atomic::Ordering;
        let t = 2_000_000_000;
        APP_SERVER_AT.store(t + 5000, Ordering::Relaxed); // Zukunft → fällig
        assert!(claim_app_server(t));
        assert!(!claim_app_server(t + 1));
        assert!(claim_app_server(t + INTERVAL));
    }

    #[test]
    fn tail_drops_partial_first_line() {
        let dir = std::env::temp_dir().join(format!("lightbar-tail-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("rollout.jsonl");
        std::fs::write(&p, "aaaa\nbbbb\ncccc\n").unwrap();
        let whole = read_tail(&p, 1000).unwrap();
        let mid = read_tail(&p, 7).unwrap(); // "b\ncccc\n" → angeschnitten
        let edge = read_tail(&p, 10).unwrap(); // "bbbb\ncccc\n" → beginnt mit ganzer Zeile
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(whole, b"aaaa\nbbbb\ncccc\n");
        assert_eq!(mid, b"cccc\n");
        assert_eq!(edge, b"bbbb\ncccc\n");
    }

    #[test]
    fn missing_cli_is_its_own_kind() {
        // Nicht mehr an den übersetzten Text gebunden.
        assert_eq!(FailKind::NotFound.key(), Some("other"));
        assert_ne!(FailKind::NotFound, FailKind::Other);
    }

    #[test]
    fn finds_rollouts_recursively() {
        let dir = std::env::temp_dir().join(format!("lightbar-usage-test-{}", std::process::id()));
        let day = dir.join("sessions").join("2026").join("10").join("21");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-a.jsonl"), join(&[META, NEW_FORMAT])).unwrap();
        std::fs::write(day.join("andere.jsonl"), NEW_FORMAT).unwrap();
        let mut files = Vec::new();
        collect_rollouts(&dir.join("sessions"), 0, &mut files);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(files.len(), 1);
        assert!(files[0].2.ends_with("rollout-a.jsonl"));
        let mut none = Vec::new();
        collect_rollouts(&dir.join("fehlt"), 0, &mut none);
        assert!(none.is_empty());
    }

    #[test]
    fn dates_and_old_folders() {
        assert_eq!(ymd(0), (1970, 1, 1));
        assert_eq!(ymd(1_792_667_924), (2026, 10, 22));
        assert_eq!(ymd(951_782_400), (2000, 2, 29));
        let since = (2026, 10, 14);
        assert!(dated_dir_too_old(&[2025], since));
        assert!(!dated_dir_too_old(&[2026], since));
        assert!(dated_dir_too_old(&[2026, 9], since));
        assert!(!dated_dir_too_old(&[2026, 10], since));
        assert!(dated_dir_too_old(&[2026, 10, 13], since));
        assert!(!dated_dir_too_old(&[2026, 10, 14], since));
        assert!(!dated_dir_too_old(&[2026, 11, 1], since));
    }

    #[test]
    fn skips_old_day_folders() {
        let dir = std::env::temp_dir().join(format!("lightbar-usage-old-{}", std::process::id()));
        let old = dir.join("sessions").join("2001").join("01").join("02");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("rollout-alt.jsonl"), NEW_FORMAT).unwrap();
        let mut files = Vec::new();
        // Stichtag heute: der Ordner von 2001 wird nicht betreten, obwohl die
        // Datei gerade erst geschrieben wurde.
        collect_rollouts(&dir.join("sessions"), now(), &mut files);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(files.is_empty());
    }

    #[test]
    fn describe_shows_local_age() {
        let l = Limits { five_hour: Some(Window { used: 12.0, resets_at: None }), week: None };
        let text = describe("Codex", &Some(Ok(l)), now() - 3 * 3600, true, None);
        assert!(text.starts_with("Codex (letzte Codex-Sitzung vor 3 Std.)"), "{text}");
        assert!(text.contains("5 Stunden: 12 %"));
    }
}

#[cfg(test)]
mod app_server_tests {
    use super::*;

    // Gekürzte Antworten im Format von `account/rateLimits/read` (Codex-CLI 0.163).
    const RESULT: &str = r#"{"id":2,"result":{"ordinaryUsageAllowed":true,"accountId":null,"rateLimitResetCredits":{"availableCount":0,"credits":null},"rateLimits":{"limitId":"codex","limitName":null,"primary":{"usedPercent":7,"windowDurationMins":300,"resetsAt":1792667924},"secondary":{"usedPercent":41,"windowDurationMins":10080,"resetsAt":1793254724},"credits":null,"planType":"team"},"rateLimitsByLimitId":{"codex":{"limitId":"codex","primary":{"usedPercent":8,"windowDurationMins":300,"resetsAt":1792667924},"secondary":{"usedPercent":42,"windowDurationMins":10080,"resetsAt":1793254724}},"codex_spark":{"limitId":"codex_spark","primary":{"usedPercent":99,"windowDurationMins":300,"resetsAt":1792667924},"secondary":null}}}}"#;
    const ONLY_SINGLE: &str = r#"{"id":2,"result":{"rateLimits":{"limitId":null,"primary":{"usedPercent":12,"windowDurationMins":10080,"resetsAt":null},"secondary":null},"rateLimitsByLimitId":null}}"#;

    fn result_of(line: &str) -> Value {
        rpc_response(line, 2).expect("Antwort auf id 2").expect("kein Fehler")
    }

    #[test]
    fn prefers_codex_bucket() {
        let l = parse_app_server_limits(&result_of(RESULT), 1_792_000_000).unwrap();
        let (f, w) = (l.five_hour.unwrap(), l.week.unwrap());
        assert_eq!((f.used, f.resets_at), (8.0, Some(1792667924)));
        assert_eq!((w.used, w.resets_at), (42.0, Some(1793254724)));
    }

    #[test]
    fn falls_back_to_single_bucket_by_window_length() {
        let l = parse_app_server_limits(&result_of(ONLY_SINGLE), 1_792_000_000).unwrap();
        assert!(l.five_hour.is_none());
        assert_eq!(l.week.map(|w| (w.used, w.resets_at)), Some((12.0, None)));
    }

    #[test]
    fn no_windows_means_no_limits() {
        let r = result_of(r#"{"id":2,"result":{"rateLimits":{"limitId":"codex","primary":null,"secondary":null}}}"#);
        assert!(parse_app_server_limits(&r, 0).is_none());
    }

    #[test]
    fn skips_notifications_and_other_ids() {
        assert!(rpc_response(r#"{"method":"remoteControl/status/changed","params":{"status":"off"}}"#, 2).is_none());
        assert!(rpc_response(r#"{"id":1,"result":{"userAgent":"x"}}"#, 2).is_none());
        assert!(rpc_response("keine JSON-Zeile", 2).is_none());
    }

    #[test]
    fn server_errors_are_classified() {
        let e = rpc_response(r#"{"id":2,"error":{"code":-32600,"message":"codex account authentication required to read rate limits"}}"#, 2).unwrap().unwrap_err();
        assert_eq!(e.kind, FailKind::Auth);
        let e = rpc_response(r#"{"id":2,"error":{"code":-32601,"message":"method not found"}}"#, 2).unwrap().unwrap_err();
        assert_eq!(e.kind, FailKind::Other);
        assert!(e.message.contains("method not found"));
    }
}

/// Echte Abfrage über den App-Server: `cargo test live_app_server -- --ignored --nocapture`
/// (gibt nur Pfad und Prozentwerte aus).
#[cfg(test)]
#[test]
#[ignore]
fn live_app_server() {
    println!("CLI: {:?}", codex_cli());
    let started = std::time::Instant::now();
    match fetch_codex_app_server() {
        Ok(f) => println!("ok nach {:?}: 5h {:?} · Woche {:?}", started.elapsed(), f.limits.five_hour, f.limits.week),
        Err(e) => panic!("{}", e.message),
    }
}
