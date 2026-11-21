//! devhub-hid: 実機との会話層。
//!
//! - `connect()` … hidapi を開いて `RealHub` を返す
//! - `RealHub::scan` … `devhub_core::registry::KNOWN` に一致したインターフェースを
//!   `DeviceInfo` として報告し、ID → HID path を `paths` に記録する
//!   (HID で開けないドライバ種別は `supported: false`)。
//! - `RealHub::driver` … ID から記録済み path を開き、ドライバを作ってキャッシュする。
//!   同じ ID には同じ `Arc` を返す (UI はデバイスごとのワーカー 1 本で呼ぶ)。

use devhub_core::backend::*;
use devhub_core::model::*;
use devhub_core::registry;
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Arc, Mutex};

// mousefit 新プロトコル (64B feature report) ドライバ — 観測した全機種 (5219 有線・
// 3854 ドングル) は Windows で feature report を持たないため現状どの機種にも振らない。
// JS バンドル解析に基づく実装として残し、feature report を持つ機種が出たらここを使う。
/// ATK 系ドライバ。診断 example (atkread) からも使うので公開。
#[doc(hidden)]
pub mod atk;
#[doc(hidden)]
pub mod lamzu;
#[allow(dead_code)]
mod mousefit;
mod mousefit8;
mod mchose;
mod serial;
mod hm66;
/// 生 HID チャネル。診断 example (axraw 等) が直接フレームを投げるために公開。
#[doc(hidden)]
pub mod transport;
mod walkplay;
mod xsyd;

/// 実機バックエンドの入口。HID が開けなければ Err (UI はデモへ落ちる)。
pub fn connect() -> HubResult<Arc<dyn HubBackend>> {
    let api = transport::open()?;
    Ok(Arc::new(RealHub {
        api: Mutex::new(api),
        paths: Mutex::new(HashMap::new()),
        drivers: Mutex::new(HashMap::new()),
        creating: Mutex::new(HashMap::new()),
        names: Mutex::new(HashMap::new()),
        watcher: Mutex::new(None),
        detected: Arc::new(Mutex::new(HashMap::new())),
    }))
}

/// ドライバ種別ごとの実装が済んでいるか。
fn driver_ready(kind: DriverKind) -> bool {
    match kind {
        DriverKind::Xsyd => true,
        DriverKind::Mousefit => true,
        DriverKind::Walkplay => true,
        DriverKind::Atk => true,
        DriverKind::Lamzu => true,
        DriverKind::Mchose => true,
        DriverKind::Hm66 => true,
        DriverKind::Unsupported => false,
    }
}

pub struct RealHub {
    api: Mutex<hidapi::HidApi>,
    /// `DeviceId.0` → HID path (最後の scan 時点)。
    paths: Mutex<HashMap<String, CString>>,
    /// `DeviceId.0` → ドライバ (オープン済みは使い回す)。
    drivers: Mutex<HashMap<String, Arc<dyn DeviceDriver>>>,
    /// `DeviceId.0` → 生成ロック。同じ ID のドライバ生成を直列化する
    /// (キャッシュ確認と登録が別区間だと並行呼び出しが同じ HID を二重に開く)。
    creating: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// `DeviceId.0` → HID が報告する製品名 (scan 時点で記録)。
    names: Mutex<HashMap<String, String>>,
    /// `DeviceId.0` → ドングル内側機種の確定値 (model, 表示名)。
    /// ドライバが EID で確定した結果を保持し、ドライバ再生成
    /// (切断/invalidate) でも registry 既定へ戻らないようにする。
    detected: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// `watch` で登録された通知窓口 (ドライバへ渡す)。
    watcher: Mutex<Option<Watcher>>,
}

/// ドライバへ渡すイベント窓口。
type Watcher = Arc<dyn Fn(HubEvent) + Send + Sync>;

impl HubBackend for RealHub {
    fn scan(&self) -> HubResult<Vec<DeviceInfo>> {
        let mut api = self.api.lock().unwrap_or_else(|p| p.into_inner());
        api.refresh_devices()
            .map_err(|e| HubError::Io(e.to_string()))?;
        let mut out: Vec<DeviceInfo> = Vec::new();
        let mut paths = self.paths.lock().unwrap_or_else(|p| p.into_inner());
        let mut names = self.names.lock().unwrap_or_else(|p| p.into_inner());
        let prev_paths = std::mem::take(&mut *paths);
        names.clear();
        for d in api.device_list() {
            let Some(spec) =
                registry::match_device(d.vendor_id(), d.product_id(), d.usage_page(), d.usage())
            else {
                continue;
            };
            let serial = d
                .serial_number()
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let id = DeviceId(format!(
                "hid:{:04x}:{:04x}:{}",
                d.vendor_id(),
                d.product_id(),
                serial
                    .clone()
                    .unwrap_or_else(|| format!("{:08x}", fxhash(d.path().to_bytes())))
            ));
            if out.iter().any(|o| o.id == id) {
                continue;
            }
            paths.insert(id.0.clone(), d.path().to_owned());
            // 表示名は HID の製品文字列を優先 (無ければ registry の名前)。
            // ただしドングルは受信機自身の名前を名乗るので、registry (ペア機種名) を優先する。
            let name = match spec.transport {
                Transport::Wireless24G | Transport::Bluetooth => spec.name.into(),
                _ => d
                    .product_string()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| spec.name.into()),
            };
            names.insert(id.0.clone(), name.clone());
            out.push(DeviceInfo {
                id,
                kind: spec.kind,
                name,
                vendor: spec.vendor.into(),
                model: spec.model.into(),
                vid: spec.vid,
                pid: spec.pid,
                transport: spec.transport,
                firmware: None,
                dongle_firmware: None,
                serial,
                driver: spec.driver,
                battery: None,
                supported: driver_ready(spec.driver),
                asleep: false,
                board_id: None,
            });
        }
        // ---------------------------------------------------------------- シリアル (HM66 等)
        // HID 列挙では見えない CDC デバイス。Windows レジストリから
        // VID_C019&PID_1234 に紐づく COM ポートのみを候補にし、Stage A の
        // 読み取りプローブを試して登録する（読取のみ・無関係な COM への書き込みを避ける）。
        let hm66_candidates = hm66_candidate_ports();
        let mut hm66_log = format!("candidates={hm66_candidates:?}\n");
        for port in hm66_candidates {
            let id = DeviceId(format!("serial:0c19:1234:{}", port));
            // ドライバがポートを排他オープン保持している間は再プローブすると失敗し、
            // スキャン結果から消えて点滅するため、キャッシュ済みならそのまま維持する。
            let cached = self
                .drivers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains_key(&id.0);
            let same_path = prev_paths
                .get(&id.0)
                .map(|p| p.to_string_lossy() == port)
                .unwrap_or(false);
            if cached && same_path {
                paths.insert(id.0.clone(), std::ffi::CString::new(port.clone()).unwrap());
                if !out.iter().any(|o| o.id == id) {
                    out.push(DeviceInfo {
                        id: id.clone(),
                        kind: DeviceKind::Keyboard,
                        name: "HM66".into(),
                        vendor: "Sikakeyb".into(),
                        model: "hm66".into(),
                        vid: 0x0C19,
                        pid: 0x1234,
                        transport: Transport::Wired,
                        firmware: None,
                        dongle_firmware: None,
                        serial: Some(port.clone()),
                        driver: DriverKind::Hm66,
                        battery: None,
                        supported: true,
                        asleep: false,
                        board_id: None,
                    });
                }
                hm66_log.push_str(&format!("{port}: cached driver kept\n"));
                continue;
            }
            // 同一スキャン内で重複しないよう保険
            if paths.values().any(|p| p.to_string_lossy() == port) {
                continue;
            }
            // プローブ: VID/PID 一致の候補ポートに Stage A の読取コマンドを試し、
            // 応答有無に関わらず登録する（実機で無応答ケースあり。読取のみ・未検証）。
            match serial::SerialPort::open(&port, 9600) {
                Ok(mut sp) => {
                    hm66_log.push_str(&format!("{port}: open ok\n"));
                    // 送信: 0x17 0x01 0x00 0x83 + 28 bytes 0x00 (32 bytes)
                    let mut cmd = [0u8; 32];
                    cmd[0] = 0x17;
                    cmd[1] = 0x01;
                    cmd[2] = 0x00;
                    cmd[3] = 0x83;
                    let wrote = sp.write_all(&cmd).is_ok();
                    hm66_log.push_str(&format!("{port}: probe write={wrote}\n"));
                    // 応答待ち (最大 ~250ms)
                    let mut buf = [0u8; 32];
                    let mut got = 0usize;
                    let start = std::time::Instant::now();
                    while got < 32 && start.elapsed().as_millis() < 250 {
                        match sp.read(&mut buf[got..]) {
                            Ok(0) => break,
                            Ok(k) => got += k,
                            Err(_) => break,
                        }
                    }
                    hm66_log.push_str(&format!("{port}: probe bytes={got}\n"));
                    if got > 0 && std::env::var_os("DEVHUB_HM66_DEBUG").is_some() {
                        let hex = buf[..got]
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ");
                        let _ = std::fs::write(
                            r"C:\Users\nemuliz\AppData\Local\Temp\opencode\hm66_probe_frame.hex",
                            format!("port={port}\nlen={got}\n{hex}\n"),
                        );
                    }
                    if out.iter().any(|o| o.id == id) {
                        continue;
                    }
                    paths.insert(id.0.clone(), std::ffi::CString::new(port.clone()).unwrap());
                    out.push(DeviceInfo {
                        id,
                        kind: DeviceKind::Keyboard,
                        name: "HM66".into(),
                        vendor: "Sikakeyb".into(),
                        model: "hm66".into(),
                        vid: 0x0C19,
                        pid: 0x1234,
                        transport: Transport::Wired,
                        firmware: None,
                        dongle_firmware: None,
                        serial: Some(port.clone()),
                        driver: DriverKind::Hm66,
                        battery: None,
                        supported: true,
                        asleep: false,
                        board_id: None,
                    });
                    hm66_log.push_str(&format!("{port}: registered\n"));
                }
                Err(e) => {
                    hm66_log.push_str(&format!("{port}: open failed: {e}\n"));
                }
            }
        }
        if std::env::var_os("DEVHUB_HM66_DEBUG").is_some() {
            let _ = std::fs::write(
                r"C:\Users\nemuliz\AppData\Local\Temp\opencode\hm66_scan.log",
                hm66_log,
            );
        }
        // 抜かれた/差し直されたデバイスのキャッシュは死んだハンドル — パスが
        // 前回と一致するものだけ残し、他は捨てて次回 driver() で開き直す。
        self.drivers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|id, _| paths.get(id).is_some() && prev_paths.get(id) == paths.get(id));
        Ok(out)
    }

    fn watch(&self, cb: Box<dyn Fn(HubEvent) + Send + Sync>) {
        *self.watcher.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::from(cb));
    }

    fn driver(&self, id: &DeviceId) -> HubResult<Arc<dyn DeviceDriver>> {
        // シリアル (CDC) デバイスは HID パスを持たない。
        if id.0.starts_with("serial:") {
            if let Some(d) = self
                .drivers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&id.0)
            {
                return Ok(d.clone());
            }
            // シリアルデバイスの info は scan 時点で組み立て済み (paths には port 名)。
            let port = self
                .paths
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&id.0)
                .cloned()
                .map(|c| c.to_string_lossy().to_string())
                .ok_or_else(|| HubError::NotFound(id.clone()))?;
            // id: serial:<vid>:<pid>:<port>
            let info = DeviceInfo {
                id: id.clone(),
                kind: DeviceKind::Keyboard,
                name: "HM66".into(),
                vendor: "Sikakeyb".into(),
                model: "hm66".into(),
                vid: 0x0C19,
                pid: 0x1234,
                transport: Transport::Wired,
                firmware: None,
                dongle_firmware: None,
                serial: Some(port.clone()),
                driver: DriverKind::Hm66,
                battery: None,
                supported: true,
                asleep: false,
                board_id: None,
            };
            let drv = Arc::new(hm66::open(info)?);
            self.drivers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id.0.clone(), drv.clone());
            return Ok(drv);
        }
        if let Some(d) = self
            .drivers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id.0)
        {
            return Ok(d.clone());
        }
        // 同じ ID の生成を直列化する。別 ID の通信はこのロックを待たない。
        let guard = self
            .creating
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(id.0.clone())
            .or_default()
            .clone();
        let _guard = guard.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(d) = self
            .drivers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id.0)
        {
            return Ok(d.clone());
        }
        let (vid, pid) = parse_id(id).ok_or_else(|| HubError::NotFound(id.clone()))?;
        let spec = known_spec(vid, pid).ok_or_else(|| HubError::NotFound(id.clone()))?;
        // 列挙の更新と生成が重なると、切れた古いパスで開いたドライバを
        // 登録してしまう — 登録前にパス世代を再確認し、変わっていればやり直す。
        for _ in 0..3 {
            let path = self
                .paths
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&id.0)
                .cloned()
                .ok_or_else(|| HubError::NotFound(id.clone()))?;
            let api = self.api.lock().unwrap_or_else(|p| p.into_inner());
            let dev = match api.open_path(&path) {
                Ok(d) => d,
                Err(e) => return Err(HubError::Io(format!("open {}: {e}", spec.name))),
            };
            drop(api);
            let drv = self.make_driver(spec, dev, id)?;
            let same = self
                .paths
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&id.0)
                == Some(&path);
            if same {
                self.drivers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(id.0.clone(), drv.clone());
                return Ok(drv);
            }
            // パスが更新された — 古い接続先で開いたハンドルは捨てて取り直す。
        }
        Err(HubError::Io(
            "デバイスの接続先が変化しています — 再試行してください".into(),
        ))
    }

    fn invalidate(&self, id: &DeviceId) {
        self.drivers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id.0);
    }
}

impl RealHub {
    /// ドライバ 1 回分の生成 (info/detected/events 解決 + registry 分岐)。
    /// 登録は呼び出し側がパス世代の再確認後に行う。
    fn make_driver(
        &self,
        spec: &registry::KnownDevice,
        dev: hidapi::HidDevice,
        id: &DeviceId,
    ) -> HubResult<Arc<dyn DeviceDriver>> {
        let name = self
            .names
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id.0)
            .cloned()
            .unwrap_or_else(|| spec.name.into());
        let mut info = DeviceInfo {
            id: id.clone(),
            kind: spec.kind,
            name,
            vendor: spec.vendor.into(),
            model: spec.model.into(),
            vid: spec.vid,
            pid: spec.pid,
            transport: spec.transport,
            firmware: None,
            dongle_firmware: None,
            serial: None,
            driver: spec.driver,
            battery: None,
            supported: true,
            asleep: false,
            board_id: None,
        };
        // ドングルの内側機種が確定済みなら復元 (再生成でも正しい caps で開く)。
        if let Some((m, n)) = self
            .detected
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id.0)
        {
            info.model = m.clone();
            info.name = n.clone();
        }
        let events = self
            .watcher
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let drv: Arc<dyn DeviceDriver> = match spec.driver {
            // mouse.fit 機種はドングル経由だけでなく有線 (5219 実機) でも
            // feature report を持たない → 全機種 report-8/EEPROM 経路。
            // 新プロトコル (64B feature report) のドライバは mousefit.rs に残してある。
            DriverKind::Mousefit => {
                Arc::new(mousefit8::open(dev, info, events, self.detected.clone())?)
            }
            DriverKind::Xsyd => Arc::new(xsyd::open(dev, info)?),
            DriverKind::Walkplay => Arc::new(walkplay::open(dev, info)?),
            DriverKind::Atk => Arc::new(atk::open(dev, info, events)?),
            DriverKind::Lamzu => Arc::new(lamzu::open(dev, info, events)?),
            DriverKind::Mchose => Arc::new(mchose::open(dev, info)?),
            DriverKind::Hm66 => {
                return Err(HubError::NotImplemented("hm66 is serial-only"))
            }
            DriverKind::Unsupported => {
                return Err(HubError::NotImplemented("このデバイスのドライバ"))
            }
        };
        Ok(drv)
    }
}

/// HM66 (VID 0xC019 PID 0x1234) に対応する COM ポート候補を Windows レジストリから列挙する。
/// 見つからない/読めない場合は空（無関係な COM ポートをプローブしない）。
fn hm66_candidate_ports() -> Vec<String> {
    let mut cmd = std::process::Command::new("reg");
    cmd.args([
        "query",
        r"HKLM\SYSTEM\CurrentControlSet\Enum\USB",
        "/s",
        "/v",
        "PortName",
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current_key = String::new();
    let mut ports = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.starts_with("HKEY_") {
            current_key = trimmed.to_ascii_uppercase();
            continue;
        }
        if !current_key.contains("VID_C019&PID_1234") {
            continue;
        }
        let Some(pos) = line.find("REG_SZ") else {
            continue;
        };
        let val = line[pos + "REG_SZ".len()..].trim();
        if val.starts_with("COM") && !ports.iter().any(|p| p == val) {
            ports.push(val.to_string());
        }
    }
    ports
}

/// `hid:<vid>:<pid>:<serial>` → (vid, pid)。
fn parse_id(id: &DeviceId) -> Option<(u16, u16)> {
    let mut it = id.0.split(':');
    if it.next()? != "hid" {
        return None;
    }
    let vid = u16::from_str_radix(it.next()?, 16).ok()?;
    let pid = u16::from_str_radix(it.next()?, 16).ok()?;
    Some((vid, pid))
}

/// VID/PID → registry エントリ (usage 条件は scan 時点で満たした前提)。
fn known_spec(vid: u16, pid: u16) -> Option<&'static registry::KnownDevice> {
    registry::KNOWN
        .iter()
        .find(|k| k.vid == vid && k.pid == pid && k.vid != 0xFFFF)
}

/// パス文字列の安定ハッシュ (シリアルが無いデバイスの ID 用)。
fn fxhash(b: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for x in b {
        h ^= *x as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}
