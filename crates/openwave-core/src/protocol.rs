use std::{fmt, time::Duration};

use crate::{
    model::{DeviceSetting, ErrorCode, OperationError, Result, UnitId},
    profiles::{ProfileId, profile_for_usb},
};

pub const BREQUEST_READ: u8 = 0x85;
pub const BREQUEST_WRITE: u8 = 0x05;
pub const RT_CLASS_IN: u8 = 0xa1;
pub const RT_CLASS_OUT: u8 = 0x21;
pub const TRANSFER_TIMEOUT: Duration = Duration::from_millis(1000);
pub const MAX_CONFIG_LEN: usize = 38;
pub const MAX_METER_LEN: usize = 10;
pub const MAX_INFO_LEN: usize = 64;
pub const DOCK_BLOCKS: [(&str, u16, usize); 3] =
    [("settings", 4, 38), ("headphones", 5, 2), ("monitor", 1, 6)];
/// Firmware log readout of a Wave XLR (007d) running a firmware patched to
/// expose its debug log: GET_MEM on the vendor entity with this wValue returns
/// and removes up to `FIRMWARE_LOG_MAX` bytes of that log, and the first read
/// switches logging on. Stock firmware stalls the request.
pub const FIRMWARE_LOG_SELECTOR: u16 = 0x004c;
pub const FIRMWARE_LOG_MAX: usize = 2048;

/// State snapshot of a Wave XLR (007d) running a patched firmware. Stock
/// firmware refuses (stalls) this request.
pub const FIRMWARE_SNAPSHOT_SELECTOR: u16 = 0x0053;
pub const FIRMWARE_SNAPSHOT_LEN: usize = 200;

/// Whether a firmware snapshot comes from a build that reports its own control
/// changes (mic gain and mute, headphone volume and mute) with USB Audio status
/// interrupts, so that ALSA follows the knob and touch mute without polling.
/// Builds 4 and 5 of the patched 1.35 firmware do.
pub fn firmware_reports_changes(snapshot: &[u8]) -> bool {
    snapshot.len() >= 0x20
        && snapshot.starts_with(b"WXS1")
        && matches!(u16::from_le_bytes([snapshot[0x1e], snapshot[0x1f]]), 4 | 5)
}

/// Split firmware log bytes into complete lines, carrying an unfinished line
/// over in `partial`. Lines end in CR LF; CR and other control characters are
/// dropped, bytes are read as Latin-1 so no input can fail to decode.
pub fn firmware_log_lines(partial: &mut String, bytes: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    for &byte in bytes {
        match byte {
            b'\n' => lines.push(std::mem::take(partial)),
            b'\t' => partial.push(' '),
            byte if byte < 0x20 || byte == 0x7f => {}
            byte => partial.push(char::from(byte)),
        }
    }
    // A runaway line without a newline must not grow without bound.
    if partial.len() > 1024 {
        lines.push(std::mem::take(partial));
    }
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnobMode {
    Gain,
    Headphones,
    MonitorMix,
    None,
}

impl fmt::Display for KnobMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gain => "Gain",
            Self::Headphones => "Headphones",
            Self::MonitorMix => "Monitor mix",
            Self::None => "None",
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub api: String,
    pub firmware: String,
    pub serial: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceState {
    pub gain_raw: u16,
    pub muted: bool,
    pub hp_volume_db: f64,
    pub phantom: Option<bool>,
    pub low_impedance: Option<bool>,
    pub monitor_mix: Option<u16>,
    pub knob_mode: KnobMode,
}

/// Owned production config. Only exact profile-length reads can construct it;
/// all setting changes retain the original reserved bytes. The runtime must
/// serialize read, patch and write as a single transaction on the captured unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigBuffer {
    profile: ProfileId,
    bytes: [u8; MAX_CONFIG_LEN],
    headphones: [u8; 2],
    monitor: [u8; 6],
}

impl ConfigBuffer {
    pub fn decode(profile: ProfileId, data: &[u8]) -> Result<Self> {
        let legacy = profile.profile().legacy.ok_or_else(|| {
            OperationError::new(
                ErrorCode::Unsupported,
                "Dock config requires all three blocks",
            )
        })?;
        exact_length(data, legacy.config_len, "config")?;
        let mut bytes = [0; MAX_CONFIG_LEN];
        bytes[..data.len()].copy_from_slice(data);
        Ok(Self {
            profile,
            bytes,
            headphones: [0; 2],
            monitor: [0; 6],
        })
    }

    pub fn decode_dock(settings: &[u8], headphones: &[u8], monitor: &[u8]) -> Result<Self> {
        exact_length(settings, 38, "settings")?;
        exact_length(headphones, 2, "headphones")?;
        exact_length(monitor, 6, "monitor")?;
        let mut config = Self {
            profile: ProfileId::XlrDockMk2,
            bytes: [0; MAX_CONFIG_LEN],
            headphones: [0; 2],
            monitor: [0; 6],
        };
        config.bytes.copy_from_slice(settings);
        config.headphones.copy_from_slice(headphones);
        config.monitor.copy_from_slice(monitor);
        Ok(config)
    }

    pub fn profile(&self) -> ProfileId {
        self.profile
    }

    pub fn as_bytes(&self) -> &[u8] {
        let length = self
            .profile
            .profile()
            .legacy
            .map_or(38, |legacy| legacy.config_len);
        &self.bytes[..length]
    }

    pub fn blocks(&self) -> impl Iterator<Item = (u16, &[u8])> {
        let legacy = self.profile.profile().legacy;
        [
            Some((legacy.map_or(4, |p| p.wvalue_config), self.as_bytes())),
            legacy.is_none().then_some((5, self.headphones.as_slice())),
            legacy.is_none().then_some((1, self.monitor.as_slice())),
        ]
        .into_iter()
        .flatten()
    }

    pub fn state(&self) -> DeviceState {
        let profile = self.profile.profile();
        let Some(p) = profile.legacy else {
            return DeviceState {
                gain_raw: u16::from(self.bytes[0]),
                muted: self.bytes[1] & 1 != 0,
                hp_volume_db: -f64::from(self.headphones[0]) / 4.0,
                phantom: Some(self.bytes[1] & 2 != 0),
                low_impedance: Some(self.headphones[1] & 2 != 0),
                monitor_mix: Some(u16::from(self.monitor[0])),
                knob_mode: KnobMode::None,
            };
        };
        let knob_mode = match self.bytes[p.off_vol_select] {
            2 => KnobMode::Headphones,
            3 if p.off_monitor_mix.is_some() => KnobMode::MonitorMix,
            _ => KnobMode::Gain,
        };
        DeviceState {
            gain_raw: read_u16(&self.bytes, p.off_gain),
            muted: self.bytes[p.off_mute] != 0,
            hp_volume_db: f64::from(read_i16(&self.bytes, p.off_hp_vol))
                / f64::from(profile.hp_scale),
            phantom: p.off_phantom.map(|offset| self.bytes[offset] != 0),
            low_impedance: p.off_low_z.map(|offset| self.bytes[offset] != 0),
            monitor_mix: p
                .off_monitor_mix
                .map(|offset| read_u16(&self.bytes, offset)),
            knob_mode,
        }
    }

    /// Refuses unsupported/nonfinite settings before changing any bytes.
    pub fn apply(&mut self, setting: DeviceSetting) -> Result<()> {
        let setting = validate_setting(self.profile, setting)?;
        let profile = self.profile.profile();
        let Some(p) = profile.legacy else {
            match setting {
                DeviceSetting::GainRaw(value) => self.bytes[0] = value as u8,
                DeviceSetting::Mute(value) => {
                    self.bytes[1] = (self.bytes[1] & !1) | u8::from(value);
                }
                DeviceSetting::Phantom(value) => {
                    self.bytes[1] = (self.bytes[1] & !2) | (u8::from(value) << 1);
                }
                DeviceSetting::HeadphoneDb(db) => {
                    self.headphones[0] = (-db * 4.0).round_ties_even() as u8;
                }
                DeviceSetting::LowImpedance(value) => {
                    self.headphones[1] = (self.headphones[1] & !2) | (u8::from(value) << 1);
                }
                DeviceSetting::MonitorMix(value) => self.monitor[0] = value as u8,
            }
            return Ok(());
        };
        match setting {
            DeviceSetting::GainRaw(value) => self.put_u16(p.off_gain, value),
            DeviceSetting::Mute(value) => self.bytes[p.off_mute] = u8::from(value),
            DeviceSetting::HeadphoneDb(db) => {
                // Truncate toward zero, matching the firmware setter's int().
                let raw = (db * f64::from(profile.hp_scale)) as i16;
                self.bytes[p.off_hp_vol..p.off_hp_vol + 2].copy_from_slice(&raw.to_le_bytes());
            }
            DeviceSetting::Phantom(value) => {
                self.bytes[p.off_phantom.expect("validated capability")] = u8::from(value)
            }
            DeviceSetting::LowImpedance(value) => {
                self.bytes[p.off_low_z.expect("validated capability")] = u8::from(value)
            }
            DeviceSetting::MonitorMix(value) => {
                self.put_u16(p.off_monitor_mix.expect("validated capability"), value)
            }
        }
        Ok(())
    }

    fn put_u16(&mut self, offset: usize, value: u16) {
        self.bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
}

/// Queue admission and the final backend patch use the same capability checks.
/// Read observations are not clamped; requested writes are profile-bounded.
pub fn validate_setting(profile: ProfileId, setting: DeviceSetting) -> Result<DeviceSetting> {
    let p = profile.profile();
    let unsupported = |feature: &str| {
        OperationError::new(
            ErrorCode::Unsupported,
            format!("{} does not support {feature}", p.display_name),
        )
    };
    Ok(match setting {
        DeviceSetting::GainRaw(value) => DeviceSetting::GainRaw(value.min(p.gain_max)),
        DeviceSetting::HeadphoneDb(db) => {
            if !db.is_finite() {
                return Err(OperationError::invalid("Headphone volume must be finite"));
            }
            DeviceSetting::HeadphoneDb(db.clamp(p.hp_min_db(), 0.0))
        }
        DeviceSetting::Phantom(_) if !p.has_phantom() => return Err(unsupported("phantom power")),
        DeviceSetting::LowImpedance(_) if !p.has_low_z() => {
            return Err(unsupported("low impedance mode"));
        }
        DeviceSetting::MonitorMix(_) if !p.has_monitor_mix() => {
            return Err(unsupported("monitor mix"));
        }
        DeviceSetting::MonitorMix(value) => DeviceSetting::MonitorMix(value.min(p.mix_max)),
        value => value,
    })
}

pub fn decode_device_info(profile: ProfileId, data: &[u8]) -> Result<DeviceInfo> {
    let p = profile.profile().legacy.ok_or_else(|| {
        OperationError::new(
            ErrorCode::Unsupported,
            "Dock has no known device-info block",
        )
    })?;
    exact_length(data, p.devinfo_len, "device info")?;
    let serial_bytes = &data[p.devinfo_serial.0..p.devinfo_serial.1];
    let serial_end = serial_bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |i| i + 1);
    // ASCII replacement is per byte, not UTF-8 lossiness; retain embedded NULs.
    let serial = serial_bytes[..serial_end]
        .iter()
        .map(|byte| {
            if byte.is_ascii() {
                char::from(*byte)
            } else {
                '\u{fffd}'
            }
        })
        .collect();
    Ok(DeviceInfo {
        api: format!("{}.{}", data[p.devinfo_api[0]], data[p.devinfo_api[1]]),
        firmware: format!(
            "{}.{}.{}",
            data[p.devinfo_fw[0]], data[p.devinfo_fw[1]], data[p.devinfo_fw[2]]
        ),
        serial,
    })
}

/// Vendor meters are raw unsigned little-endian levels, not PCM samples or dB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterLevels {
    pub left: u32,
    pub right: u32,
}

pub fn decode_meters(profile: ProfileId, data: &[u8]) -> Result<MeterLevels> {
    let legacy = profile.profile().legacy.ok_or_else(|| {
        OperationError::new(ErrorCode::Unsupported, "Dock has no known meter block")
    })?;
    exact_length(data, legacy.meter_len, "meter")?;
    Ok(MeterLevels {
        left: u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
        right: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
    })
}

fn exact_length(data: &[u8], expected: usize, block: &str) -> Result<()> {
    if data.len() == expected {
        Ok(())
    } else {
        Err(OperationError::invalid(format!(
            "Invalid {block} block length: {} bytes, expected {expected}",
            data.len()
        )))
    }
}

fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}
fn read_i16(data: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes([data[offset], data[offset + 1]])
}

pub fn gain_raw_to_db(profile: ProfileId, raw: u16) -> f64 {
    f64::from(raw) / f64::from(profile.profile().gain_scale)
}

pub fn fw_gain_to_alsa(profile: ProfileId, raw: u16) -> i32 {
    (gain_raw_to_db(profile, raw) / 0.5).round_ties_even() as i32
}

pub fn fw_hp_to_alsa(profile: ProfileId, raw: i16) -> i32 {
    let db = f64::from(raw) / f64::from(profile.profile().hp_scale);
    (db / 0.5 + 120.0).round_ties_even().clamp(0.0, 120.0) as i32
}

pub fn alsa_hp_to_fw(profile: ProfileId, value: i32) -> i16 {
    let db = ((f64::from(value) - 120.0) * 0.5).clamp(-128.0, 0.0);
    (db * f64::from(profile.profile().hp_scale)) as i16
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbIdentity {
    pub vid: u16,
    pub pid: u16,
    pub bus: u8,
    pub address: u8,
}

/// Keeps same-model duplicates and deterministic bus/address enumeration order.
pub fn supported_devices(
    devices: impl IntoIterator<Item = UsbIdentity>,
) -> Vec<(ProfileId, u8, u8)> {
    let mut found: Vec<_> = devices
        .into_iter()
        .filter_map(|device| {
            profile_for_usb(device.vid, device.pid).map(|p| (p.id, device.bus, device.address))
        })
        .collect();
    found.sort_by_key(|(_, bus, address)| (*bus, *address));
    found
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlsaCardIdentity {
    pub card: u32,
    pub usb: UsbIdentity,
}

impl AlsaCardIdentity {
    /// Parse /proc/asound/cardN/{usbid,usbbus}; there is no name-only fallback.
    pub fn parse(card: u32, usbid: &str, usbbus: &str) -> Result<Self> {
        let (vid, pid) = Self::parse_usb_id(usbid)?;
        let bus = usbbus.trim();
        let bad = || OperationError::new(ErrorCode::Identity, "Malformed ALSA USB identity");
        let (bus, address) = bus.split_once('/').ok_or_else(bad)?;
        if bus.len() != 3
            || address.len() != 3
            || !bus
                .bytes()
                .chain(address.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return Err(bad());
        }
        Ok(Self {
            card,
            usb: UsbIdentity {
                vid,
                pid,
                bus: bus.parse().map_err(|_| bad())?,
                address: address.parse().map_err(|_| bad())?,
            },
        })
    }

    /// Identify unrelated USB products before requiring their bus metadata.
    pub fn parse_usb_id(usbid: &str) -> Result<(u16, u16)> {
        let bad = || OperationError::new(ErrorCode::Identity, "Malformed ALSA USB identity");
        let (vid, pid) = usbid.trim().split_once(':').ok_or_else(bad)?;
        if vid.len() != 4
            || pid.len() != 4
            || !vid
                .bytes()
                .chain(pid.bytes())
                .all(|b| b.is_ascii_hexdigit())
        {
            return Err(bad());
        }
        Ok((
            u16::from_str_radix(vid, 16).map_err(|_| bad())?,
            u16::from_str_radix(pid, 16).map_err(|_| bad())?,
        ))
    }
}

pub fn card_for_unit(unit: UnitId, cards: &[AlsaCardIdentity]) -> Option<u32> {
    let p = unit.profile.profile();
    let wanted = UsbIdentity {
        vid: p.vid,
        pid: p.pid,
        bus: unit.bus,
        address: unit.address,
    };
    let mut matches = cards.iter().filter(|card| card.usb == wanted);
    let matched = matches.next()?;
    if matches.next().is_some() {
        None
    } else {
        Some(matched.card)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ConnectedIdentity<'a> {
    pub unit: UnitId,
    pub serial: &'a str,
    pub connected: bool,
}

/// Accepts bare firmware serial or the entire model-qualified udev ID_SERIAL.
/// Card indices, serial suffixes and missing/ambiguous serials never select USB.
pub fn device_for_capture(serial: &str, devices: &[ConnectedIdentity<'_>]) -> Option<UnitId> {
    if serial.is_empty() {
        return None;
    }
    let mut matched = None;
    for device in devices {
        if !device.connected || device.serial.is_empty() {
            continue;
        }
        let prefix = device.unit.profile.profile().capture_serial_prefix;
        if serial != device.serial && serial.strip_prefix(prefix) != Some(device.serial) {
            continue;
        }
        if matched.is_some() {
            return None;
        }
        matched = Some(device.unit);
    }
    matched
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlsaRange {
    pub min: i32,
    pub max: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlsaControl {
    pub numid: u32,
    pub range: Option<AlsaRange>,
}

impl AlsaControl {
    pub fn clamp(&self, value: i32) -> Result<i32> {
        let range = self
            .range
            .ok_or_else(|| OperationError::unavailable("ALSA control range unavailable"))?;
        if range.min > range.max {
            return Err(OperationError::invalid("Invalid ALSA control range"));
        }
        Ok(value.clamp(range.min, range.max))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlsaControls {
    pub mute: Option<AlsaControl>,
    pub gain: Option<AlsaControl>,
    pub hp_volume: Option<AlsaControl>,
}

/// Discover roles by their existing driver name suffix and MIXER interface.
/// Absent/malformed roles stay absent; missing ranges never acquire defaults.
pub fn parse_alsa_controls(contents: &str) -> AlsaControls {
    let mut result = AlsaControls::default();
    let mut current = None;
    for line in contents.lines().map(str::trim) {
        if line.starts_with("numid=") {
            current = parse_control_header(line);
            continue;
        }
        let Some((numid, name)) = current else {
            continue;
        };
        let Some(metadata) = line.strip_prefix("; type=") else {
            continue;
        };
        let kind = metadata.split(',').next().unwrap_or("");
        let target = if name.ends_with("Capture Switch") && kind == "BOOLEAN" {
            &mut result.mute
        } else if name.ends_with("Capture Volume") && kind == "INTEGER" {
            &mut result.gain
        } else if name.ends_with("Playback Volume") && kind == "INTEGER" {
            &mut result.hp_volume
        } else {
            continue;
        };
        if target.is_some() {
            continue;
        }
        let range = metadata_number(metadata, "min")
            .zip(metadata_number(metadata, "max"))
            .filter(|(min, max)| min <= max)
            .map(|(min, max)| AlsaRange { min, max });
        *target = Some(AlsaControl { numid, range });
    }
    result
}

fn parse_control_header(line: &str) -> Option<(u32, &str)> {
    let line = line.strip_prefix("numid=")?;
    let (numid, line) = line.split_once(",iface=MIXER,name='")?;
    let numid = numid.parse().ok().filter(|numid| *numid != 0)?;
    let end = line.rfind('\'')?;
    Some((numid, &line[..end]))
}

fn metadata_number(metadata: &str, key: &str) -> Option<i32> {
    metadata.split(',').find_map(|part| {
        let (name, value) = part.split_once('=')?;
        if name == key {
            value.parse().ok()
        } else {
            None
        }
    })
}

fn alsa_value(output: &str) -> Option<&str> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix(": values="))
}

pub fn parse_alsa_mute(output: &str) -> Option<bool> {
    match alsa_value(output)? {
        "off" => Some(true),
        "on" => Some(false),
        _ => None,
    }
}

pub fn parse_alsa_volume(output: &str) -> Option<i32> {
    alsa_value(output)?.parse().ok()
}

#[cfg(test)]
mod firmware_log_tests {
    use super::{firmware_log_lines, firmware_reports_changes};

    #[test]
    fn only_notifying_builds_report_their_own_changes() {
        let snapshot = |build: u16| {
            let mut bytes = vec![0; 200];
            bytes[..4].copy_from_slice(b"WXS1");
            bytes[0x1e..0x20].copy_from_slice(&build.to_le_bytes());
            bytes
        };
        assert!(firmware_reports_changes(&snapshot(4)));
        assert!(firmware_reports_changes(&snapshot(5)));
        for build in [0, 1, 2, 3, 6] {
            assert!(!firmware_reports_changes(&snapshot(build)), "build {build}");
        }
        assert!(!firmware_reports_changes(&snapshot(4)[..0x1f]));
        let mut wrong = snapshot(4);
        wrong[0] = b'X';
        assert!(!firmware_reports_changes(&wrong));
    }

    #[test]
    fn firmware_log_lines_carry_partial_lines_across_reads() {
        let mut partial = String::new();
        assert_eq!(
            firmware_log_lines(&mut partial, b"HP Itf: Alt. Setting 1.\r\nMic It"),
            ["HP Itf: Alt. Setting 1."]
        );
        assert_eq!(partial, "Mic It");
        assert_eq!(
            firmware_log_lines(&mut partial, b"f: Alt. Setting 1.\r\n"),
            ["Mic Itf: Alt. Setting 1."]
        );
        assert!(partial.is_empty());
    }

    #[test]
    fn firmware_log_lines_never_fail_or_grow_without_bound() {
        let mut partial = String::new();
        assert_eq!(
            firmware_log_lines(&mut partial, b"\xff\x00a\tb\n"),
            ["\u{ff}a b"]
        );
        let lines = firmware_log_lines(&mut partial, &[b'x'; 1500]);
        assert_eq!(lines.len(), 1);
        assert!(partial.is_empty());
    }
}
