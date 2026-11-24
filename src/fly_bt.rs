//! Bluetooth flyout (same macOS style as fly_wifi.rs): radio switch, connected/paired devices,
//! nearby available devices discovered via Bluetooth inquiry scan, and link to Bluetooth Settings.

use std::sync::{Arc, Mutex};

use windows::Devices::Radios::RadioState;
use windows::Win32::Devices::Bluetooth::*;

use crate::fly_wifi::spawn;
use crate::flyout::{white, Cmd, Content, Ui, ACCENT4};
use crate::sysctl;

#[derive(Clone)]
struct BtDevice {
    name: String,
    connected: bool,
    remembered: bool,
}

#[derive(Default)]
struct State {
    loaded: bool,
    available: bool,
    on: bool,
    devices: Vec<BtDevice>,
    scanning: bool,
    status: String,
    busy: bool,
}

const ID_SWITCH: u32 = 1;
const ID_SCAN: u32 = 2;
const ID_SETTINGS: u32 = 3;
const ID_DEV: u32 = 100;
const ROW_H: f32 = 36.0;
const LIST_MAX: f32 = 320.0;

pub struct BluetoothFly {
    hwnd: isize,
    st: Arc<Mutex<State>>,
    scroll: usize,
}

fn wide_str(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n]).trim().to_string()
}

/// Enumerate Bluetooth devices. `inquiry = false` returns paired/connected devices immediately (< 5 ms);
/// `inquiry = true` also discovers nearby unpaired devices (~2.5 s).
fn enumerate(inquiry: bool) -> Vec<BtDevice> {
    let mut out = Vec::new();
    let params = BLUETOOTH_DEVICE_SEARCH_PARAMS {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
        fReturnAuthenticated: true.into(),
        fReturnRemembered: true.into(),
        fReturnUnknown: inquiry.into(),
        fReturnConnected: true.into(),
        fIssueInquiry: inquiry.into(),
        cTimeoutMultiplier: if inquiry { 2 } else { 0 },
        hRadio: windows::Win32::Foundation::HANDLE::default(),
    };
    let mut info = BLUETOOTH_DEVICE_INFO {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let Ok(h) = BluetoothFindFirstDevice(&params, &mut info) else {
            return out;
        };
        loop {
            let name = wide_str(&info.szName);
            if !name.is_empty() {
                let connected = info.fConnected.as_bool();
                let remembered = info.fRemembered.as_bool() || info.fAuthenticated.as_bool();
                if let Some(existing) = out.iter_mut().find(|d: &&mut BtDevice| d.name.eq_ignore_ascii_case(&name)) {
                    existing.connected |= connected;
                    existing.remembered |= remembered;
                } else {
                    out.push(BtDevice { name, connected, remembered });
                }
            }
            if BluetoothFindNextDevice(h, &mut info).is_err() {
                break;
            }
        }
        let _ = BluetoothFindDeviceClose(h);
    }
    out.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| b.remembered.cmp(&a.remembered))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

fn reload(st: &Mutex<State>, inquiry: bool) {
    let radio = sysctl::bluetooth();
    let Some(r) = radio else {
        let mut s = st.lock().unwrap();
        s.loaded = true;
        s.available = false;
        s.on = false;
        s.devices.clear();
        s.scanning = false;
        s.status = "Bluetooth unavailable".into();
        return;
    };
    let on = r.State().is_ok_and(|x| x == RadioState::On);
    {
        let mut s = st.lock().unwrap();
        s.loaded = true;
        s.available = true;
        s.on = on;
        if !on {
            s.devices.clear();
            s.scanning = false;
            s.status = "Bluetooth is off".into();
            return;
        }
        s.status.clear();
    }
    // Fast pass first so paired/connected devices show up immediately
    let fast = enumerate(false);
    {
        let mut s = st.lock().unwrap();
        s.devices = fast;
        if inquiry {
            s.scanning = true;
            s.status = "Scanning for devices…".into();
        }
    }
    if inquiry {
        let full = enumerate(true);
        let mut s = st.lock().unwrap();
        s.devices = full;
        s.scanning = false;
        if s.devices.is_empty() {
            s.status = "No Bluetooth devices found".into();
        } else {
            s.status.clear();
        }
    }
}

impl BluetoothFly {
    pub fn new() -> BluetoothFly {
        BluetoothFly {
            hwnd: 0,
            st: Arc::new(Mutex::new(State { available: true, on: true, ..Default::default() })),
            scroll: 0,
        }
    }

    fn run(&self, f: impl FnOnce(&Mutex<State>) + Send + 'static) {
        let st = self.st.clone();
        {
            let mut s = st.lock().unwrap();
            if s.busy {
                return;
            }
            s.busy = true;
        }
        spawn(self.hwnd, move || {
            f(&st);
            st.lock().unwrap().busy = false;
        });
    }
}

impl Content for BluetoothFly {
    fn width(&self) -> f32 {
        304.0
    }

    fn opened(&mut self, hwnd: isize) {
        self.hwnd = hwnd;
        self.run(|st| reload(st, true));
    }

    fn wheel(&mut self, delta: i32) {
        let n = self.st.lock().unwrap().devices.len();
        let max = n.saturating_sub((LIST_MAX / ROW_H) as usize);
        if delta < 0 {
            self.scroll = (self.scroll + 2).min(max);
        } else {
            self.scroll = self.scroll.saturating_sub(2);
        }
    }

    fn click(&mut self, id: u32, _fx: f32) -> Cmd {
        match id {
            ID_SWITCH => {
                let (avail, want) = {
                    let mut s = self.st.lock().unwrap();
                    if !s.available {
                        return Cmd::None;
                    }
                    s.on = !s.on;
                    s.status = if s.on { "Turning on…".into() } else { "Bluetooth is off".into() };
                    if !s.on {
                        s.devices.clear();
                    }
                    (s.available, s.on)
                };
                if avail {
                    self.run(move |st| {
                        if let Some(r) = sysctl::bluetooth() {
                            sysctl::set_radio(&r, want);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(if want { 800 } else { 150 }));
                        reload(st, want);
                    });
                }
            }
            ID_SCAN => {
                self.run(|st| reload(st, true));
            }
            ID_SETTINGS => return Cmd::Launch("ms-settings:bluetooth".into()),
            i if i >= ID_DEV => {
                // Open Windows Bluetooth settings directly to connect / pair the chosen device.
                return Cmd::Launch("ms-settings:bluetooth".into());
            }
            _ => {}
        }
        Cmd::None
    }

    fn draw(&mut self, ui: &mut Ui) -> f32 {
        let st = self.st.lock().unwrap();
        let w = ui.w;
        let mut y = 2.0;

        // Header + switch
        ui.text_mid(2.0, y + 12.0, "Bluetooth", 14.0, 600, white(0xF5), w - 50.0);
        ui.switch(ID_SWITCH, w - 38.0, y + 2.0, st.available && st.on);
        y += 30.0;
        ui.divider(y);
        y += 8.0;

        if !st.status.is_empty() && st.devices.is_empty() {
            let h = ui.text_wrap(4.0, y, &st.status, 12.0, 400, white(0x8C), w - 8.0);
            y += h + 8.0;
        }

        if st.on && !st.devices.is_empty() {
            let header = if st.scanning { "Devices · Scanning…" } else { "Devices" };
            ui.text(4.0, y, header, 11.5, 600, white(0x88), w - 54.0);
            if !st.scanning && !st.busy {
                let sw = ui.text_w("Scan", 11.5, 500) + 12.0;
                let hov = ui.row(ID_SCAN, w - sw, y - 3.0, sw, 20.0);
                ui.text(w - sw + 6.0, y, "Scan", 11.5, 500, if hov { white(0xFF) } else { ACCENT4 }, 0.0);
            }
            y += 20.0;

            let max_rows = (LIST_MAX / ROW_H) as usize;
            let start = self.scroll.min(st.devices.len().saturating_sub(1));
            for (idx, d) in st.devices.iter().enumerate().skip(start).take(max_rows) {
                ui.row(ID_DEV + idx as u32, 0.0, y, w, ROW_H);
                let dot = if d.connected { ACCENT4 } else { white(0x28) };
                ui.circle(16.0, y + ROW_H / 2.0, 12.0, dot);
                ui.glyph(9.5, y + ROW_H / 2.0, '\u{E702}', 12.0, white(if d.connected { 0xFF } else { 0xC8 }));
                let sub = if d.connected {
                    "Connected"
                } else if d.remembered {
                    "Paired"
                } else {
                    "Available"
                };
                ui.text_mid(36.0, y + 12.5, &d.name, 13.0, 500, white(0xF0), w - 44.0);
                ui.text_mid(36.0, y + 26.5, sub, 11.0, 400, if d.connected { ACCENT4 } else { white(0x80) }, w - 44.0);
                y += ROW_H;
            }
            y += 4.0;
        } else if st.on && !st.loaded {
            ui.text(4.0, y, "Loading…", 12.0, 400, white(0x8C), w - 8.0);
            y += 22.0;
        }

        ui.divider(y);
        y += 6.0;
        ui.row(ID_SETTINGS, 0.0, y, w, 30.0);
        ui.text_mid(6.0, y + 15.0, "Bluetooth Settings…", 12.5, 400, white(0xD9), w - 12.0);
        y + 30.0
    }
}
