//! MCHOSE HUB 系マウス — MCHOSE L7 Ultra 等。
//!
//! www.mchose.com.cn の WebHID プロトコル (JS 解析) を Rust に移植。
//!
//! - 設定チャネル: vendor usage page 0xFF0B / usage 0x0104、64B feature report
//!   (report id 0 先頭に 0x11/0x12 コマンド) で読み出し、
//!   output report id 0x4D (77) の構造化バッファで書き込み。
//! - 読み: `0x11 0x06` (mouseDeviceRead) / `0x12 0x67` (mouseAllRead) /
//!   `0x11 0x1b` (mouseLightRead)。応答は feature report で同期的に返る。
//! - 書き: `getCommandBuffer` 形式 (flags/len/cmdId/bizCode/seq/data/xor) を
//!   output report 0x4D で送る。コマンド: 0x0102 (allInfo), 0x0103 (dpi),
//!   0x0104 (single dpi), 0x0109 (lod), 0x010a (adaptive power)。

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
}

fn build_all_info(last: &AllRead) -> Vec<u8> {
    // allInfoToArr layout
    let mut data: Vec<u8> = Vec::new();
    data.push(last.profile_index);
    let aa = if last.usb_rate_index > 0 {
        last.usb_rate_index + 1
    } else {
        last.usb_rate_index
    };
    let ra = if last.g_rate_index > 0 {
        last.g_rate_index + 1
    } else {
        last.g_rate_index
    };
    let oa = ((aa as u8) & 0x0F) << 4 | (last.usb_dpi_index & 0x0F);
    let ia = ((ra as u8) & 0x0F) << 4 | (last.usb_dpi_index & 0x0F);
    data.push(oa);
    data.push(ia);
    data.push(last.sleep);
    data.push(0); // sleepMode
    data.push(last.sensor);
    data.push(last.rotate_val);
    data.push(last.key_debounce); // leftKeyDebounce
    data.push(last.key_debounce); // rightKeyDebounce
    for _ in 0..10 {
        data.push(0);
    }
    data
}

impl MchoseMouse {
    fn send_feature_read(dev: &hidapi::HidDevice, cmd: u8, sub: u8) -> HubResult<Vec<u8>> {
        // body = [cmd, sub^0xFF, 0xFF, 0xFF, ...] (64B) — cmd が report id、
        // サブコマンド以降は XOR 0xFF で反転して送る (JS の readCommand と同じ)。
        let mut buf = [0xFFu8; 64];
        buf[0] = cmd;
        buf[1] = sub ^ 0xFF;
        dev.send_feature_report(&buf)
            .map_err(|e| HubError::Io(format!("mchose feature write: {e}")))?;
        // 応答: get_feature_report は buf[0] に report id を入れて要求
        let mut resp = [0u8; 65];
        resp[0] = cmd;
        let n = dev
            .get_feature_report(&mut resp)
            .map_err(|e| HubError::Io(format!("mchose feature read: {e}")))?;
        if n < 2 {
            return Err(HubError::Protocol("empty feature report".into()));
        }
        // resp[0] = report id, resp[1..] = data
        Ok(resp[1..].to_vec())
    }

    fn write_report(dev: &hidapi::HidDevice, cmd_id: u16, data: &[u8]) -> HubResult<()> {
        // getCommandBuffer 形式 (64B 全体、report id 0x4D)
        let mut buf = [0u8; 64];
        buf[0] = 1; // flags (na)
        buf[1] = data.len() as u8; // length
        buf[2] = (cmd_id & 0xFF) as u8; // cmdId low
        buf[3] = ((cmd_id >> 8) & 0xFF) as u8; // cmdId high
        buf[4] = 0; // bizCode
        buf[5] = 0; // sequence
        let n = data.len().min(57);
        buf[6..6 + n].copy_from_slice(&data[..n]);
        // xor checksum: indices 2..(7+n) inclusive in the 64-byte buffer
        // = buf[2] ^ buf[3] ^ ... ^ buf[6+n-1]? 
        // calculateXorSum(ia, 2, 7+ta.length): ia[2..=7+ta.length]
        // ia index = buffer index. ta.length = data.len(). 7+ta.length = 7+data.len().
        // inclusive range 2..=7+data.len() → indices 2,3,...,7+data.len()
        let mut xor: u8 = 0;
        for i in 2..=(7 + data.len()) {
            xor ^= buf[i];
        }
        buf[6 + data.len()] = xor;
        // L7 Ultra の書き込みは feature report (report id 0x12, 64B) を使う。
        // 書き込みデータは反転 (XOR 0xFF) で送る (キャプチャで確認)。
        // レポートサイズは 64B (report id + 63B data)。65B 送ると WriteFile が失敗する。
        let mut out = [0u8; 64];
        out[0] = 0x12;
        for (i, b) in buf.iter().enumerate().take(63) {
            out[1 + i] = b ^ 0xFF;
        }
        dev.send_feature_report(&out)
            .map_err(|e| HubError::Io(format!("mchose write: {e}")))?;
        // 少し待つ (デバイス処理)
        std::thread::sleep(Duration::from_millis(50));
        Ok(())
    }

    fn parse_all(data: &[u8]) -> AllRead {
        let mut a = AllRead::default();
        if data.len() < 63 {
            return a;
        }
        a.profile_index = data[0];
        a.g_dpi_index = (data[1] >> 4) & 0x0F;
        a.g_rate_index = data[1] & 0x0F;
        a.usb_dpi_index = (data[2] >> 4) & 0x0F;
        a.usb_rate_index = data[2] & 0x0F;
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

    fn parse_device(data: &[u8]) -> (u32, u8, u8) {
        // fwVersion u32 LE at offset 4, batteryLevel at 9, chargeStatus at 10
        let fw = if data.len() >= 8 {
            u32::from_le_bytes([data[4], data[5], data[6], data[7]])
        } else {
            0
        };
        let bat = if data.len() > 9 { data[9] } else { 0 };
        let charge = if data.len() > 10 { data[10] } else { 0 };
        (fw, bat, charge)
    }

    fn rate_index_to_hz(idx: u8) -> u32 {
        match idx {
            0 => 125,
            1 => 250,
            2 => 500,
            3 => 1000,
            4 => 2000,
            5 => 4000,
            6 => 8000,
            _ => 1000,
        }
    }
    fn hz_to_rate_index(hz: u32) -> u8 {
        match hz {
            125 => 0,
            250 => 1,
            500 => 2,
            1000 => 3,
            2000 => 4,
            4000 => 5,
            8000 => 6,
            _ => 3,
        }
    }
}

pub fn open(dev: hidapi::HidDevice, mut info: DeviceInfo) -> HubResult<MchoseMouse> {
    // 読み取りで info を埋める
    let dev_ref = &dev;
    // mouseDeviceRead
    let device_data = MchoseMouse::send_feature_read(dev_ref, 0x11, 0x06).unwrap_or_default();
    let (fw, bat, _charge) = MchoseMouse::parse_device(&device_data);
    let all_data = MchoseMouse::send_feature_read(dev_ref, 0x12, 0x67).unwrap_or_default();
    let mut last_all = MchoseMouse::parse_all(&all_data);
    last_all.fw_version = fw;
    last_all.battery = bat;

    info.firmware = Some(format!("{}.{}", fw >> 16, fw & 0xFFFF));
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
            "ポーリングレート (有線)",
            vec![
                ChoiceOpt::new(125, "125"),
                ChoiceOpt::new(250, "250"),
                ChoiceOpt::new(500, "500"),
                ChoiceOpt::new(1000, "1K"),
                ChoiceOpt::new(2000, "2K"),
                ChoiceOpt::new(4000, "4K"),
                ChoiceOpt::new(8000, "8K"),
            ],
        )
        .help("マウス→PC 間の報告頻度 (Hz)"),
        SettingSpec::choice(
            "polling_wireless",
            PERFORMANCE,
            "ポーリングレート (無線)",
            vec![
                ChoiceOpt::new(125, "125"),
                ChoiceOpt::new(250, "250"),
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
        SettingSpec::slider("sensor_angle", SENSOR, "センサー角度", -30., 30., 1., "°"),
    ];
    let buttons = vec![
        ButtonSlot { name: "左ボタン".into() },
        ButtonSlot { name: "右ボタン".into() },
        ButtonSlot { name: "中ボタン".into() },
        ButtonSlot { name: "サイド前".into() },
        ButtonSlot { name: "サイド後".into() },
        ButtonSlot { name: "DPI ボタン".into() },
    ];
    let button_kinds = vec![
        ButtonActionKind::Disabled,
        ButtonActionKind::Mouse,
        ButtonActionKind::Dpi,
        ButtonActionKind::Key,
        ButtonActionKind::Media,
        ButtonActionKind::Macro,
    ];
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
            macros: Some(MacroCaps {
                slots: 5,
                max_events: 100,
                max_delay_ms: 10000,
                modes: vec![],
                repeat_modes: vec![],
                names: true,
                mouse_events: true,
            }),
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
        let all_data = Self::send_feature_read(&dev, 0x12, 0x67).unwrap_or_default();
        let device_data = Self::send_feature_read(&dev, 0x11, 0x06).unwrap_or_default();
        drop(dev);

        let mut last = self.last_all.lock().unwrap_or_else(|p| p.into_inner());
        *last = Self::parse_all(&all_data);
        let (fw, bat, _) = Self::parse_device(&device_data);
        last.fw_version = fw;
        last.battery = bat;

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
        let active = (last.g_dpi_index as usize).min(stages.len().saturating_sub(1));

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
                m.insert("polling_rate".into(), SettingValue::Choice(MchoseMouse::rate_index_to_hz(last.g_rate_index) as i64));
                m.insert("polling_wireless".into(), SettingValue::Choice(MchoseMouse::rate_index_to_hz(last.usb_rate_index) as i64));
                m.insert("sleep".into(), SettingValue::Num(last.sleep as f32));
                m.insert("debounce_press".into(), SettingValue::Num(last.key_debounce as f32));
                m.insert("sensor_angle".into(), SettingValue::Num(last.rotate_val as f32));
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
            info.firmware = Some(format!("{}.{}", fw >> 16, fw & 0xFFFF));
        }

        Ok(state)
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::DpiStages { stages, active: _ } => {
                let mut last = self.last_all.lock().unwrap_or_else(|p| p.into_inner());
                last.dpi_sum = stages.len() as u8;
                for (i, s) in stages.iter().enumerate() {
                    if i < 6 {
                        last.dpi[i] = s.x as u16;
                    }
                }
                // build dpiToArr data: profileIndex, dpiDirection=0, dpiHasY=0, dpiSum, usbDpiIndex, dpi0..dpi5
                let mut data: Vec<u8> = Vec::new();
                data.push(last.profile_index);
                data.push(0); // dpiDirection
                data.push(0); // dpiHasY
                data.push(last.dpi_sum);
                data.push(last.usb_dpi_index);
                for i in 0..6usize {
                    let v = last.dpi[i];
                    data.push((v & 0xFF) as u8);
                    data.push(((v >> 8) & 0xFF) as u8);
                }
                let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
                Self::write_report(&dev, 0x0103, &data)?;
                Ok(())
            }
            Change::ActiveStage(idx) => {
                let mut last = self.last_all.lock().unwrap_or_else(|p| p.into_inner());
                last.g_dpi_index = *idx as u8;
                // send allInfo
                let data = build_all_info(&last);
                let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
                Self::write_report(&dev, 0x0102, &data)?;
                Ok(())
            }
            Change::Setting { id, value } => {
                let mut last = self.last_all.lock().unwrap_or_else(|p| p.into_inner());
                match id.as_str() {
                    "polling_rate" => {
                        if let SettingValue::Choice(v) = value {
                            let idx = Self::hz_to_rate_index(*v as u32);
                            last.g_rate_index = idx;
                        }
                    }
                    "polling_wireless" => {
                        if let SettingValue::Choice(v) = value {
                            let idx = Self::hz_to_rate_index(*v as u32);
                            last.usb_rate_index = idx;
                        }
                    }
                    "sleep" => {
                        if let SettingValue::Num(v) = value {
                            last.sleep = *v as u8;
                        }
                    }
                    "debounce_press" => {
                        if let SettingValue::Num(v) = value {
                            last.key_debounce = *v as u8;
                        }
                    }
                    "sensor_angle" => {
                        if let SettingValue::Num(v) = value {
                            last.rotate_val = *v as u8;
                        }
                    }
                    _ => return Err(HubError::Unsupported("setting")),
                }
                let data = build_all_info(&last);
                let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
                Self::write_report(&dev, 0x0102, &data)?;
                Ok(())
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
