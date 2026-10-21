//! Systemstatus für die rechte Seite: Netzwerk, Lautstärke, Akku.
//!
//! Ereignisgesteuert: Windows meldet Lautstärke- und Geräteänderungen (Audio-
//! Callbacks) und Netzwerkwechsel (IP-Helper); die Leiste fragt dann neu ab.
//! Akku kommt per WM_POWERBROADCAST direkt an die Leiste. Ein seltener
//! Sicherheits-Abruf fängt den Rest ab (z. B. WLAN-Signalstärke).

use std::cell::RefCell;
use std::mem::zeroed;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::core::{implement, PCWSTR};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl};
use windows::Win32::Media::Audio::{EDataFlow, ERole, IMMNotificationClient, IMMNotificationClient_Impl, AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::NetworkManagement::WiFi::*;
use windows_sys::Win32::Networking::WinInet::InternetGetConnectedState;
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

#[derive(Clone, PartialEq)]
pub enum Net {
    /// Signalqualität 0–100 und SSID.
    Wifi(u32, String),
    Wired,
    Offline,
}

#[derive(Clone, PartialEq)]
pub struct Status {
    pub net: Net,
    pub volume: f32,
    pub muted: bool,
    /// Prozent und ob geladen wird; None auf Rechnern ohne Akku.
    pub battery: Option<(u8, bool)>,
}

struct Handles {
    wlan: HANDLE,
    audio: Option<IMMDeviceEnumerator>,
    /// Offenes Standard-Audiogerät; alle 10 s neu ermittelt (Kopfhörer-Wechsel).
    endpoint: Option<(IAudioEndpointVolume, Instant)>,
    /// Letztes WLAN-Ergebnis; WLAN ändert sich selten, Abfrage alle 5 s.
    wifi: Option<(Option<(u32, String)>, Instant)>,
    /// Rückmeldungen von Windows (nur gesetzt, wenn watch lief).
    volume_watch: Option<IAudioEndpointVolumeCallback>,
    device_watch: Option<IMMNotificationClient>,
}

thread_local! {
    static HANDLES: RefCell<Handles> = const { RefCell::new(Handles { wlan: null_mut(), audio: None, endpoint: None, wifi: None, volume_watch: None, device_watch: None }) };
}

unsafe fn wifi(h: &mut Handles) -> Option<(u32, String)> {
    if h.wlan.is_null() {
        let mut ver = 0u32;
        if WlanOpenHandle(2, null_mut(), &mut ver, &mut h.wlan) != 0 {
            h.wlan = null_mut();
            return None;
        }
    }
    let mut list: *mut WLAN_INTERFACE_INFO_LIST = null_mut();
    if WlanEnumInterfaces(h.wlan, null_mut(), &mut list) != 0 || list.is_null() {
        return None;
    }
    let count = (*list).dwNumberOfItems as usize;
    let infos = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), count);
    let mut result = None;
    for info in infos {
        if info.isState != wlan_interface_state_connected {
            continue;
        }
        let mut size = 0u32;
        let mut data: *mut core::ffi::c_void = null_mut();
        let ok = WlanQueryInterface(
            h.wlan,
            &info.InterfaceGuid,
            wlan_intf_opcode_current_connection,
            null_mut(),
            &mut size,
            &mut data,
            null_mut(),
        );
        if ok == 0 && !data.is_null() {
            let attr = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
            let ssid = &attr.wlanAssociationAttributes.dot11Ssid;
            let name = String::from_utf8_lossy(&ssid.ucSSID[..ssid.uSSIDLength.min(32) as usize]).into_owned();
            result = Some((attr.wlanAssociationAttributes.wlanSignalQuality, name));
            WlanFreeMemory(data);
            break;
        }
    }
    WlanFreeMemory(list as _);
    result
}

unsafe fn endpoint(h: &mut Handles) -> Option<IAudioEndpointVolume> {
    // Mit Rückmeldungen bleibt das Gerät offen, bis Windows einen Wechsel meldet;
    // ohne sie wird es spätestens nach 10 s neu ermittelt.
    let changed = DEVICE_CHANGED.swap(false, Ordering::Relaxed);
    if let Some((ep, at)) = &h.endpoint {
        if !changed && (h.volume_watch.is_some() || at.elapsed() < Duration::from_secs(10)) {
            return Some(ep.clone());
        }
    }
    let Some(ep) = open_endpoint(h) else {
        // Beim Gerätewechsel kann das neue Standardgerät kurz noch fehlen: den
        // Wechsel nicht "verbrauchen", sonst bliebe das entfernte Gerät für immer.
        if changed {
            DEVICE_CHANGED.store(true, Ordering::Relaxed);
        }
        return None;
    };
    if let Some(cb) = &h.volume_watch {
        if let Some((old, _)) = h.endpoint.take() {
            let _ = old.UnregisterControlChangeNotify(cb);
        }
        let _ = ep.RegisterControlChangeNotify(cb);
    }
    h.endpoint = Some((ep.clone(), Instant::now()));
    Some(ep)
}

// ------------------------------------------------------- Rückmeldungen

/// Fenster, das bei Änderungen benachrichtigt wird (die Hauptleiste).
static NOTIFY: AtomicIsize = AtomicIsize::new(0);
/// Standard-Audiogerät gewechselt: beim nächsten Abruf neu öffnen.
static DEVICE_CHANGED: AtomicBool = AtomicBool::new(false);

fn notify() {
    let h = NOTIFY.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(h as _, crate::bar::WM_BAR_STATUS, 0, 0);
        }
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeWatch;

impl IAudioEndpointVolumeCallback_Impl for VolumeWatch_Impl {
    fn OnNotify(&self, _: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        notify();
        Ok(())
    }
}

#[implement(IMMNotificationClient)]
struct DeviceWatch;

impl IMMNotificationClient_Impl for DeviceWatch_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _: &PCWSTR) -> windows::core::Result<()> {
        if flow == eRender && role == eConsole {
            DEVICE_CHANGED.store(true, Ordering::Relaxed);
            notify();
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

unsafe extern "system" fn on_network(_: *const core::ffi::c_void, _: *const windows_sys::Win32::NetworkManagement::IpHelper::MIB_IPINTERFACE_ROW, _: windows_sys::Win32::NetworkManagement::IpHelper::MIB_NOTIFICATION_TYPE) {
    notify();
}

/// Rückmeldungen einrichten; Änderungen kommen als WM_BAR_STATUS an hwnd.
pub fn watch(hwnd: windows_sys::Win32::Foundation::HWND) {
    NOTIFY.store(hwnd as isize, Ordering::Relaxed);
    HANDLES.with(|cell| unsafe {
        let h = &mut *cell.borrow_mut();
        if h.audio.is_none() {
            h.audio = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok();
        }
        let device: IMMNotificationClient = DeviceWatch.into();
        if let Some(en) = &h.audio {
            let _ = en.RegisterEndpointNotificationCallback(&device);
        }
        h.device_watch = Some(device);
        h.volume_watch = Some(VolumeWatch.into());
        // Aktuelles Gerät neu öffnen, damit die Lautstärke-Rückmeldung daran hängt.
        DEVICE_CHANGED.store(true, Ordering::Relaxed);
        let _ = endpoint(h);
        let mut handle: windows_sys::Win32::Foundation::HANDLE = null_mut();
        windows_sys::Win32::NetworkManagement::IpHelper::NotifyIpInterfaceChange(0, Some(on_network), std::ptr::null(), 0, &mut handle);
    });
}

/// Netzwerk hat sich geändert: WLAN beim nächsten Abruf frisch lesen.
pub fn invalidate_network() {
    HANDLES.with(|cell| cell.borrow_mut().wifi = None);
}

unsafe fn open_endpoint(h: &mut Handles) -> Option<IAudioEndpointVolume> {
    if h.audio.is_none() {
        h.audio = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok();
    }
    // Standardgerät jedes Mal neu holen: Kopfhörer rein/raus wechselt es.
    let device = h.audio.as_ref()?.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
    device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
}

pub fn poll() -> Status {
    HANDLES.with(|cell| unsafe {
        let h = &mut *cell.borrow_mut();
        let cached = h.wifi.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(5)).map(|(w, _)| w.clone());
        let wifi_now = match cached {
            Some(w) => w,
            None => {
                let w = wifi(h);
                h.wifi = Some((w.clone(), Instant::now()));
                w
            }
        };
        let net = match wifi_now {
            Some((q, ssid)) => Net::Wifi(q, ssid),
            None => {
                let mut flags = 0u32;
                if InternetGetConnectedState(&mut flags, 0) != 0 {
                    Net::Wired
                } else {
                    Net::Offline
                }
            }
        };
        let (volume, muted) = match endpoint(h) {
            Some(ep) => (
                ep.GetMasterVolumeLevelScalar().unwrap_or(0.0),
                ep.GetMute().map(|b| b.as_bool()).unwrap_or(false),
            ),
            None => (0.0, true),
        };
        let mut ps: SYSTEM_POWER_STATUS = zeroed();
        let battery = if GetSystemPowerStatus(&mut ps) != 0 && ps.BatteryFlag & 128 == 0 && ps.BatteryLifePercent <= 100 {
            Some((ps.BatteryLifePercent, ps.ACLineStatus == 1))
        } else {
            None
        };
        Status { net, volume, muted, battery }
    })
}

pub fn set_volume(v: f32) {
    HANDLES.with(|cell| unsafe {
        if let Some(ep) = endpoint(&mut cell.borrow_mut()) {
            let _ = ep.SetMasterVolumeLevelScalar(v.clamp(0.0, 1.0), std::ptr::null());
            if v > 0.0 {
                let _ = ep.SetMute(false, std::ptr::null());
            }
        }
    })
}

pub fn toggle_mute() {
    HANDLES.with(|cell| unsafe {
        if let Some(ep) = endpoint(&mut cell.borrow_mut()) {
            let muted = ep.GetMute().map(|b| b.as_bool()).unwrap_or(false);
            let _ = ep.SetMute(!muted, std::ptr::null());
        }
    })
}

// Symbole aus Segoe Fluent Icons.
pub fn net_glyph(n: &Net) -> &'static str {
    match n {
        Net::Wifi(q, _) if *q >= 75 => "\u{E701}",
        Net::Wifi(q, _) if *q >= 50 => "\u{E874}",
        Net::Wifi(q, _) if *q >= 25 => "\u{E873}",
        Net::Wifi(..) => "\u{E872}",
        Net::Wired => "\u{E839}",
        Net::Offline => "\u{F384}",
    }
}

pub fn volume_glyph(v: f32, muted: bool) -> &'static str {
    if muted {
        "\u{E74F}"
    } else if v <= 0.0 {
        "\u{E992}"
    } else if v < 0.34 {
        "\u{E993}"
    } else if v < 0.67 {
        "\u{E994}"
    } else {
        "\u{E995}"
    }
}

pub fn battery_glyph(pct: u8, charging: bool) -> String {
    let step = (pct as u32 + 5) / 10; // 0..=10
    let base = if charging { 0xEBAB } else { 0xE850 };
    char::from_u32(base + step.min(10)).map(String::from).unwrap_or_default()
}
