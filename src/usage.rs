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
    /// Zeitpunkt des angezeigten Werts (für "Stand vor …").
    pub claude_at: i64,
    pub codex_at: i64,
    /// Nächster geplanter Versuch, falls gerade pausiert (429), sonst 0.
    pub claude_paused_until: i64,
    pub codex_paused_until: i64,
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
        let via = proxy.as_ref().map(|p| format!(" (über Proxy {})", p.server)).unwrap_or_default();
        let header_text: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        let h = w16(&header_text);
        if WinHttpSendRequest(req.0, h.as_ptr(), (h.len() - 1) as u32, null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(req.0, null_mut()) == 0
        {
            return Err(format!("{host} nicht erreichbar{via}"));
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

/// Warum eine Abfrage scheiterte – bestimmt, wann es erneut versucht wird.
#[derive(Clone, Debug)]
pub struct Failure {
    pub message: String,
    /// HTTP 429: Pause einlegen (ggf. so lange, wie der Server verlangt).
    rate_limited: Option<Option<i64>>,
    /// Login fehlt/abgelaufen: alte Werte nicht weiter anzeigen.
    auth: bool,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure { message, rate_limited: None, auth: false }
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

fn http_failure(service: &str, status: u32, retry_after: Option<i64>) -> Failure {
    Failure {
        message: http_error(service, status),
        rate_limited: (status == 429).then_some(retry_after),
        auth: status == 401 || status == 403,
    }
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

fn fetch_claude() -> Result<Limits, Failure> {
    let path = home().ok_or("kein Benutzerordner")?.join(".claude").join(".credentials.json");
    let text = std::fs::read_to_string(path).map_err(|_| Failure { auth: true, ..Failure::from("Claude: nicht angemeldet") })?;
    let creds: Value = serde_json::from_str(&text).map_err(|_| Failure::from("Claude: Login-Datei unlesbar"))?;
    let token = creds
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .ok_or("Claude: kein Abo-Login (API-Key-Nutzung hat keine Limits)")?;
    let (status, body, retry) = https_get(
        "api.anthropic.com",
        "/api/oauth/usage",
        &[("Authorization", format!("Bearer {token}")), ("anthropic-beta", "oauth-2025-04-20".into()), ("Accept", "application/json".into())],
    )?;
    if status != 200 {
        return Err(http_failure("Claude", status, retry));
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| Failure::from("Claude: Antwort unlesbar"))?;
    let limits = Limits {
        five_hour: v.get("five_hour").and_then(claude_window),
        week: v.get("seven_day").and_then(claude_window),
    };
    if limits.five_hour.is_none() && limits.week.is_none() {
        return Err("Claude: Antwortformat unbekannt".into());
    }
    Ok(limits)
}

fn fetch_codex() -> Result<Limits, Failure> {
    let path = home().ok_or("kein Benutzerordner")?.join(".codex").join("auth.json");
    let text = std::fs::read_to_string(path).map_err(|_| Failure { auth: true, ..Failure::from("Codex: nicht angemeldet") })?;
    let auth: Value = serde_json::from_str(&text).map_err(|_| Failure::from("Codex: Login-Datei unlesbar"))?;
    let token = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .ok_or("Codex: kein ChatGPT-Login (API-Key-Nutzung hat keine Limits)")?;
    let mut headers = vec![("Authorization", format!("Bearer {token}")), ("Accept", "application/json".into())];
    if let Some(account) = auth.pointer("/tokens/account_id").and_then(Value::as_str) {
        headers.push(("ChatGPT-Account-Id", account.to_string()));
    }
    let fetched = now();
    let (status, body, retry) = https_get("chatgpt.com", "/backend-api/wham/usage", &headers)?;
    if status != 200 {
        return Err(http_failure("Codex", status, retry));
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| Failure::from("Codex: Antwort unlesbar"))?;
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

// --------------------------------------------------------- Zeitplanung
//
// Rücksichtsvoll abfragen: je Dienst höchstens alle 10 Minuten, bei HTTP 429
// Pause (Retry-After des Servers, sonst 10 → 20 → 40 → 60 Minuten). Werte und
// Zeitplan liegen in `usage-cache.json`, damit ein Neustart nicht sofort neu
// fragt, sondern die letzten Werte zeigt.

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
}

/// Index 0 = Claude, 1 = Codex.
static SLOTS: Mutex<[Slot; 2]> = Mutex::new([
    Slot { value: None, fetched_at: 0, next_at: 0, backoff: 0 },
    Slot { value: None, fetched_at: 0, next_at: 0, backoff: 0 },
]);

fn fetch_slot(i: usize) -> Result<Limits, Failure> {
    if i == 0 {
        fetch_claude()
    } else {
        fetch_codex()
    }
}

/// Ergebnis einer Abfrage in den Zeitplan einarbeiten.
fn apply(slot: &mut Slot, result: Result<Limits, Failure>, t: i64) {
    match result {
        Ok(l) => {
            slot.value = Some(Ok(l));
            slot.fetched_at = t;
            slot.backoff = 0;
            slot.next_at = t + INTERVAL;
        }
        Err(f) => {
            let wait = match f.rate_limited {
                Some(retry) => {
                    slot.backoff = retry.unwrap_or(if slot.backoff == 0 { INTERVAL } else { slot.backoff * 2 }).clamp(INTERVAL, MAX_BACKOFF);
                    slot.backoff
                }
                None => INTERVAL / 2,
            };
            slot.next_at = t + wait;
            // Letzten guten Wert behalten – außer der Login ist weg.
            let keep = matches!(slot.value, Some(Ok(_))) && !f.auth;
            if !keep {
                let msg = if f.rate_limited.is_some() {
                    format!("{} – nächster Versuch {}", f.message, until(slot.next_at))
                } else {
                    f.message
                };
                slot.value = Some(Err(msg));
            }
        }
    }
}

/// Fällige Dienste abfragen. `manual`: auf Klick, aber nicht öfter als
/// einmal pro Minute und nie während einer 429-Pause.
fn run_due(manual: bool) -> bool {
    // Ausgeschaltet: keine Login-Dateien lesen, nichts abfragen.
    if !crate::settings::get().show_usage {
        return false;
    }
    let t = now();
    let due: Vec<usize> = match SLOTS.lock() {
        Ok(s) => (0..2)
            .filter(|&i| {
                let sl = &s[i];
                if manual {
                    sl.backoff == 0 && t - sl.fetched_at >= MIN_MANUAL
                } else {
                    t >= sl.next_at
                }
            })
            .collect(),
        Err(_) => return false,
    };
    if due.is_empty() {
        return false;
    }
    // Netzwerk außerhalb der Sperre.
    let results: Vec<(usize, Result<Limits, Failure>)> = due.into_iter().map(|i| (i, fetch_slot(i))).collect();
    if let Ok(mut s) = SLOTS.lock() {
        let t = now();
        for (i, r) in results {
            apply(&mut s[i], r, t);
        }
        save_cache(&s);
    }
    true
}

pub fn current() -> Option<Usage> {
    let s = SLOTS.lock().ok()?;
    Some(Usage {
        claude: s[0].value.clone(),
        codex: s[1].value.clone(),
        claude_at: s[0].fetched_at,
        codex_at: s[1].fetched_at,
        claude_paused_until: if s[0].backoff > 0 { s[0].next_at } else { 0 },
        codex_paused_until: if s[1].backoff > 0 { s[1].next_at } else { 0 },
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

fn save_cache(slots: &[Slot; 2]) {
    let entry = |s: &Slot| {
        let (five, week, error) = match &s.value {
            Some(Ok(l)) => (window_json(&l.five_hour), window_json(&l.week), Value::Null),
            Some(Err(e)) => (Value::Null, Value::Null, Value::from(e.as_str())),
            None => (Value::Null, Value::Null, Value::Null),
        };
        serde_json::json!({ "fetched_at": s.fetched_at, "next_at": s.next_at, "backoff": s.backoff, "five_hour": five, "week": week, "error": error })
    };
    let doc = serde_json::json!({ "claude": entry(&slots[0]), "codex": entry(&slots[1]) });
    if let Some(p) = cache_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, doc.to_string());
    }
}

fn load_cache() {
    let Some(text) = cache_path().and_then(|p| std::fs::read_to_string(p).ok()) else { return };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else { return };
    let Ok(mut s) = SLOTS.lock() else { return };
    for (i, key) in ["claude", "codex"].into_iter().enumerate() {
        let Some(e) = doc.get(key) else { continue };
        let num = |k: &str| e.get(k).and_then(Value::as_i64).unwrap_or(0);
        let limits = Limits { five_hour: e.get("five_hour").and_then(window_from), week: e.get("week").and_then(window_from) };
        let fetched_at = num("fetched_at");
        if limits.five_hour.is_some() || limits.week.is_some() {
            s[i].value = Some(Ok(limits));
            s[i].fetched_at = fetched_at;
        } else if let Some(err) = e.get("error").and_then(Value::as_str) {
            s[i].value = Some(Err(err.to_string()));
        }
        s[i].backoff = num("backoff");
        // Nach einem Neustart frühestens zum geplanten Zeitpunkt wieder fragen.
        s[i].next_at = num("next_at").max(fetched_at + INTERVAL);
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
        0..=59 => "gleich".into(),
        60..=3599 => format!("in {} Min.", s / 60),
        3600..=86399 => format!("in {} Std. {} Min.", s / 3600, (s % 3600) / 60),
        _ => format!("in {} Tagen", (s + 43200) / 86400),
    }
}

pub fn describe(name: &str, r: &Option<Result<Limits, String>>, fetched_at: i64) -> String {
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
            let age = now() - fetched_at;
            let stand = if fetched_at > 0 && age >= 120 { format!(" (Stand vor {} Min.)", age / 60) } else { String::new() };
            format!("{name}{stand}{}{}", line("5 Stunden", &l.five_hour), line("Woche", &l.week))
        }
    }
}

/// `--usage-test`: einmal abfragen und nur Prozentwerte/Fehler in eine Datei
/// schreiben – zum Prüfen ohne Tokens anzuzeigen.
pub fn self_test() {
    let d = |r: Result<Limits, Failure>| Some(r.map_err(|f| f.message));
    let text = format!("{}\n\n{}\n", describe("Claude", &d(fetch_claude()), now()), describe("Codex", &d(fetch_codex()), now()));
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
