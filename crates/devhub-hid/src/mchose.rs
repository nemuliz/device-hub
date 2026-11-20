//! MCHOSE HUB 系マウス — MCHOSE L7 Ultra 等。
//!
//! www.mchose.com.cn の WebHID プロトコル (JS 解析) を Rust に移植。
//!
//! - 設定チャネル: vendor usage page 0xFF01 / usage 0x0001 の feature report。
//! - 読み: `0x11 0x06` (device) / `0x11 0x04` (firmware string) /
//!   `0x12 0x67` (allInfo)。応答は XOR 0xFF 反転 + エコー subcommand 付き。
//! - 書き: 読みと同じ feature report に、report id + XOR 0xFF 反転データ +
//!   0xFF パディングを送る。MCHOSE HUB の `setCommand` と同一形式。
//!   - DPI: report `0x12` / command `0x40` (dpiXYSetParser)
//!   - レート: report `0x11` / command `0x41` (reportParser)
//!   - スリープ: report `0x11` / command `0x0A`
//!   - デバウンス: report `0x11` / command `0x43`

use devhub_core::backend::*;
use devhub_core::model::*;
use std::sync::Mutex;
use std::time::Duration;

pub struct MchoseMouse {
    dev: Mutex<hidapi::HidDevice>,
    info: Mutex<DeviceInfo>,
    caps: Capabilities,
    last_all: Mutex<AllRead>,
}

#[derive(Clone, Default)]
struct AllRead {
    profile_index: u8,
    g_dpi_index: u8,
    g_rate_index: u8,
    usb_dpi_index: u8,
    usb_rate_index: u8,
    dpi: [u16; 6],
    dpi_sum: u8,
    sensor: u8,
    key_debounce: u8,
    sleep: u8,
    rotate_val: u8,
    battery: u8,
    fw_version: u32,
    /// `deviceInfo.connectMode` from `0x11 0x06` (true = 2.4G wireless).
    connect_mode: bool,
}

impl MchoseMouse {
    fn send_feature_read(dev: &hidapi::HidDevice, cmd: u8, sub: u8, len: usize) -> HubResult<Vec<u8>> {
        // body = [cmd, sub^0xFF, 0xFF, ...] (len B) — cmd が report id、
        // サブコマンド以降は XOR 0xFF で反転して送る (JS の readCommand と同じ)。
        let mut buf = vec![0xFFu8; len];
        buf[0] = cmd;
        if len > 1 {
            buf[1] = sub ^ 0xFF;
        }
        dev.send_feature_report(&buf)
            .map_err(|e| HubError::Io(format!("mchose feature write: {e}")))?;
        // L7 prepares the feature response asynchronously. In particular,
        // `mouseAllRead` (0x67) needs roughly 600 ms; reading immediately
        // yields the previous/default feature buffer.
        std::thread::sleep(Duration::from_millis(if sub == 0x67 { 650 } else { 150 }));
        // 応答: get_feature_report は buf[0] に report id を入れて要求
        let mut resp = vec![0u8; len + 1];
        resp[0] = cmd;
        let n = dev
            .get_feature_report(&mut resp)
            .map_err(|e| HubError::Io(format!("mchose feature read: {e}")))?;
        if n < 2 {
            return Err(HubError::Protocol("empty feature report".into()));
        }
        // The payload is XOR-inverted and begins with an echoed subcommand.
        // For example, device read returns:
        //   11 f9 ac ad 4e ... -> 06 53 52 b1 ...
        // Strip the decoded `06`/`67` echo before parsing the actual data.
        let decoded: Vec<u8> = resp[1..n].iter().map(|b| b ^ 0xFF).collect();
        if decoded.first() != Some(&sub) {
            return Err(HubError::Protocol("unexpected MCHOSE feature response".into()));
        }
        Ok(decoded[1..].to_vec())
    }

    /// Send a feature report exactly like MCHOSE HUB's `setCommand`.
    ///
    /// `data[0]` is the command byte (for example `0x40` for DPI). The report
    /// id itself is **not** inverted; all following bytes are XOR-inverted and
    /// padded with `0xFF` to the report size (`0x11` = 20 B, `0x12` = 64 B).
    fn write_report(dev: &hidapi::HidDevice, report: u8, data: &[u8]) -> HubResult<()> {
        let payload_len = if report == 0x11 { 20 } else { 64 };
        if data.is_empty() || data.len() > payload_len {
            return Err(HubError::Invalid("mchose report length".into()));
        }
        let mut out = vec![0xFFu8; payload_len + 1];
        out[0] = report;
        out[1..1 + data.len()].copy_from_slice(data);
        for b in &mut out[1..] {
            *b ^= 0xFF;
        }
        dev.send_feature_report(&out)
            .map_err(|e| HubError::Io(format!("mchose write: {e}")))?;
        std::thread::sleep(Duration::from_millis(80));
        Ok(())
    }

    /// `0x12 0x40` dpiXYSetParser. X and Y stages are identical on the L7.
    fn write_dpi(dev: &hidapi::HidDevice, last: &AllRead) -> HubResult<()> {
        let mut data: Vec<u8> = vec![0x40, last.usb_dpi_index, last.g_dpi_index, 1];
        for &v in &last.dpi {
            data.extend_from_slice(&v.to_le_bytes());
        }
        data.push(last.dpi_sum);
        data.push(0xFF); // diff: no separate Y values
        for &v in &last.dpi {
            data.extend_from_slice(&v.to_le_bytes());
        }
        Self::write_report(dev, 0x12, &data)
    }

    /// `0x11 0x41` reportParser. `free_rate` is the 2.4G/wireless rate.
    fn write_rate(dev: &hidapi::HidDevice, usb_rate: u8, free_rate: u8) -> HubResult<()> {
        Self::write_report(dev, 0x11, &[0x41, usb_rate, free_rate])
    }

    fn parse_all(data: &[u8]) -> AllRead {
        let mut a = AllRead::default();
        if data.len() < 63 {
            return a;
        }
        a.profile_index = data[0];
        // mouseAllReadParser uses `endianness("little")`. In binary-parser this
        // means consecutive bit4 fields are taken from the low nibble first:
        //   byte 0x23 -> gDpiIndex=3 (low), gRateIndex=2 (high).
        // Verified on device: writing usb/g = 0/3 reads back `00 23 23 03`.
        a.g_dpi_index = data[1] & 0x0F;
        a.g_rate_index = (data[1] >> 4) & 0x0F;
        a.usb_dpi_index = data[2] & 0x0F;
        a.usb_rate_index = (data[2] >> 4) & 0x0F;
        // dpi0..dpi5 at offsets 4..16 (u16 LE)
        for i in 0..6usize {
            let off = 4 + i * 2;
            a.dpi[i] = u16::from_le_bytes([data[off], data[off + 1]]);
        }
        a.dpi_sum = data[16];
        a.sensor = data[17];
        a.key_debounce = data[18];
        a.sleep = data[19];
        // rotateVal at 49
        if data.len() > 49 {
            a.rotate_val = data[49];
        }
        // battery/fw not here
        a
    }

    fn parse_device(data: &[u8]) -> (u32, u8, u8, bool) {
        // mouseDeviceReadParser:
        // vid[0..2], pid[2..4], fw[4..8], bitfield[8], battery[9], charge[10].
        let fw = if data.len() >= 8 {
            u32::from_le_bytes([data[4], data[5], data[6], data[7]])
        } else {
            0
        };
        // mouseDeviceReadParser: bit3 connectMode, bit1 connectStatus,
        // bit4 reserved — also little-endian bit order. MCHOSE HUB treats
        // connectMode 0 as wired and 1 as 2.4G.
        let connect_mode = data.get(8).map(|b| b & 0x07 == 1).unwrap_or(false);
        let bat = if data.len() > 9 { data[9] } else { 0 };
        let charge = if data.len() > 10 { data[10] } else { 0 };
        (fw, bat, charge, connect_mode)
    }

    fn parse_version(data: &[u8]) -> Option<String> {
        let len = *data.first()? as usize;
        let bytes = data.get(1..1 + len)?;
        let version = std::str::from_utf8(bytes).ok()?.trim();
        (!version.is_empty()).then(|| version.to_owned())
    }

    // L7 Ultra exposes rateMap8K = ["125", "500", "1000", "2000", "4000", "8000"].
    fn rate_index_to_hz(idx: u8) -> u32 {
        match idx {
            0 => 125,
            1 => 500,
            2 => 1000,
            3 => 2000,
            4 => 4000,
            5 => 8000,
            _ => 1000,
        }
    }
    fn hz_to_rate_index(hz: u32) -> u8 {
        match hz {
            125 => 0,
            500 => 1,
            1000 => 2,
            2000 => 3,
            4000 => 4,
            8000 => 5,
            _ => 2,
        }
    }

    fn firmware_string(fw: u32) -> String {
        format!(
            "{}.{}.{}.{}",
            fw & 0xFF,
            (fw >> 8) & 0xFF,
            (fw >> 16) & 0xFF,
            (fw >> 24) & 0xFF
        )
    }
}

pub fn open(dev: hidapi::HidDevice, mut info: DeviceInfo) -> HubResult<MchoseMouse> {
    // 読み取りで info を埋める
    let dev_ref = &dev;
    // mouseDeviceRead
    let device_data = MchoseMouse::send_feature_read(dev_ref, 0x11, 0x06, 20).unwrap_or_default();
    let (fw, bat, _charge, connect_mode) = MchoseMouse::parse_device(&device_data);
    let version_data = MchoseMouse::send_feature_read(dev_ref, 0x11, 0x04, 20).unwrap_or_default();
    let version = MchoseMouse::parse_version(&version_data);
    let all_data = MchoseMouse::send_feature_read(dev_ref, 0x12, 0x67, 64).unwrap_or_default();
    let mut last_all = MchoseMouse::parse_all(&all_data);
    last_all.fw_version = fw;
    last_all.battery = bat;
    last_all.connect_mode = connect_mode;

    info.firmware = Some(version.unwrap_or_else(|| MchoseMouse::firmware_string(fw)));
    info.battery = Some(Battery {
        percent: Some(bat),
        charging: false,
    });

    let caps = build_caps();
    let mouse = MchoseMouse {
        dev: Mutex::new(dev),
        info: Mutex::new(info),
        caps,
        last_all: Mutex::new(last_all),
    };
    Ok(mouse)
}

fn build_caps() -> Capabilities {
    use section::*;
    let settings = vec![
        SettingSpec::choice(
            "polling_rate",
            PERFORMANCE,
            "ポーリングレート",
            vec![
                ChoiceOpt::new(125, "125"),
                ChoiceOpt::new(500, "500"),
                ChoiceOpt::new(1000, "1K"),
                ChoiceOpt::new(2000, "2K"),
                ChoiceOpt::new(4000, "4K"),
                ChoiceOpt::new(8000, "8K"),
            ],
        )
        .help("接続中のモードの報告頻度 (Hz)"),
        SettingSpec::choice(
            "polling_wireless",
            PERFORMANCE,
            "ポーリングレート (無線)",
            vec![
                ChoiceOpt::new(125, "125"),
                ChoiceOpt::new(500, "500"),
                ChoiceOpt::new(1000, "1K"),
                ChoiceOpt::new(2000, "2K"),
                ChoiceOpt::new(4000, "4K"),
                ChoiceOpt::new(8000, "8K"),
            ],
        )
        .help("無線時の報告頻度 (Hz)"),
        SettingSpec::slider("sleep", PERFORMANCE, "スリープ時間", 0., 30., 1., "分"),
        SettingSpec::slider("debounce_press", CLICK, "クリックデバウンス", 0., 20., 1., "ms"),
    ];
    // Button and macro remapping use separate, unverified L7 commands. Do not
    // advertise controls that this driver cannot safely persist yet.
    let buttons = vec![];
    let button_kinds = vec![];
    Capabilities {
        mouse: Some(MouseCaps {
            dpi_min: 50,
            dpi_max: 42000,
            dpi_step: 50,
            min_stages: 1,
            max_stages: 6,
            xy_split: false,
            stage_colors: true,
            stage_toggle: false,
            buttons,
            button_kinds,
            require_left_click: true,
            macros: None,
            live_buttons: vec![],
            adaptive_dpi: None,
        }),
        keyboard: None,
        settings,
        lighting: vec![LightArea {
            id: 0,
            name: "RGB".into(),
            effects: vec![],
            brightness_max: 4,
            speed_max: 4,
            directions: vec![],
            switchable: true,
            zones: vec![],
            per_key: false,
            direct_drive: false,
            live_readback: false,
        }],
        battery: true,
        peq: None,
        firmware_feed: None,
        profiles: None,
        actions: vec![],
    }
}

impl DeviceDriver for MchoseMouse {
    fn info(&self) -> DeviceInfo {
        self.info.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    fn caps(&self) -> Capabilities {
        self.caps.clone()
    }
    fn read(&self) -> HubResult<DeviceState> {
        let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
        let all_data = Self::send_feature_read(&dev, 0x12, 0x67, 64).unwrap_or_default();
        let device_data = Self::send_feature_read(&dev, 0x11, 0x06, 20).unwrap_or_default();
        let version_data = Self::send_feature_read(&dev, 0x11, 0x04, 20).unwrap_or_default();
        drop(dev);

        let mut last = self.last_all.lock().unwrap_or_else(|p| p.into_inner());
        *last = Self::parse_all(&all_data);
        let (fw, bat, _, connect_mode) = Self::parse_device(&device_data);
        last.fw_version = fw;
        last.battery = bat;
        last.connect_mode = connect_mode;

        // stages
        let n = (last.dpi_sum as usize).clamp(1, 6);
        let mut stages = Vec::new();
        for i in 0..n {
            stages.push(DpiStage {
                x: last.dpi[i] as u32,
                y: None,
                color: Rgb::new(255, 255, 255),
                enabled: true,
            });
        }
        // MCHOSE HUB tracks the selected stage with `usbDpiIndex` (low nibble
        // of allInfo byte 2); `gDpiIndex` is a separate gaming-mode field.
        let active = (last.usb_dpi_index as usize).min(stages.len().saturating_sub(1));

        let buttons = vec![
            ButtonAction::Mouse(MouseFn::Left),
            ButtonAction::Mouse(MouseFn::Right),
            ButtonAction::Mouse(MouseFn::Middle),
            ButtonAction::Mouse(MouseFn::Forward),
            ButtonAction::Mouse(MouseFn::Back),
            ButtonAction::Dpi(DpiFn::Shift(1000)),
        ];

        let macros = vec![None; 5];

        let state = DeviceState {
            profile: ProfileState {
                active: last.profile_index,
                names: vec![],
            },
            mouse: Some(MouseState {
                stages,
                active_stage: active,
                buttons,
                macros,
                adaptive: None,
            }),
            settings: {
                let mut m = std::collections::BTreeMap::new();
                // Observed on L7 Ultra (2.4G): `0x11 0x41 [cmd, usbRate, freeRate]`
                // updates the high nibble of allInfo byte 2 (`usb_rate_index`), and
                // this is the field that reflects the active polling rate after
                // writes. Use it for the reported/verified rate.
                let active_rate = last.usb_rate_index;
                m.insert("polling_rate".into(), SettingValue::Choice(MchoseMouse::rate_index_to_hz(active_rate) as i64));
                m.insert("polling_wireless".into(), SettingValue::Choice(MchoseMouse::rate_index_to_hz(last.usb_rate_index) as i64));
                m.insert("sleep".into(), SettingValue::Num(last.sleep as f32));
                m.insert("debounce_press".into(), SettingValue::Num(last.key_debounce as f32));
                m
            },
            keyboard: None,
            lighting: std::collections::BTreeMap::new(),
            peq: None,
        };

        // update info battery
        {
            let mut info = self.info.lock().unwrap_or_else(|p| p.into_inner());
            info.battery = Some(Battery {
                percent: Some(bat),
                charging: false,
            });
            info.firmware = Some(
                Self::parse_version(&version_data)
                    .unwrap_or_else(|| Self::firmware_string(fw)),
            );
        }

        Ok(state)
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::DpiStages { stages, active } => {
                {
                    let mut last = self.last_all.lock().unwrap_or_else(|p| p.into_inner());
                    let selected = (*active).min(5) as u8;
                    last.usb_dpi_index = selected;
                    last.g_dpi_index = selected;
                    last.dpi_sum = stages.len().min(6) as u8;
                    for (i, s) in stages.iter().take(6).enumerate() {
                        last.dpi[i] = s.x as u16;
                    }
                    let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
                    Self::write_dpi(&dev, &last)?;
                }
                // Verify from the device instead of trusting the write call.
                std::thread::sleep(Duration::from_millis(120));
                let check = self.read()?;
                let stored = check
                    .mouse
                    .ok_or_else(|| HubError::Protocol("missing mouse state".into()))?;
                let expected: Vec<u32> = stages.iter().take(6).map(|s| s.x).collect();
                let actual: Vec<u32> = stored.stages.iter().map(|s| s.x).collect();
                if actual != expected {
                    return Err(HubError::Protocol(format!(
                        "DPI write did not persist (expected {expected:?}, got {actual:?})"
                    )));
                }
                Ok(())
            }
            Change::ActiveStage(idx) => {
                {
                    let mut last = self.last_all.lock().unwrap_or_else(|p| p.into_inner());
                    let selected = (*idx).min(5) as u8;
                    last.usb_dpi_index = selected;
                    last.g_dpi_index = selected;
                    let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
                    Self::write_dpi(&dev, &last)?;
                }
                std::thread::sleep(Duration::from_millis(120));
                let check = self.read()?;
                let stored = check
                    .mouse
                    .ok_or_else(|| HubError::Protocol("missing mouse state".into()))?;
                if stored.active_stage != (*idx).min(5) {
                    return Err(HubError::Protocol(format!(
                        "DPI stage write did not persist (expected {}, got {})",
                        idx, stored.active_stage
                    )));
                }
                Ok(())
            }
            Change::Setting { id, value } => {
                let expected = match id.as_str() {
                    "polling_rate" | "polling_wireless" => {
                        let SettingValue::Choice(v) = value else {
                            return Err(HubError::Invalid("rate must be a choice".into()));
                        };
                        let idx = Self::hz_to_rate_index(*v as u32);
                        {
                            let mut last = self.last_all.lock().unwrap_or_else(|p| p.into_inner());
                            // MCHOSE HUB: `reportParser [0x41, usbRate, freeRate]`.
                            // In 2.4G mode it writes freeRate; in wired it writes usbRate.
                            // On-device verification shows the selected rate is reflected
                            // in allInfo `usb_rate_index` (byte 2, high nibble).
                            let (usb, free) = if id == "polling_wireless" || last.connect_mode {
                                (0, idx)
                            } else {
                                (idx, 0)
                            };
                            last.usb_rate_index = idx;
                            last.g_rate_index = free;
                            let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
                            Self::write_rate(&dev, usb, free)?;
                        }
                        SettingValue::Choice(Self::rate_index_to_hz(idx) as i64)
                    }
                    "sleep" => {
                        let SettingValue::Num(v) = value else {
                            return Err(HubError::Invalid("sleep must be a number".into()));
                        };
                        let v = (*v).clamp(0., 30.) as u8;
                        {
                            let mut last = self.last_all.lock().unwrap_or_else(|p| p.into_inner());
                            last.sleep = v;
                            let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
                            Self::write_report(
                                &dev,
                                0x11,
                                &[0x0A, if v > 0 { 1 } else { 0 }, v],
                            )?;
                        }
                        SettingValue::Num(v as f32)
                    }
                    "debounce_press" => {
                        let SettingValue::Num(v) = value else {
                            return Err(HubError::Invalid("debounce must be a number".into()));
                        };
                        let v = (*v).clamp(0., 20.) as u8;
                        {
                            let mut last = self.last_all.lock().unwrap_or_else(|p| p.into_inner());
                            last.key_debounce = v;
                            let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
                            Self::write_report(&dev, 0x11, &[0x43, v])?;
                        }
                        SettingValue::Num(v as f32)
                    }
                    _ => return Err(HubError::Unsupported("setting")),
                };
                std::thread::sleep(Duration::from_millis(120));
                let check = self.read()?;
                let actual = check
                    .settings
                    .get(id)
                    .ok_or_else(|| HubError::Protocol("setting missing after write".into()))?;
                if *actual == expected {
                    Ok(())
                } else {
                    Err(HubError::Protocol(format!(
                        "{id} write did not persist (expected {expected:?}, got {actual:?})"
                    )))
                }
            }
            Change::Lighting { .. } => Err(HubError::Unsupported("lighting")),
            Change::Button { .. } => Err(HubError::Unsupported("button")),
            Change::Macro { .. } => Err(HubError::Unsupported("macro")),
            _ => Err(HubError::Unsupported("change")),
        }
    }

    fn battery(&self) -> HubResult<Battery> {
        let last = self.last_all.lock().unwrap_or_else(|p| p.into_inner());
        Ok(Battery {
            percent: Some(last.battery),
            charging: false,
        })
    }
}

// end of module
