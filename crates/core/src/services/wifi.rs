//! The built-in Wi-Fi for wireless Android Auto. While wireless mode is on,
//! the Wi-Fi is a 5 GHz hotspot; otherwise NetworkManager keeps it on the saved
//! home network. The choice is kept across restarts, and the home network comes
//! back whenever NavBridge stops, as the hotspot is no use without it.
use crate::settings::Settings;
use navbridge_common::WirelessSnapshot;
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read},
    net::Ipv4Addr,
    process::Command,
    sync::{
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc, RwLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

/// The hotspot's NetworkManager profile. It never connects on its own, so
/// after a crash or a reboot the Wi-Fi is back on the home network.
const PROFILE: &str = "navbridge-hotspot";
const INTERFACE: &str = "wlan0";
/// 5 GHz, and usable in the EU without radar detection.
const CHANNEL: &str = "36";
/// The hotspot's own address, where phones open the Android Auto session.
/// They get theirs over DHCP.
pub const HOTSPOT_IP: Ipv4Addr = Ipv4Addr::new(10, 42, 0, 1);
const ADDRESS: &str = "10.42.0.1/24";
const REFRESH: Duration = Duration::from_secs(10);
/// How long the home network may take to reconnect, read every second.
const SETTLE: Duration = Duration::from_secs(20);
/// A hotspot that failed or dropped is started again after this long, doubled
/// up to `RETRY_MAX` while it keeps failing.
const RETRY: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(300);

/// What a phone needs to join the hotspot.
pub struct Hotspot {
    pub address: Ipv4Addr,
    pub ssid: String,
    pub key: String,
    pub bssid: String,
}

/// The hotspot's details as NetworkManager has them.
pub fn hotspot() -> Result<Hotspot, String> {
    let nmcli = Nmcli(vec!["nmcli".into()]);
    let fields = "802-11-wireless.ssid,802-11-wireless-security.psk";
    let values = nmcli.run(&["-s", "-t", "-g", fields, "connection", "show", PROFILE])?;
    let mut lines = values.lines();
    let (Some(ssid), Some(key)) = (lines.next(), lines.next()) else {
        return Err("the hotspot has no network name or password".into());
    };
    // While the hotspot is up, the interface's address is its BSSID.
    let mac = fs::read_to_string(format!("/sys/class/net/{INTERFACE}/address"))
        .map_err(|e| format!("read {INTERFACE} address: {e}"))?;
    Ok(Hotspot {
        address: HOTSPOT_IP,
        ssid: ssid.into(),
        key: key.into(),
        bssid: mac.trim().into(),
    })
}

/// The name NavBridge goes by on Wi-Fi and Bluetooth, such as NavBridge-984B.
pub fn unit_name() -> String {
    hotspot_ssid(
        &fs::read_to_string(format!("/sys/class/net/{INTERFACE}/address")).unwrap_or_default(),
    )
}

/// Whether the hotspot is up, as the Wi-Fi thread last saw it: for a phone's
/// Bluetooth setup, which must not send it to the hotspot any sooner.
#[derive(Clone, Default)]
pub struct HotspotWatch(Arc<RwLock<WirelessSnapshot>>);

impl HotspotWatch {
    /// Waits up to `limit` for the hotspot; returns whether it is up.
    pub fn wait(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while !self.0.read().unwrap().hotspot {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
        true
    }
}

pub struct WifiService {
    requests: Option<Sender<bool>>,
    shared: Arc<RwLock<WirelessSnapshot>>,
    worker: Option<JoinHandle<()>>,
}

impl WifiService {
    /// With `wireless` (`--wireless`), wireless mode starts on whatever the
    /// saved choice; the saved choice itself only changes from the UI.
    pub fn start(wireless: bool, settings: Arc<Settings>) -> Self {
        Self::with(Nmcli(vec!["nmcli".into()]), settings, wireless)
    }

    fn with(nmcli: Nmcli, settings: Arc<Settings>, wireless: bool) -> Self {
        let saved = settings.load().wireless_android_auto;
        if wireless && !saved {
            info!("wireless Android Auto on from the command line");
        }
        let enabled = wireless || saved;
        let shared = Arc::new(RwLock::new(WirelessSnapshot {
            enabled,
            hotspot: false,
            status: String::new(),
        }));
        let (requests, received) = mpsc::channel();
        let wifi = Wifi {
            nmcli,
            shared: shared.clone(),
            settings,
            enabled,
            ssid: None,
            connection: None,
            error: None,
            attempt: None,
            retry: RETRY,
            settle_until: None,
        };
        let worker = thread::Builder::new()
            .name("wifi".into())
            .spawn(move || wifi.run(received))
            .expect("start the Wi-Fi thread");
        Self {
            requests: Some(requests),
            shared,
            worker: Some(worker),
        }
    }

    /// Turns wireless mode on or off; the Wi-Fi follows in the background.
    pub fn set_enabled(&self, enabled: bool) {
        {
            let mut shared = self.shared.write().unwrap();
            shared.enabled = enabled;
            shared.status = if enabled {
                "Starting the hotspot…"
            } else {
                "Returning to the home Wi-Fi…"
            }
            .into();
        }
        if let Some(requests) = &self.requests {
            let _ = requests.send(enabled);
        }
    }

    pub fn snapshot(&self) -> WirelessSnapshot {
        self.shared.read().unwrap().clone()
    }

    pub fn watch(&self) -> HotspotWatch {
        HotspotWatch(self.shared.clone())
    }
}

impl Drop for WifiService {
    fn drop(&mut self) {
        // The worker then gives the Wi-Fi back to the home network.
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Wifi {
    nmcli: Nmcli,
    shared: Arc<RwLock<WirelessSnapshot>>,
    settings: Arc<Settings>,
    /// The latest choice: while it is on, the hotspot is kept up.
    enabled: bool,
    ssid: Option<String>,
    /// The NetworkManager connection on the interface at the last refresh.
    connection: Option<String>,
    /// Why the hotspot last failed to start.
    error: Option<String>,
    attempt: Option<Instant>,
    retry: Duration,
    settle_until: Option<Instant>,
}

impl Wifi {
    fn run(mut self, requests: Receiver<bool>) {
        if self.enabled {
            self.hotspot();
        }
        self.refresh();
        loop {
            let settling = self.settle_until.is_some_and(|end| Instant::now() < end);
            let wait = if settling {
                Duration::from_secs(1)
            } else {
                REFRESH
            };
            match requests.recv_timeout(wait) {
                Ok(mut enabled) => {
                    // Only the latest choice matters.
                    while let Ok(next) = requests.try_recv() {
                        enabled = next;
                    }
                    self.enabled = enabled;
                    self.retry = RETRY;
                    self.settings
                        .update(|saved| saved.wireless_android_auto = enabled);
                    if enabled {
                        if self.connection.as_deref() != Some(PROFILE) {
                            self.hotspot();
                        }
                    } else {
                        self.home();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    let due = self.attempt.is_none_or(|at| at.elapsed() >= self.retry);
                    if self.enabled && self.connection.as_deref() != Some(PROFILE) && due {
                        self.retry = (self.retry * 2).min(RETRY_MAX);
                        self.hotspot();
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    if self.active().as_deref() == Some(PROFILE) {
                        self.home();
                    }
                    return;
                }
            }
            self.refresh();
        }
    }

    fn hotspot(&mut self) {
        self.attempt = Some(Instant::now());
        self.status("Starting the hotspot…".into());
        let started = self.profile().and_then(|ssid| {
            self.nmcli.run(&["-w", "30", "connection", "up", PROFILE])?;
            Ok(ssid)
        });
        match started {
            Ok(ssid) => {
                info!(%ssid, "wireless Android Auto hotspot on");
                // A phone's setup may be waiting: tell it before the refresh.
                self.shared.write().unwrap().hotspot = true;
                self.error = None;
                self.retry = RETRY;
            }
            Err(e) => {
                warn!(%e, "wireless Android Auto hotspot failed");
                self.error = Some(e);
            }
        }
    }

    /// Takes the hotspot down; NetworkManager then reconnects the saved
    /// network it would choose at boot.
    fn home(&mut self) {
        match self.nmcli.run(&["-w", "30", "connection", "down", PROFILE]) {
            Ok(_) => info!("wireless Android Auto hotspot off"),
            Err(e) => debug!(%e, "hotspot was not on"),
        }
        self.error = None;
        self.settle_until = Some(Instant::now() + SETTLE);
    }

    /// The hotspot's network name, creating its profile on first use.
    fn profile(&mut self) -> Result<String, String> {
        let existing = [
            "-t",
            "-g",
            "802-11-wireless.ssid",
            "connection",
            "show",
            PROFILE,
        ];
        if let Ok(ssid) = self.nmcli.run(&existing) {
            let ssid = ssid.trim().to_string();
            self.ssid = Some(ssid.clone());
            return Ok(ssid);
        }
        let mac = fs::read_to_string(format!("/sys/class/net/{INTERFACE}/address"));
        let ssid = hotspot_ssid(&mac.unwrap_or_default());
        // The passphrase is on nmcli's command line for this one call: only
        // local users could see it, and the phone gets it over Bluetooth.
        let password = password().map_err(|e| format!("hotspot password: {e}"))?;
        self.nmcli.run(&profile_args(&ssid, &password))?;
        info!(%ssid, "created the wireless Android Auto hotspot");
        self.ssid = Some(ssid.clone());
        Ok(ssid)
    }

    /// The NetworkManager connection on the interface, if any.
    fn active(&self) -> Option<String> {
        let args = [
            "-t",
            "-g",
            "GENERAL.CONNECTION",
            "device",
            "show",
            INTERFACE,
        ];
        let name = self.nmcli.run(&args).ok()?;
        let name = name.trim();
        (!name.is_empty()).then(|| name.to_string())
    }

    fn refresh(&mut self) {
        self.connection = self.active();
        let hotspot = self.connection.as_deref() == Some(PROFILE);
        let home = match self.connection.as_deref() {
            Some(name) if !hotspot => {
                let args = [
                    "-t",
                    "-g",
                    "802-11-wireless.ssid",
                    "connection",
                    "show",
                    name,
                ];
                self.nmcli
                    .run(&args)
                    .ok()
                    .map(|ssid| ssid.trim().to_string())
            }
            _ => None,
        };
        if home.is_some() {
            self.settle_until = None;
        }
        let settling = self.settle_until.is_some_and(|end| Instant::now() < end);
        let ssid = self.ssid.as_deref().unwrap_or("NavBridge");
        let status = describe(
            self.enabled,
            hotspot,
            ssid,
            home.as_deref(),
            self.error.as_deref(),
            settling,
        );
        let mut shared = self.shared.write().unwrap();
        shared.hotspot = hotspot;
        shared.status = status;
    }

    fn status(&self, status: String) {
        self.shared.write().unwrap().status = status;
    }
}

fn describe(
    enabled: bool,
    hotspot: bool,
    ssid: &str,
    home: Option<&str>,
    error: Option<&str>,
    settling: bool,
) -> String {
    match (enabled, hotspot) {
        (true, true) => format!("Hotspot {ssid} is on (5 GHz, channel {CHANNEL})"),
        (true, false) => match error {
            Some(e) => format!("Hotspot failed: {e}"),
            None => "Starting the hotspot…".into(),
        },
        (false, true) => "Returning to the home Wi-Fi…".into(),
        (false, false) => match home {
            Some(name) => format!("Home Wi-Fi: {name}"),
            None if settling => "Returning to the home Wi-Fi…".into(),
            None => "Wi-Fi not connected".into(),
        },
    }
}

fn profile_args(ssid: &str, password: &str) -> Vec<String> {
    [
        "connection",
        "add",
        "type",
        "wifi",
        "ifname",
        INTERFACE,
        "con-name",
        PROFILE,
        "autoconnect",
        "no",
        "ssid",
        ssid,
        "mode",
        "ap",
        "802-11-wireless.band",
        "a",
        "802-11-wireless.channel",
        CHANNEL,
        "wifi-sec.key-mgmt",
        "wpa-psk",
        "wifi-sec.proto",
        "rsn",
        "wifi-sec.pairwise",
        "ccmp",
        "wifi-sec.group",
        "ccmp",
        "wifi-sec.psk",
        password,
        // NetworkManager's DHCP server hands phones an address.
        "ipv4.method",
        "shared",
        "ipv4.addresses",
        ADDRESS,
        "ipv6.method",
        "disabled",
    ]
    .map(String::from)
    .to_vec()
}

/// "NavBridge-984B" from the interface's MAC address, so units differ.
fn hotspot_ssid(mac: &str) -> String {
    let hex: String = mac.chars().filter(char::is_ascii_hexdigit).collect();
    match hex.get(8..) {
        Some(tail) if hex.len() == 12 => format!("NavBridge-{}", tail.to_uppercase()),
        _ => "NavBridge".into(),
    }
}

/// A random WPA2 passphrase of 20 letters and digits; nobody types it.
fn password() -> io::Result<String> {
    const SYMBOLS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut random = [0u8; 64];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    // Bytes below 248 (4 x 62) keep every symbol equally likely.
    let password: String = random
        .iter()
        .filter(|&&byte| byte < 248)
        .take(20)
        .map(|&byte| SYMBOLS[byte as usize % SYMBOLS.len()] as char)
        .collect();
    if password.len() < 20 {
        return Err(io::Error::other("not enough random bytes"));
    }
    Ok(password)
}

/// NetworkManager's command line. Tests substitute a script.
struct Nmcli(Vec<OsString>);

impl Nmcli {
    fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<String, String> {
        let output = Command::new(&self.0[0])
            .args(&self.0[1..])
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .map_err(|e| format!("nmcli: {e}"))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("nmcli failed");
        Err(message.strip_prefix("Error: ").unwrap_or(message).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hotspot_is_a_5_ghz_wpa2_network_that_never_starts_on_its_own() {
        let args = profile_args("NavBridge-984B", "secret");
        let value = |key: &str| {
            let at = args.iter().position(|arg| arg == key).unwrap();
            args[at + 1].as_str()
        };
        assert_eq!(value("autoconnect"), "no");
        assert_eq!(value("mode"), "ap");
        assert_eq!(value("802-11-wireless.band"), "a");
        assert_eq!(value("802-11-wireless.channel"), "36");
        assert_eq!(value("wifi-sec.key-mgmt"), "wpa-psk");
        assert_eq!(value("wifi-sec.psk"), "secret");
        assert_eq!(value("ipv4.method"), "shared");
        assert!(ADDRESS.starts_with(&format!("{HOTSPOT_IP}/")));
        assert_eq!(hotspot_ssid("2c:cf:67:e1:98:4b\n"), "NavBridge-984B");
        assert_eq!(hotspot_ssid(""), "NavBridge");
        let (first, second) = (password().unwrap(), password().unwrap());
        assert_eq!(first.len(), 20);
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(first, second);
    }

    #[test]
    fn the_status_says_what_the_wifi_does() {
        let on = describe(true, true, "NavBridge-984B", None, None, false);
        assert_eq!(on, "Hotspot NavBridge-984B is on (5 GHz, channel 36)");
        let failed = describe(
            true,
            false,
            "NavBridge",
            None,
            Some("No suitable device"),
            false,
        );
        assert_eq!(failed, "Hotspot failed: No suitable device");
        let home = describe(false, false, "", Some("Freebox"), None, false);
        assert_eq!(home, "Home Wi-Fi: Freebox");
        let settling = describe(false, false, "", None, None, true);
        assert_eq!(settling, "Returning to the home Wi-Fi…");
        let away = describe(false, false, "", None, None, false);
        assert_eq!(away, "Wi-Fi not connected");
    }

    /// Stands in for nmcli: logs each call and keeps the interface's
    /// connection in a file, as NetworkManager would.
    const FAKE_NMCLI: &str = r#"cd "$(dirname "$0")"
echo "$*" >> calls
case "$*" in
  "-t -g GENERAL.CONNECTION device show wlan0") cat active ;;
  "-t -g 802-11-wireless.ssid connection show home") echo "Home Net" ;;
  "-t -g 802-11-wireless.ssid connection show navbridge-hotspot")
    if [ -f created ]; then echo NavBridge-TEST; else
      echo "Error: navbridge-hotspot - no such connection profile." >&2; exit 10; fi ;;
  "connection add "*) touch created ;;
  "-w 30 connection up navbridge-hotspot") echo navbridge-hotspot > active ;;
  "-w 30 connection down navbridge-hotspot") echo home > active ;;
  *) echo "Error: unexpected $*" >&2; exit 2 ;;
esac
"#;

    fn wait_for(wifi: &WifiService, done: impl Fn(&WirelessSnapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(&wifi.snapshot()) {
            assert!(Instant::now() < deadline, "stuck at {:?}", wifi.snapshot());
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn wireless_mode_switches_the_wifi_and_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("navbridge-wifi-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("nmcli"), FAKE_NMCLI).unwrap();
        fs::write(dir.join("active"), "home\n").unwrap();
        let start = || {
            let script = dir.join("nmcli").into_os_string();
            let nmcli = Nmcli(vec!["sh".into(), script]);
            let settings = Arc::new(Settings::new(dir.join("state/settings.toml")));
            WifiService::with(nmcli, settings, false)
        };
        let hotspot_on = |s: &WirelessSnapshot| s.status.starts_with("Hotspot NavBridge");
        let at_home = |s: &WirelessSnapshot| s.status == "Home Wi-Fi: Home Net";

        // Off by default: the home network is left alone.
        let wifi = start();
        wait_for(&wifi, at_home);
        assert!(!wifi.snapshot().enabled);
        wifi.set_enabled(true);
        assert!(wifi.snapshot().enabled);
        wait_for(&wifi, hotspot_on);
        assert!(wifi.snapshot().hotspot);
        wifi.set_enabled(false);
        wait_for(&wifi, at_home);
        assert!(!wifi.snapshot().hotspot);
        wifi.set_enabled(true);
        wait_for(&wifi, hotspot_on);
        // Stopping NavBridge gives the Wi-Fi back to the home network...
        drop(wifi);
        assert_eq!(fs::read_to_string(dir.join("active")).unwrap(), "home\n");
        // ...and the next start brings the hotspot back.
        let wifi = start();
        assert!(wifi.snapshot().enabled);
        wait_for(&wifi, hotspot_on);
        drop(wifi);

        let calls = fs::read_to_string(dir.join("calls")).unwrap();
        assert_eq!(calls.matches("connection add ").count(), 1, "{calls}");
        assert_eq!(calls.matches("connection up navbridge-hotspot").count(), 3);
        assert_eq!(
            calls.matches("connection down navbridge-hotspot").count(),
            3
        );
        assert!(!calls.contains("unexpected"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_wireless_flag_turns_wireless_mode_on_without_saving_it() {
        let dir =
            std::env::temp_dir().join(format!("navbridge-wifi-flag-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("nmcli"), FAKE_NMCLI).unwrap();
        fs::write(dir.join("active"), "home\n").unwrap();
        let path = dir.join("state/settings.toml");
        let settings = Arc::new(Settings::new(&path));
        let start = |wireless| {
            let script = dir.join("nmcli").into_os_string();
            let nmcli = Nmcli(vec!["sh".into(), script]);
            WifiService::with(nmcli, settings.clone(), wireless)
        };
        let hotspot_on = |s: &WirelessSnapshot| s.status.starts_with("Hotspot NavBridge");
        let at_home = |s: &WirelessSnapshot| s.status == "Home Wi-Fi: Home Net";

        // Nothing saved: the flag alone brings the hotspot up...
        let wifi = start(true);
        assert!(wifi.snapshot().enabled);
        assert!(wifi.watch().wait(Duration::from_secs(5)));
        wait_for(&wifi, hotspot_on);
        drop(wifi);
        // ...but a start without it keeps the saved choice.
        assert!(!path.exists());
        let wifi = start(false);
        wait_for(&wifi, at_home);
        assert!(!wifi.snapshot().enabled);
        assert!(!wifi.watch().wait(Duration::from_millis(100)));
        drop(wifi);
        // The UI can still turn it off; the flag wins again at the next start.
        let wifi = start(true);
        wait_for(&wifi, hotspot_on);
        wifi.set_enabled(false);
        wait_for(&wifi, at_home);
        drop(wifi);
        assert!(!settings.load().wireless_android_auto);
        let wifi = start(true);
        assert!(wifi.snapshot().enabled);
        wait_for(&wifi, hotspot_on);
        drop(wifi);
        fs::remove_dir_all(dir).unwrap();
    }
}
