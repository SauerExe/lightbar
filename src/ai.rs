//! KI-Antworten im Startmenü: Fragen gehen an einen OpenAI-kompatiblen
//! Chat-Server (z. B. Ollama/vLLM, auch über einen Tunnel wie Ngino), die
//! Antwort kommt gestreamt und erscheint Stück für Stück.
//!
//! Eingerichtet wird es über `%APPDATA%\Lightbar\ai.txt` (`url=`, `key=`,
//! `model=`) oder `lightbar.exe --ai-setup <url> <key> [model]`. Ohne die
//! Datei ist die Funktion aus. Der Schlüssel steht nur dort – nie im Code, im
//! Protokoll oder in Fehlermeldungen.

use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::Networking::WinHttp::*;

use crate::util::w16;

const DEFAULT_MODEL: &str = "qwen3.8-flash-next-iq3_s";

#[derive(Clone)]
pub struct Config {
    pub url: String,
    key: String,
    pub model: String,
}

fn path() -> Option<std::path::PathBuf> {
    Some(crate::util::data_dir()?.join("ai.txt"))
}

/// Einstellungen, falls eingerichtet.
pub fn config() -> Option<Config> {
    let text = std::fs::read_to_string(path()?).ok()?;
    let mut c = Config { url: String::new(), key: String::new(), model: DEFAULT_MODEL.into() };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim().to_string();
        match k.trim() {
            "url" => c.url = v.trim_end_matches('/').to_string(),
            "key" => c.key = v,
            "model" if !v.is_empty() => c.model = v,
            _ => {}
        }
    }
    (!c.url.is_empty()).then_some(c)
}

/// Eingerichtet? Für Zeichnen und Suche – liest die Datei höchstens alle
/// paar Sekunden neu.
pub fn enabled() -> bool {
    static CACHE: Mutex<Option<(std::time::Instant, bool)>> = Mutex::new(None);
    let Ok(mut c) = CACHE.lock() else { return false };
    match *c {
        Some((t, on)) if t.elapsed() < std::time::Duration::from_secs(5) => on,
        _ => {
            let on = config().is_some();
            *c = Some((std::time::Instant::now(), on));
            on
        }
    }
}

/// `--ai-setup <url> <key> [model]`: Einstellungsdatei schreiben.
pub fn setup(url: &str, key: &str, model: Option<&str>) -> bool {
    let Some(p) = path() else { return false };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = format!("url={}\nkey={}\nmodel={}\n", url.trim_end_matches('/'), key, model.unwrap_or(DEFAULT_MODEL));
    std::fs::write(p, text).is_ok()
}

/// Stand einer Antwort.
#[derive(Clone, Default)]
pub struct Answer {
    pub question: String,
    pub text: String,
    /// Das Modell denkt noch (es kamen nur Denk-Abschnitte).
    pub thinking: bool,
    pub done: bool,
    pub error: Option<String>,
}

static CURRENT: Mutex<Option<(u64, Answer)>> = Mutex::new(None);
static GEN: AtomicU64 = AtomicU64::new(0);

/// Aktuelle Antwort (falls eine läuft oder fertig ist).
pub fn current() -> Option<Answer> {
    CURRENT.lock().ok()?.as_ref().map(|(_, a)| a.clone())
}

/// Laufende Antwort verwerfen (der Thread hört beim nächsten Stück auf).
pub fn cancel() {
    GEN.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut c) = CURRENT.lock() {
        *c = None;
    }
}

fn update(gen: u64, f: impl FnOnce(&mut Answer)) -> bool {
    let Ok(mut c) = CURRENT.lock() else { return false };
    match c.as_mut() {
        Some((g, a)) if *g == gen => {
            f(a);
            true
        }
        _ => false,
    }
}

/// Frage stellen. `notify` wird bei jedem neuen Stück aufgerufen (aus dem
/// Hintergrund-Thread – also nur eine Nachricht posten).
pub fn ask(question: String, notify: impl Fn() + Send + 'static) {
    let Some(cfg) = config() else { return };
    let gen = GEN.fetch_add(1, Ordering::Relaxed) + 1;
    if let Ok(mut c) = CURRENT.lock() {
        *c = Some((gen, Answer { question: question.clone(), thinking: true, ..Default::default() }));
    }
    notify();
    std::thread::spawn(move || {
        let result = unsafe { stream(&cfg, &question, gen, &notify) };
        update(gen, |a| {
            a.done = true;
            a.thinking = false;
            if let Err(e) = result {
                a.error = Some(e);
            }
        });
        notify();
    });
}

fn today() -> String {
    let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    format!("{:02}.{:02}.{}", t.wDay, t.wMonth, t.wYear)
}

struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// POST mit Streaming-Antwort (Server-Sent Events) lesen.
unsafe fn stream(cfg: &Config, question: &str, gen: u64, notify: &dyn Fn()) -> Result<(), String> {
    let (secure, rest) = match cfg.url.split_once("://") {
        Some(("http", r)) => (false, r),
        Some((_, r)) => (true, r),
        None => (true, cfg.url.as_str()),
    };
    let (hostport, prefix) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().unwrap_or(if secure { 443 } else { 80 })),
        None => (hostport, if secure { 443 } else { 80 }),
    };
    let path = format!("/{}{}v1/chat/completions", prefix, if prefix.is_empty() || prefix.ends_with('/') { "" } else { "/" });
    let body = serde_json::json!({
        "model": cfg.model,
        "stream": true,
        "messages": [
            { "role": "system", "content": format!("Du bist ein Assistent in der Windows-Taskleiste. Antworte knapp und hilfreich, in der Sprache der Frage. Heute ist der {}.", today()) },
            { "role": "user", "content": question },
        ],
    })
    .to_string();

    let session = Handle(WinHttpOpen(w16("Lightbar").as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0));
    if session.0.is_null() {
        return Err("WinHTTP nicht verfügbar".into());
    }
    // Erste Antwort kann dauern (Modell lädt), danach kommen Stücke laufend.
    WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 120_000);
    let conn = Handle(WinHttpConnect(session.0, w16(host).as_ptr(), port, 0));
    if conn.0.is_null() {
        return Err(format!("{host} nicht erreichbar"));
    }
    let req = Handle(WinHttpOpenRequest(conn.0, w16("POST").as_ptr(), w16(&path).as_ptr(), null(), null(), null(), if secure { WINHTTP_FLAG_SECURE } else { 0 }));
    if req.0.is_null() {
        return Err("Anfrage nicht möglich".into());
    }
    let headers = format!("Content-Type: application/json\r\nAccept: text/event-stream\r\n{}", if cfg.key.is_empty() { String::new() } else { format!("Authorization: Bearer {}\r\n", cfg.key) });
    let h = w16(&headers);
    if WinHttpSendRequest(req.0, h.as_ptr(), (h.len() - 1) as u32, body.as_ptr() as _, body.len() as u32, body.len() as u32, 0) == 0
        || WinHttpReceiveResponse(req.0, null_mut()) == 0
    {
        return Err(format!("{host} nicht erreichbar"));
    }
    let mut status: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    WinHttpQueryHeaders(req.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, null(), &mut status as *mut _ as _, &mut size, null_mut());
    if status != 200 {
        return Err(match status {
            401 | 403 => "Schlüssel abgelehnt".into(),
            502 | 503 | 504 => "Server erreicht, aber das Modell antwortet gerade nicht".into(),
            s => format!("Server antwortet mit HTTP {s}"),
        });
    }
    let mut pending: Vec<u8> = Vec::new();
    loop {
        if GEN.load(Ordering::Relaxed) != gen {
            return Ok(()); // Abgebrochen.
        }
        let mut avail = 0u32;
        if WinHttpQueryDataAvailable(req.0, &mut avail) == 0 {
            return Err("Verbindung abgebrochen".into());
        }
        if avail == 0 {
            return Ok(());
        }
        let mut buf = vec![0u8; avail as usize];
        let mut read = 0u32;
        if WinHttpReadData(req.0, buf.as_mut_ptr() as _, avail, &mut read) == 0 {
            return Err("Verbindung abgebrochen".into());
        }
        pending.extend_from_slice(&buf[..read as usize]);
        // Vollständige Zeilen verarbeiten ("data: {...}").
        while let Some(n) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=n).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                return Ok(());
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { continue };
            let delta = &v["choices"][0]["delta"];
            let piece = delta["content"].as_str().unwrap_or("");
            let reasoning = delta["reasoning_content"].as_str().is_some_and(|s| !s.is_empty());
            if !piece.is_empty() || reasoning {
                let changed = update(gen, |a| {
                    a.text.push_str(piece);
                    a.thinking = a.text.trim().is_empty();
                });
                if !changed {
                    return Ok(());
                }
                if !piece.is_empty() {
                    notify();
                }
            }
        }
    }
}
