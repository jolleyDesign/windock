//! Volume, network and battery for the dock's status icons. On Windows 11 these aren't
//! tray icons (the taskbar draws them itself), so they're read straight from Windows.
//!
//! Nothing polls: Core Audio, IP Helper and WLAN call back when something changes, and
//! battery changes arrive as WM_POWERBROADCAST. Callbacks run on system threads and only
//! call the `notify` function the dock passes in.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Foundation::{HANDLE, NO_ERROR};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl};
use windows::Win32::Media::Audio::{
    AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, GetBestInterface, GetIfEntry2, GetNetworkConnectivityHint, IF_TYPE_IEEE80211, MIB_IF_ROW2, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
    NotifyIpInterfaceChange, NotifyNetworkConnectivityHintChange,
};
use windows::Win32::NetworkManagement::WiFi::{
    L2_NOTIFICATION_DATA, WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO_LIST, WLAN_NOTIFICATION_SOURCE_ACM, WLAN_NOTIFICATION_SOURCE_MSM, WlanCloseHandle, WlanEnumInterfaces,
    WlanFreeMemory, WlanOpenHandle, WlanQueryInterface, WlanRegisterNotification, wlan_interface_state_connected, wlan_intf_opcode_current_connection,
};
use windows::Win32::Networking::WinSock::{
    AF_UNSPEC, NL_NETWORK_CONNECTIVITY_HINT, NetworkConnectivityLevelHintConstrainedInternetAccess, NetworkConnectivityLevelHintInternetAccess,
    NetworkConnectivityLevelHintLocalAccess,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::core::{PCWSTR, Result, implement};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Volume {
    /// 0.0 to 1.0.
    pub level: f32,
    pub muted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Link {
    #[default]
    None,
    Wired,
    /// Signal quality 0-100, when Windows lets us read it.
    Wifi(Option<u32>),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Network {
    pub link: Link,
    pub internet: bool,
    /// The Wi-Fi network's name.
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Status {
    pub volume: Option<Volume>,
    pub network: Network,
    pub battery: Option<Battery>,
}

// ---------------------------------------------------------------- glyphs
// Segoe Fluent Icons (Segoe MDL2 Assets on Windows 10), the taskbar's own icon font.

pub fn volume_glyph(v: &Volume) -> char {
    match v.level {
        _ if v.muted => '\u{E74F}',
        l if l <= 0.0 => '\u{E992}',
        l if l < 0.34 => '\u{E993}',
        l if l < 0.67 => '\u{E994}',
        _ => '\u{E995}',
    }
}

pub fn network_glyph(n: &Network) -> char {
    match n.link {
        Link::None => '\u{F384}',
        Link::Wired if n.internet => '\u{E839}',
        Link::Wired => '\u{EB56}',
        Link::Wifi(signal) => {
            // 1-4 bars, like the taskbar; full when the signal can't be read.
            let bars = signal.map_or(4, |q| match q {
                0..25 => 1,
                25..50 => 2,
                50..75 => 3,
                _ => 4,
            });
            match (n.internet, bars) {
                (false, b) => char::from_u32(0xEB5F + b).unwrap_or('\u{EB63}'),
                (true, 1) => '\u{E872}',
                (true, 2) => '\u{E873}',
                (true, 3) => '\u{E874}',
                (true, _) => '\u{E701}',
            }
        }
    }
}

pub fn battery_glyph(b: &Battery) -> char {
    const LEVELS: [u32; 11] = [0xE850, 0xE851, 0xE852, 0xE853, 0xE854, 0xE855, 0xE856, 0xE857, 0xE858, 0xE859, 0xE83F];
    const CHARGING: [u32; 11] = [0xE85A, 0xE85B, 0xE85C, 0xE85D, 0xE85E, 0xE85F, 0xE860, 0xE861, 0xE862, 0xE83E, 0xEA93];
    let step = (b.percent.min(100) as usize + 5) / 10;
    let table = if b.charging { &CHARGING } else { &LEVELS };
    char::from_u32(table[step]).unwrap_or('\u{E996}')
}

/// For the hover label, e.g. "Wi-Fi (Home): Internet access · Volume 45% · Battery 80%".
pub fn describe(s: &Status, volume: bool, network: bool, battery: bool) -> String {
    let mut parts = Vec::new();
    if network {
        let access = if s.network.internet { "Internet access" } else { "No internet" };
        parts.push(match (&s.network.link, &s.network.name) {
            (Link::None, _) => "Not connected".to_string(),
            (Link::Wired, _) => format!("Ethernet: {access}"),
            (Link::Wifi(_), Some(name)) => format!("Wi-Fi ({name}): {access}"),
            (Link::Wifi(_), None) => format!("Wi-Fi: {access}"),
        });
    }
    if volume && let Some(v) = &s.volume {
        parts.push(if v.muted { "Muted".to_string() } else { format!("Volume {}%", (v.level * 100.0).round()) });
    }
    if battery && let Some(b) = &s.battery {
        parts.push(format!("Battery {}%{}", b.percent, if b.charging { ", charging" } else { "" }));
    }
    parts.join("  ·  ")
}

// --------------------------------------------------------------- watching

static NOTIFY: OnceLock<fn()> = OnceLock::new();
/// The default playback device changed: re-attach to the new one on the next read.
static REBIND_AUDIO: AtomicBool = AtomicBool::new(false);

fn notify() {
    if let Some(f) = NOTIFY.get() {
        f();
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeChanged;

impl IAudioEndpointVolumeCallback_Impl for VolumeChanged_Impl {
    fn OnNotify(&self, _data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> Result<()> {
        notify();
        Ok(())
    }
}

#[implement(IMMNotificationClient)]
struct DevicesChanged;

impl IMMNotificationClient_Impl for DevicesChanged_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> Result<()> {
        rebind();
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> Result<()> {
        rebind();
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> Result<()> {
        if flow == eRender && role == eConsole {
            rebind();
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> Result<()> {
        Ok(())
    }
}

fn rebind() {
    REBIND_AUDIO.store(true, Ordering::Relaxed);
    notify();
}

unsafe extern "system" fn ip_changed(_: *const core::ffi::c_void, _: *const MIB_IPINTERFACE_ROW, _: MIB_NOTIFICATION_TYPE) {
    notify();
}

unsafe extern "system" fn connectivity_changed(_: *const core::ffi::c_void, _: NL_NETWORK_CONNECTIVITY_HINT) {
    notify();
}

unsafe extern "system" fn wlan_changed(_: *mut L2_NOTIFICATION_DATA, _: *mut core::ffi::c_void) {
    notify();
}

struct Audio {
    devices: IMMDeviceEnumerator,
    devices_changed: IMMNotificationClient,
    endpoint: Option<(IAudioEndpointVolume, IAudioEndpointVolumeCallback)>,
}

impl Audio {
    fn new() -> Option<Self> {
        unsafe {
            let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER).ok()?;
            let devices_changed: IMMNotificationClient = DevicesChanged.into();
            let _ = devices.RegisterEndpointNotificationCallback(&devices_changed);
            let mut audio = Self { devices, devices_changed, endpoint: None };
            audio.bind();
            Some(audio)
        }
    }

    /// Attaches to the current default playback device (there may be none).
    fn bind(&mut self) {
        self.unbind();
        self.endpoint = unsafe {
            self.devices.GetDefaultAudioEndpoint(eRender, eConsole).ok().and_then(|device| {
                let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
                let changed: IAudioEndpointVolumeCallback = VolumeChanged.into();
                volume.RegisterControlChangeNotify(&changed).ok()?;
                Some((volume, changed))
            })
        };
    }

    fn unbind(&mut self) {
        if let Some((volume, changed)) = self.endpoint.take() {
            unsafe {
                let _ = volume.UnregisterControlChangeNotify(&changed);
            }
        }
    }

    fn read(&self) -> Option<Volume> {
        let (v, _) = self.endpoint.as_ref()?;
        unsafe { Some(Volume { level: v.GetMasterVolumeLevelScalar().ok()?, muted: v.GetMute().ok()?.as_bool() }) }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.unbind();
        unsafe {
            let _ = self.devices.UnregisterEndpointNotificationCallback(&self.devices_changed);
        }
    }
}

/// Watches for changes while the status icons are on the dock.
pub struct Watcher {
    audio: Option<Audio>,
    wlan: Option<HANDLE>,
    ip_notifications: Vec<HANDLE>,
}

impl Watcher {
    /// `on_change` is called from other threads; it should only post a message.
    pub fn new(on_change: fn()) -> Self {
        let _ = NOTIFY.set(on_change);
        let mut ip_notifications = Vec::new();
        unsafe {
            let mut h = HANDLE::default();
            if NotifyNetworkConnectivityHintChange(Some(connectivity_changed), None, false, &mut h) == NO_ERROR {
                ip_notifications.push(h);
            }
            let mut h = HANDLE::default();
            if NotifyIpInterfaceChange(AF_UNSPEC, Some(ip_changed), None, false, &mut h) == NO_ERROR {
                ip_notifications.push(h);
            }
        }
        let wlan = unsafe {
            let (mut version, mut h) = (0, HANDLE::default());
            (WlanOpenHandle(2, None, &mut version, &mut h) == NO_ERROR.0).then(|| {
                let sources = WLAN_NOTIFICATION_SOURCE_MSM | WLAN_NOTIFICATION_SOURCE_ACM;
                WlanRegisterNotification(h, sources, false, Some(wlan_changed), None, None, None);
                h
            })
        };
        Self { audio: Audio::new(), wlan, ip_notifications }
    }

    pub fn read(&mut self) -> Status {
        if REBIND_AUDIO.swap(false, Ordering::Relaxed)
            && let Some(a) = &mut self.audio
        {
            a.bind();
        }
        Status { volume: self.audio.as_ref().and_then(Audio::read), network: self.network(), battery: battery() }
    }

    /// Nudges the volume by `delta` (-1.0 to 1.0), unmuting if it goes up.
    pub fn change_volume(&self, delta: f32) {
        let Some((v, _)) = self.audio.as_ref().and_then(|a| a.endpoint.as_ref()) else { return };
        unsafe {
            let level = v.GetMasterVolumeLevelScalar().unwrap_or(0.0);
            let _ = v.SetMasterVolumeLevelScalar((level + delta).clamp(0.0, 1.0), std::ptr::null());
            if delta > 0.0 {
                let _ = v.SetMute(false, std::ptr::null());
            }
        }
    }

    pub fn toggle_mute(&self) {
        let Some((v, _)) = self.audio.as_ref().and_then(|a| a.endpoint.as_ref()) else { return };
        unsafe {
            let muted = v.GetMute().map(|m| m.as_bool()).unwrap_or(false);
            let _ = v.SetMute(!muted, std::ptr::null());
        }
    }

    fn network(&self) -> Network {
        let mut hint = NL_NETWORK_CONNECTIVITY_HINT::default();
        let level = (unsafe { GetNetworkConnectivityHint(&mut hint) } == NO_ERROR).then_some(hint.ConnectivityLevel);
        let internet = level.is_some_and(|l| l == NetworkConnectivityLevelHintInternetAccess || l == NetworkConnectivityLevelHintConstrainedInternetAccess);
        // The adapter traffic leaves through: whichever has the best route to the internet.
        // Looking up a route sends nothing.
        let mut index = 0;
        let routed = unsafe { GetBestInterface(u32::from_ne_bytes([1, 1, 1, 1]), &mut index) } == NO_ERROR.0;
        let mut row = MIB_IF_ROW2 { InterfaceIndex: index, ..Default::default() };
        let wifi = routed && unsafe { GetIfEntry2(&mut row) } == NO_ERROR && row.Type == IF_TYPE_IEEE80211;
        let connected = routed || internet || level == Some(NetworkConnectivityLevelHintLocalAccess);
        match (connected, wifi) {
            (false, _) => Network::default(),
            (true, false) => Network { link: Link::Wired, internet, name: None },
            (true, true) => {
                let (signal, name) = self.wifi_details();
                Network { link: Link::Wifi(signal), internet, name }
            }
        }
    }

    /// Signal and network name of the connected Wi-Fi adapter. Windows 11 may withhold
    /// these from apps without location access, in which case they're `None`.
    fn wifi_details(&self) -> (Option<u32>, Option<String>) {
        let Some(h) = self.wlan else { return (None, None) };
        unsafe {
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(h, None, &mut list) != NO_ERROR.0 || list.is_null() {
                return (None, None);
            }
            let count = (*list).dwNumberOfItems as usize;
            let infos = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), count);
            let mut found = (None, None);
            for info in infos.iter().filter(|i| i.isState == wlan_interface_state_connected) {
                let (mut size, mut data) = (0u32, std::ptr::null_mut());
                if WlanQueryInterface(h, &info.InterfaceGuid, wlan_intf_opcode_current_connection, None, &mut size, &mut data, None) != NO_ERROR.0 || data.is_null() {
                    continue;
                }
                let conn = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
                let ssid = &conn.wlanAssociationAttributes.dot11Ssid;
                let name = String::from_utf8_lossy(&ssid.ucSSID[..(ssid.uSSIDLength as usize).min(32)]).into_owned();
                found = (Some(conn.wlanAssociationAttributes.wlanSignalQuality), Some(name).filter(|n| !n.is_empty()));
                WlanFreeMemory(data);
                break;
            }
            WlanFreeMemory(list as _);
            found
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            for h in self.ip_notifications.drain(..) {
                let _ = CancelMibChangeNotify2(h);
            }
            if let Some(h) = self.wlan.take() {
                WlanCloseHandle(h, None);
            }
        }
    }
}

fn battery() -> Option<Battery> {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s) }.ok()?;
    // 128: no system battery; 255: unknown.
    if s.BatteryFlag & 128 != 0 || s.BatteryFlag == 255 || s.BatteryLifePercent > 100 {
        return None;
    }
    Some(Battery { percent: s.BatteryLifePercent, charging: s.ACLineStatus == 1 || s.BatteryFlag & 8 != 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_icons() {
        let v = |level, muted| volume_glyph(&Volume { level, muted });
        assert_eq!(v(0.5, true), '\u{E74F}');
        assert_eq!(v(0.0, false), '\u{E992}');
        assert_eq!(v(0.2, false), '\u{E993}');
        assert_eq!(v(0.5, false), '\u{E994}');
        assert_eq!(v(1.0, false), '\u{E995}');
    }

    #[test]
    fn network_icons() {
        let n = |link, internet| network_glyph(&Network { link, internet, name: None });
        assert_eq!(n(Link::None, false), '\u{F384}');
        assert_eq!(n(Link::Wired, true), '\u{E839}');
        assert_eq!(n(Link::Wired, false), '\u{EB56}');
        assert_eq!(n(Link::Wifi(None), true), '\u{E701}');
        assert_eq!(n(Link::Wifi(Some(10)), true), '\u{E872}');
        assert_eq!(n(Link::Wifi(Some(60)), true), '\u{E874}');
        assert_eq!(n(Link::Wifi(Some(30)), false), '\u{EB61}');
    }

    #[test]
    fn battery_icons() {
        let b = |percent, charging| battery_glyph(&Battery { percent, charging });
        assert_eq!(b(0, false), '\u{E850}');
        assert_eq!(b(54, false), '\u{E855}');
        assert_eq!(b(100, false), '\u{E83F}');
        assert_eq!(b(96, true), '\u{EA93}');
        assert_eq!(b(88, true), '\u{E83E}');
    }

    #[test]
    fn descriptions() {
        let s = Status {
            volume: Some(Volume { level: 0.45, muted: false }),
            network: Network { link: Link::Wifi(Some(80)), internet: true, name: Some("Home".into()) },
            battery: Some(Battery { percent: 80, charging: true }),
        };
        assert_eq!(describe(&s, true, true, true), "Wi-Fi (Home): Internet access  ·  Volume 45%  ·  Battery 80%, charging");
        assert_eq!(describe(&s, true, false, false), "Volume 45%");
    }
}
