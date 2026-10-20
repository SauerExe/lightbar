//! Claude- und Codex-Limits (5-Stunden-Fenster und Woche) für die Anzeige
//! unten links.
//!
//! Quelle sind dieselben Usage-Schnittstellen, die `/usage` in Claude Code und
//! Codex nutzen. Die Zugangsdaten kommen bei jeder Abfrage frisch aus den
//! lokalen Login-Dateien der beiden Tools (die erneuern ihre Tokens selbst);
//! sie werden nirgends gespeichert, geloggt oder an andere Hosts geschickt.
//! Die Schnittstellen sind nicht offiziell dokumentiert – ändert sich das
//! Format, zeigt die Leiste einen Fehler statt falscher Zahlen.

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

#[derive(Clone, Debug)]
pub struct Limits {
    pub five_hour: Option<Window>,
    pub week: Option<Window>,
}

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub claude: Option<Result<Limits, String>>,
    pub codex: Option<Result<Limits, String>>,
}

static CURRENT: Mutex<Option<Usage>> = Mutex::new(None);

pub fn current() -> Option<Usage> {
    CURRENT.lock().ok()?.clone()
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

/// HTTPS-GET über WinHTTP (Systemproxy, Systemzertifikate). Rückgabe: (Status, Body).
fn https_get(host: &str, path: &str, headers: &[(&str, String)]) -> Result<(u32, String), String> {
    unsafe {
        let session = Handle(WinHttpOpen(w16("lightbar/0.1").as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0));
        if session.0.is_null() {
            return Err("WinHTTP nicht verfügbar".into());
        }
        WinHttpSetTimeouts(session.0, 5000, 5000, 10000, 10000);
        let conn = Handle(WinHttpConnect(session.0, w16(host).as_ptr(), INTERNET_DEFAULT_HTTPS_PORT as u16, 0));
        if conn.0.is_null() {
            return Err(format!("Keine Verbindung zu {host}"));
        }
        let req = Handle(WinHttpOpenRequest(conn.0, w16("GET").as_ptr(), w16(path).as_ptr(), null(), null(), null(), WINHTTP_FLAG_SECURE));
        if req.0.is_null() {
            return Err("Anfrage nicht möglich".into());
        }
        let header_text: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        let h = w16(&header_text);
        if WinHttpSendRequest(req.0, h.as_ptr(), (h.len() - 1) as u32, null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(req.0, null_mut()) == 0
        {
            return Err(format!("{host} nicht erreichbar"));
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
        Ok((status, String::from_utf8_lossy(&body).into_owned()))
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
            let oh = off[1..3].parse::<i64>().ok()?;
            let om = off[4..6].parse::<i64>().ok()?;
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

fn http_error(service: &str, status: u32) -> String {
    match status {
        401 => format!("{service}: Login abgelaufen – {service} einmal starten (HTTP 401)"),
        403 => format!("{service}: Login hat keine Berechtigung für die Usage-Abfrage (HTTP 403)"),
        429 => format!("{service}: zu viele Abfragen, später erneut (HTTP 429)"),
        _ => format!("{service}: HTTP {status}"),
    }
}

// ------------------------------------------------------------- Abfragen

fn fetch_claude() -> Result<Limits, String> {
    let path = home().ok_or("kein Benutzerordner")?.join(".claude").join(".credentials.json");
    let text = std::fs::read_to_string(path).map_err(|_| "Claude: nicht angemeldet".to_string())?;
    let creds: Value = serde_json::from_str(&text).map_err(|_| "Claude: Login-Datei unlesbar".to_string())?;
    let token = creds
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .ok_or("Claude: kein Abo-Login (API-Key-Nutzung hat keine Limits)")?;
    let (status, body) = https_get(
        "api.anthropic.com",
        "/api/oauth/usage",
        &[("Authorization", format!("Bearer {token}")), ("anthropic-beta", "oauth-2025-04-20".into()), ("Accept", "application/json".into())],
    )?;
    if status != 200 {
        return Err(http_error("Claude", status));
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| "Claude: Antwort unlesbar".to_string())?;
    let limits = Limits {
        five_hour: v.get("five_hour").and_then(claude_window),
        week: v.get("seven_day").and_then(claude_window),
    };
    if limits.five_hour.is_none() && limits.week.is_none() {
        return Err("Claude: Antwortformat unbekannt".into());
    }
    Ok(limits)
}

fn fetch_codex() -> Result<Limits, String> {
    let path = home().ok_or("kein Benutzerordner")?.join(".codex").join("auth.json");
    let text = std::fs::read_to_string(path).map_err(|_| "Codex: nicht angemeldet".to_string())?;
    let auth: Value = serde_json::from_str(&text).map_err(|_| "Codex: Login-Datei unlesbar".to_string())?;
    let token = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .ok_or("Codex: kein ChatGPT-Login (API-Key-Nutzung hat keine Limits)")?;
    let mut headers = vec![("Authorization", format!("Bearer {token}")), ("Accept", "application/json".into())];
    if let Some(account) = auth.pointer("/tokens/account_id").and_then(Value::as_str) {
        headers.push(("ChatGPT-Account-Id", account.to_string()));
    }
    let fetched = now();
    let (status, body) = https_get("chatgpt.com", "/backend-api/wham/usage", &headers)?;
    if status != 200 {
        return Err(http_error("Codex", status));
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| "Codex: Antwort unlesbar".to_string())?;
    let rl = v.get("rate_limit").ok_or("Codex: Antwortformat unbekannt")?;
    let limits = Limits {
        five_hour: rl.get("primary_window").and_then(|w| codex_window(w, fetched)),
        week: rl.get("secondary_window").and_then(|w| codex_window(w, fetched)),
    };
    if limits.five_hour.is_none() && limits.week.is_none() {
        return Err("Codex: Antwortformat unbekannt".into());
    }
    Ok(limits)
}

/// Beide Dienste abfragen (blockiert, nur vom Hintergrund-Thread aufrufen).
pub fn fetch() -> Usage {
    Usage { claude: Some(fetch_claude()), codex: Some(fetch_codex()) }
}

/// Neues Ergebnis übernehmen. Schlägt eine Abfrage nur vorübergehend fehl
/// (Netz weg, 429), bleibt der letzte gute Wert stehen.
fn store(new: Usage) {
    let Ok(mut cur) = CURRENT.lock() else { return };
    let old = cur.take().unwrap_or_default();
    let keep = |n: Option<Result<Limits, String>>, o: Option<Result<Limits, String>>| match (n, o) {
        (Some(Err(e)), Some(Ok(prev))) if !e.contains("401") && !e.contains("403") => Some(Ok(prev)),
        (n, _) => n,
    };
    *cur = Some(Usage { claude: keep(new.claude, old.claude), codex: keep(new.codex, old.codex) });
}

/// Hintergrund-Thread: sofort, dann alle 5 Minuten; meldet sich per
/// `notify` (PostMessage an die Leiste) nach jeder Abfrage.
pub fn spawn(notify: impl Fn() + Send + 'static) {
    std::thread::spawn(move || loop {
        store(fetch());
        notify();
        std::thread::park_timeout(Duration::from_secs(300));
    });
}

/// Sofort neu abfragen (Klick auf die Anzeige).
pub fn refresh_now(notify: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        store(fetch());
        notify();
    });
}

// --------------------------------------------------------------- Anzeige

/// "in 2 Std. 10 Min." / "in 3 Tagen"
pub fn until(t: i64) -> String {
    let s = (t - now()).max(0);
    match s {
        0..=59 => "gleich".into(),
        60..=3599 => format!("in {} Min.", s / 60),
        3600..=86399 => format!("in {} Std. {} Min.", s / 3600, (s % 3600) / 60),
        _ => format!("in {} Tagen", (s + 43200) / 86400),
    }
}

pub fn describe(name: &str, r: &Option<Result<Limits, String>>) -> String {
    match r {
        None => format!("{name}: wird abgefragt …"),
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
            format!("{name}{}{}", line("5 Stunden", &l.five_hour), line("Woche", &l.week))
        }
    }
}

/// `--usage-test`: einmal abfragen und nur Prozentwerte/Fehler in eine Datei
/// schreiben – zum Prüfen ohne Tokens anzuzeigen.
pub fn self_test() {
    let u = fetch();
    let text = format!("{}\n\n{}\n", describe("Claude", &u.claude), describe("Codex", &u.codex));
    if let Ok(appdata) = std::env::var("APPDATA") {
        let dir = PathBuf::from(appdata).join("Lightbar");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("usage-test.txt"), text);
    }
}
