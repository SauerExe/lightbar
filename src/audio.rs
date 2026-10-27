//! Audiogeräte: aktive Ausgabe- und Eingabegeräte auflisten und das
//! Standardgerät umschalten (wie „Tonausgabe auswählen“ in den Windows-11-
//! Schnelleinstellungen, EarTrumpet, SoundSwitch).
//!
//! Auflisten geht über die offizielle MMDevice-API. Zum Umschalten gibt es
//! keine öffentliche Schnittstelle; alle bekannten Programme nutzen das
//! undokumentierte `IPolicyConfig` der Klasse `PolicyConfigClient` – hier
//! genauso, mit von Hand gebauter Vtable (siehe `PolicyConfigVtbl`).
//!
//! Lautstärke und Stummschaltung bleiben in `status.rs`; die Leiste erfährt
//! Gerätewechsel über dessen `IMMNotificationClient`.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{Interface, IUnknown, GUID, HRESULT, PCWSTR};
use windows::Win32::Media::Audio::{eCapture, eCommunications, eConsole, eMultimedia, eRender, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE};
use windows::Win32::System::Com::StructuredStorage::{PropVariantToStringAlloc, PropVariantToUInt32};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    Output,
    Input,
}

impl Flow {
    fn data_flow(self) -> EDataFlow {
        match self {
            Flow::Output => eRender,
            Flow::Input => eCapture,
        }
    }
}

/// Art des Geräts (fürs Symbol), aus PKEY_AudioEndpoint_FormFactor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Speakers,
    Headphones,
    Headset,
    /// HDMI/DisplayPort (Monitor, Fernseher).
    Display,
    Microphone,
    /// Line, S/PDIF, Netzwerk, unbekannt.
    Other,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Device {
    /// Endpunkt-ID (für SetDefaultEndpoint).
    pub id: String,
    pub name: String,
    pub kind: Kind,
    /// Standardgerät (Rolle eConsole).
    pub default: bool,
}

// ------------------------------------------------------- reine Helfer

/// Windows-Enum `EndpointFormFactor` → Kind.
pub fn kind_for(form_factor: u32, flow: Flow) -> Kind {
    match form_factor {
        1 => Kind::Speakers,           // Speakers
        3 => Kind::Headphones,         // Headphones
        5 | 6 => Kind::Headset,        // Headset, Handset
        4 => Kind::Microphone,         // Microphone
        9 => Kind::Display,            // DigitalAudioDisplayDevice (HDMI/DP)
        _ if flow == Flow::Input => Kind::Microphone,
        _ => Kind::Other,              // RemoteNetworkDevice, LineLevel, SPDIF, unbekannt
    }
}

/// Symbol aus Segoe Fluent Icons.
pub fn glyph(kind: Kind) -> &'static str {
    match kind {
        Kind::Speakers | Kind::Other => "\u{E7F5}",
        Kind::Headphones => "\u{E7F6}",
        Kind::Headset => "\u{E95B}",
        Kind::Display => "\u{E7F4}",
        Kind::Microphone => "\u{E720}",
    }
}

/// Gerät zu einem (vom Nutzer oder der KI genannten) Namen: genauer Treffer vor
/// Anfang vor Teilwort vor allen Wörtern; sonst Gerätearten wie „Kopfhörer“,
/// „HDMI“, „Lautsprecher“. Bei Gleichstand gewinnt das erste Gerät.
pub fn find<'a>(devices: &'a [Device], query: &str) -> Option<&'a Device> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    let words: Vec<&str> = q.split_whitespace().collect();
    let kind_hint = match q.as_str() {
        s if ["kopfhörer", "kopfhoerer", "headphone", "headphones", "earbuds"].iter().any(|k| s.contains(k)) => Some(&[Kind::Headphones, Kind::Headset][..]),
        s if ["headset"].iter().any(|k| s.contains(k)) => Some(&[Kind::Headset, Kind::Headphones][..]),
        s if ["hdmi", "displayport", "monitor", "bildschirm", "fernseher", "tv"].iter().any(|k| s.contains(k)) => Some(&[Kind::Display][..]),
        s if ["lautsprecher", "boxen", "speaker"].iter().any(|k| s.contains(k)) => Some(&[Kind::Speakers][..]),
        s if ["mikrofon", "microphone", "mic"].iter().any(|k| s.contains(k)) => Some(&[Kind::Microphone, Kind::Headset][..]),
        _ => None,
    };
    let mut best: Option<(u32, &Device)> = None;
    for d in devices {
        let n = d.name.to_lowercase();
        let mut score = if n == q {
            100
        } else if n.starts_with(&q) {
            60
        } else if n.contains(&q) {
            40
        } else if words.iter().all(|w| n.contains(w)) {
            20
        } else {
            0
        };
        if score == 0 {
            if let Some(kinds) = kind_hint {
                if let Some(pos) = kinds.iter().position(|k| *k == d.kind) {
                    score = 10 - pos as u32;
                }
            }
        }
        if score > 0 && best.is_none_or(|(b, _)| score > b) {
            best = Some((score, d));
        }
    }
    best.map(|(_, d)| d)
}

// ------------------------------------------------------------ Auflisten

/// PKEY_Device_FriendlyName ({a45c254e-df1c-4efd-8020-67d146a850e0}, 14),
/// z. B. „Lautsprecher (Realtek(R) Audio)“.
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY { fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0), pid: 14 };
/// PKEY_Device_DeviceDesc (gleiche fmtid, 2) – Ersatz, falls der Name fehlt.
const PKEY_DEVICE_DESC: PROPERTYKEY = PROPERTYKEY { fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0), pid: 2 };
/// PKEY_AudioEndpoint_FormFactor ({1da5d803-d492-4edd-8c23-e0c0ffee7f0e}, 0), VT_UI4.
const PKEY_AUDIO_ENDPOINT_FORM_FACTOR: PROPERTYKEY = PROPERTYKEY { fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e), pid: 0 };

unsafe fn enumerator() -> Option<IMMDeviceEnumerator> {
    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()
}

unsafe fn device_id(d: &IMMDevice) -> Option<String> {
    let p = d.GetId().ok()?;
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const c_void));
    s
}

unsafe fn describe(d: &IMMDevice, flow: Flow, default_id: Option<&str>) -> Option<Device> {
    let id = device_id(d)?;
    let store = d.OpenPropertyStore(STGM_READ).ok()?;
    let text = |key: &PROPERTYKEY| -> Option<String> {
        let v = store.GetValue(key).ok()?;
        let p = PropVariantToStringAlloc(&v).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s.filter(|s| !s.trim().is_empty())
    };
    let name = text(&PKEY_DEVICE_FRIENDLY_NAME).or_else(|| text(&PKEY_DEVICE_DESC)).unwrap_or_else(|| t!("Unbekanntes Gerät", "Unknown device").to_string());
    let ff = store.GetValue(&PKEY_AUDIO_ENDPOINT_FORM_FACTOR).ok().and_then(|v| PropVariantToUInt32(&v).ok()).unwrap_or(10);
    let default = default_id == Some(id.as_str());
    Some(Device { id, name, kind: kind_for(ff, flow), default })
}

/// Aktive Geräte einer Richtung, Standardgerät markiert. Braucht COM im Thread.
pub fn list(flow: Flow) -> Vec<Device> {
    unsafe {
        let Some(en) = enumerator() else { return Vec::new() };
        let default_id = en.GetDefaultAudioEndpoint(flow.data_flow(), eConsole).ok().and_then(|d| device_id(&d));
        let Ok(coll) = en.EnumAudioEndpoints(flow.data_flow(), DEVICE_STATE_ACTIVE) else { return Vec::new() };
        let n = coll.GetCount().unwrap_or(0);
        (0..n).filter_map(|i| coll.Item(i).ok()).filter_map(|d| describe(&d, flow, default_id.as_deref())).collect()
    }
}

/// Aktuelles Standardgerät einer Richtung.
pub fn default_device(flow: Flow) -> Option<Device> {
    unsafe {
        let en = enumerator()?;
        let d = en.GetDefaultAudioEndpoint(flow.data_flow(), eConsole).ok()?;
        let id = device_id(&d)?;
        describe(&d, flow, Some(&id))
    }
}

// ----------------------------------------------------------- Umschalten

/// CLSID_PolicyConfigClient.
const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
/// IID_IPolicyConfig (Windows 7 bis 11; die Vista-Variante hat eine andere IID).
const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

type Slot = unsafe extern "system" fn();

/// Vtable von IPolicyConfig in der Reihenfolge der Deklaration (so auch in
/// EarTrumpet, SoundSwitch, AudioDeviceCmdlets):
///
/// | Slot | Methode               |
/// |------|-----------------------|
/// |  0–2 | IUnknown (QueryInterface, AddRef, Release) |
/// |    3 | GetMixFormat          |
/// |    4 | GetDeviceFormat       |
/// |    5 | ResetDeviceFormat     |
/// |    6 | SetDeviceFormat       |
/// |    7 | GetProcessingPeriod   |
/// |    8 | SetProcessingPeriod   |
/// |    9 | GetShareMode          |
/// |   10 | SetShareMode          |
/// |   11 | GetPropertyValue      |
/// |   12 | SetPropertyValue      |
/// |   13 | **SetDefaultEndpoint**(PCWSTR deviceId, ERole role) |
/// |   14 | SetEndpointVisibility |
///
/// Genutzt wird nur Slot 13; die übrigen stehen als Platzhalter da, damit der
/// Versatz stimmt (per Kompilierzeit-Prüfung unten abgesichert).
#[repr(C)]
struct PolicyConfigVtbl {
    query_interface: Slot,
    add_ref: Slot,
    release: Slot,
    get_mix_format: Slot,
    get_device_format: Slot,
    reset_device_format: Slot,
    set_device_format: Slot,
    get_processing_period: Slot,
    set_processing_period: Slot,
    get_share_mode: Slot,
    set_share_mode: Slot,
    get_property_value: Slot,
    set_property_value: Slot,
    set_default_endpoint: unsafe extern "system" fn(this: *mut c_void, device_id: PCWSTR, role: ERole) -> HRESULT,
    set_endpoint_visibility: Slot,
}

const _: () = assert!(std::mem::offset_of!(PolicyConfigVtbl, set_default_endpoint) == 13 * std::mem::size_of::<usize>());

/// Testlauf: `set_default` holt IPolicyConfig, ruft SetDefaultEndpoint aber
/// nicht auf (für Tests und `--ai-test`).
static DRY_RUN: AtomicBool = AtomicBool::new(false);

pub fn set_dry_run(on: bool) {
    DRY_RUN.store(on, Ordering::Relaxed);
}

/// Gerät als Standard für alle drei Rollen setzen (Konsole, Multimedia,
/// Kommunikation) – wie die Windows-Schnelleinstellungen. Braucht COM im Thread.
pub fn set_default(id: &str) -> Result<(), String> {
    unsafe {
        let unk: IUnknown = CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL).map_err(|e| format!("PolicyConfigClient: {e}"))?;
        let mut raw: *mut c_void = null_mut();
        unk.query(&IID_POLICY_CONFIG, &mut raw).ok().map_err(|e| format!("IPolicyConfig: {e}"))?;
        if raw.is_null() {
            return Err("IPolicyConfig: null".into());
        }
        // Gibt die per QueryInterface erhaltene Referenz am Ende frei.
        let owned = IUnknown::from_raw(raw);
        if DRY_RUN.load(Ordering::Relaxed) {
            drop(owned);
            return Ok(());
        }
        let vtbl = &**(raw as *mut *const PolicyConfigVtbl);
        let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        let mut result = Ok(());
        for role in [eConsole, eMultimedia, eCommunications] {
            let hr = (vtbl.set_default_endpoint)(raw, PCWSTR(wide.as_ptr()), role);
            if hr.is_err() {
                result = Err(format!("SetDefaultEndpoint: 0x{:08X}", hr.0 as u32));
            }
        }
        drop(owned);
        // status.rs soll das neue Gerät sofort öffnen, nicht erst auf die Rückmeldung warten.
        crate::status::default_changed();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, kind: Kind) -> Device {
        Device { id: name.into(), name: name.into(), kind, default: false }
    }

    #[test]
    fn form_factor_icons() {
        assert_eq!(kind_for(1, Flow::Output), Kind::Speakers);
        assert_eq!(kind_for(3, Flow::Output), Kind::Headphones);
        assert_eq!(kind_for(5, Flow::Output), Kind::Headset);
        assert_eq!(kind_for(9, Flow::Output), Kind::Display);
        assert_eq!(kind_for(10, Flow::Output), Kind::Other);
        assert_eq!(kind_for(2, Flow::Input), Kind::Microphone);
        assert_eq!(kind_for(5, Flow::Input), Kind::Headset);
        assert_ne!(glyph(Kind::Headphones), glyph(Kind::Speakers));
        assert_ne!(glyph(Kind::Display), glyph(Kind::Speakers));
        assert_eq!(glyph(Kind::Other), glyph(Kind::Speakers));
    }

    #[test]
    fn name_matching() {
        let d = [
            dev("Lautsprecher (Realtek(R) Audio)", Kind::Speakers),
            dev("Kopfhörer (WH-1000XM4)", Kind::Headphones),
            dev("LG ULTRAGEAR (NVIDIA High Definition Audio)", Kind::Display),
            dev("Realtek Digital Output (Realtek(R) Audio)", Kind::Other),
        ];
        let name = |q: &str| find(&d, q).map(|d| d.name.as_str());
        assert_eq!(name("Lautsprecher (Realtek(R) Audio)"), Some(d[0].name.as_str()));
        assert_eq!(name("  lautsprecher "), Some(d[0].name.as_str()));
        assert_eq!(name("WH-1000"), Some(d[1].name.as_str()));
        assert_eq!(name("ultragear nvidia"), Some(d[2].name.as_str()));
        assert_eq!(name("digital"), Some(d[3].name.as_str()));
        // Nur Geräteart genannt.
        assert_eq!(name("headphones"), Some(d[1].name.as_str()));
        assert_eq!(name("HDMI"), Some(d[2].name.as_str()));
        assert_eq!(name("speakers"), Some(d[0].name.as_str()));
        assert_eq!(name("xyzzy"), None);
        assert_eq!(name(""), None);
        // Genauer Treffer schlägt Teilwort.
        let e = [dev("Realtek Audio 2", Kind::Speakers), dev("Realtek Audio", Kind::Speakers)];
        assert_eq!(find(&e, "realtek audio").map(|d| d.name.as_str()), Some("Realtek Audio"));
    }

    #[test]
    fn vtable_slot() {
        assert_eq!(std::mem::offset_of!(PolicyConfigVtbl, set_default_endpoint) / std::mem::size_of::<usize>(), 13);
        assert_eq!(std::mem::size_of::<PolicyConfigVtbl>() / std::mem::size_of::<usize>(), 15);
    }

    /// Echte Geräte dieses Rechners, nur lesend; der Umschalter läuft als
    /// Testlauf (holt IPolicyConfig, setzt nichts):
    /// `cargo test audio_devices_here -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn audio_devices_here() {
        let _com = crate::util::ComGuard::sta();
        for flow in [Flow::Output, Flow::Input] {
            for d in list(flow) {
                println!("{flow:?} {:?} {}{}", d.kind, d.name, if d.default { " [Standard]" } else { "" });
            }
        }
        set_dry_run(true);
        if let Some(d) = default_device(Flow::Output) {
            assert_eq!(set_default(&d.id), Ok(()));
        }
    }
}
