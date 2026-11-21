//! HM66 (Sikakeyb Castle) — CDC serial プロトコル (Stage A: 読み取り中心)。
//!
//! デバイスは COM ポート (9600 8N1) 越しに 32B フレームをやり取りする。
//! - 読み: `17 01 00 83 ...` (32B) → 設定読み取り
//! - 応答は同じヘッダ + 32B ペイロード。
//! Stage A では接続・読み取り・live depth のみ。書き込み (apply) は未実装。

use devhub_core::backend::*;
use devhub_core::model::*;
use devhub_core::registry;
use std::sync::Mutex;

pub struct Hm66Keyboard {
    port: Mutex<crate::serial::SerialPort>,
    info: Mutex<DeviceInfo>,
    caps: Capabilities,
    #[allow(dead_code)]
    last_frame: Mutex<Vec<u8>>,
}

pub fn open(info: DeviceInfo) -> HubResult<Hm66Keyboard> {
    // info.serial から COM ポート名を取る
    let port_name = info
        .serial
        .clone()
        .unwrap_or_else(|| "COM6".into());
    let sp = crate::serial::SerialPort::open(&port_name, 9600)?;
    let caps = build_caps();
    Ok(Hm66Keyboard {
        port: Mutex::new(sp),
        info: Mutex::new(info),
        caps,
        last_frame: Mutex::new(Vec::new()),
    })
}

fn build_caps() -> Capabilities {
    Capabilities {
        settings: vec![],
        actions: vec![],
        profiles: None,
        mouse: None,
        keyboard: Some(KeyboardCaps {
            layout: registry::su66max_layout(),
            layers: vec!["Base".into(), "Fn".into()],
            catalog: vec![],
            combos: false,
            perf: None,
            advanced: vec![],
            macros: None,
        }),
        lighting: vec![],
        battery: false,
        peq: None,
        firmware_feed: None,
    }
}

impl DeviceDriver for Hm66Keyboard {
    fn info(&self) -> DeviceInfo {
        self.info.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    fn caps(&self) -> Capabilities {
        self.caps.clone()
    }
    fn read(&self) -> HubResult<DeviceState> {
        let mut port = self.port.lock().unwrap_or_else(|p| p.into_inner());
        // 読み取りコマンド送信 (Stage A: 設定読み取り)
        let mut cmd = [0u8; 32];
        cmd[0] = 0x17;
        cmd[1] = 0x01;
        cmd[2] = 0x00;
        cmd[3] = 0x83;
        port.write_all(&cmd)?;
        // 応答待ち
        let mut buf = [0u8; 32];
        let mut got = 0usize;
        let start = std::time::Instant::now();
        while got < 32 && start.elapsed().as_millis() < 500 {
            match port.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(_) => break,
            }
        }
        drop(port);
        // Stage A: 応答があれば保持・保存（payload は未デコード）
        if got > 0 {
            let header_ok =
                got >= 4 && buf[0] == 0x17 && buf[1] == 0x01 && buf[2] == 0x00 && buf[3] == 0x83;
            if header_ok {
                if let Ok(mut lf) = self.last_frame.lock() {
                    lf.clear();
                    lf.extend_from_slice(&buf[..got]);
                }
            }
            // 解析材料用: 読取専用で最新フレームをhex保存（失敗は無視）
            if std::env::var_os("DEVHUB_HM66_DEBUG").is_some() {
                let hex = buf[..got]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                let _ = std::fs::write(
                    r"C:\Users\nemuliz\AppData\Local\Temp\opencode\hm66_last_frame.hex",
                    format!("len={got}\nheader_match={header_ok}\n{hex}\n"),
                );
            }
        }
        // バッテリー等は応答から取れない (HM66 はバッテリー無し有線)。空の state を返す。
        Ok(DeviceState {
            profile: ProfileState { active: 0, names: vec![] },
            mouse: None,
            keyboard: Some(KeyboardState {
                keymap: vec![],
                perf: vec![],
                advanced: vec![],
                macros: vec![],
            }),
            settings: std::collections::BTreeMap::new(),
            lighting: std::collections::BTreeMap::new(),
            peq: None,
        })
    }
    fn apply(&self, _change: &Change) -> HubResult<()> {
        Err(HubError::NotImplemented("hm66 apply (stage A)"))
    }
    fn battery(&self) -> HubResult<Battery> {
        Err(HubError::Unsupported("battery"))
    }
}
