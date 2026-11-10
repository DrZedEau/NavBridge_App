//! The built-in Wi-Fi for wireless Android Auto. While wireless mode is on,
//! the Wi-Fi is a 5 GHz hotspot; otherwise NetworkManager keeps it on the saved
//! home network. The choice is kept across restarts, and the home network comes
//! back whenever NavBridge stops, as the hotspot is no use without it.
use crate::config::WirelessConfig;
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
/// Its interface and channel are core.toml's `[wireless]`.
const PROFILE: &str = "navbridge-hotspot";
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

/// The Wi-Fi interface's own address, as "2c:cf:67:e1:98:4b".
fn mac_address(interface: &str) -> io::Result<String> {
    fs::read_to_string(format!("/sys/class/net/{interface}/address"))
}

/// The hotspot for a phone's Bluetooth setup: whether it is up, as the Wi-Fi
/// thread last saw it, since the setup must not send the phone there any
/// sooner, and then its details.
#[derive(Clone, Default)]
pub struct HotspotWatch {
    shared: Arc<RwLock<WirelessSnapshot>>,
    /// The Wi-Fi interface, from core.toml.
    interface: String,
}

impl HotspotWatch {
    /// The hotspot's details as NetworkManager has them.
    pub fn details(&self) -> Result<Hotspot, String> {
        let nmcli = Nmcli(vec!["nmcli".into()]);
        let fields = "802-11-wireless.ssid,802-11-wireless-security.psk";
        let values = nmcli.run(&["-s", "-t", "-g", fields, "connection", "show", PROFILE])?;
        let mut lines = values.lines();
        let (Some(ssid), Some(key)) = (lines.next(), lines.next()) else {
            return Err("the hotspot has no network name or password".into());
        };
        // While the hotspot is up, the interface's address is its BSSID.
        let mac = mac_address(&self.interface)
            .map_err(|e| format!("read {} address: {e}", self.interface))?;
        Ok(Hotspot {
            address: HOTSPOT_IP,
            ssid: ssid.into(),
            key: key.into(),
            bssid: mac.trim().into(),
        })
    }

    /// The name NavBridge goes by on Wi-Fi and Bluetooth, such as
    /// NavBridge-984B.
    pub fn unit_name(&self) -> String {
        hotspot_ssid(&mac_address(&self.interface).unwrap_or_default())
    }

    /// Waits up to `limit` for the hotspot; returns whether it is up.
    pub fn wait(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while !self.shared.read().unwrap().hotspot {
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
    interface: String,
    worker: Option<JoinHandle<()>>,
}

impl WifiService {
    /// With `wireless` (`--wireless`), wireless mode starts on whatever the
    /// saved choice; the saved choice itself only changes from the UI. The
    /// hotspot runs on `config`'s interface and channel.
    pub fn start(wireless: bool, settings: Arc<Settings>, config: WirelessConfig) -> Self {
        Self::with(Nmcli(vec!["nmcli".into()]), settings, wireless, config)
    }

    fn with(nmcli: Nmcli, settings: Arc<Settings>, wireless: bool, config: WirelessConfig) -> Self {
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
        let interface = config.interface.clone();
        let wifi = Wifi {
            nmcli,
            config,
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
            interface,
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
        HotspotWatch {
            shared: self.shared.clone(),
            interface: self.interface.clone(),
        }
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
    /// The interface and channel of the hotspot, from core.toml.
    config: WirelessConfig,
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
            self.follow_config();
            return Ok(ssid);
        }
        let ssid = hotspot_ssid(&mac_address(&self.config.interface).unwrap_or_default());
        // The passphrase is on nmcli's command line for this one call: only
        // local users could see it, and the phone gets it over Bluetooth.
        let password = password().map_err(|e| format!("hotspot password: {e}"))?;
        self.nmcli
            .run(&profile_args(&self.config, &ssid, &password))?;
        info!(%ssid, "created the wireless Android Auto hotspot");
        self.ssid = Some(ssid.clone());
        Ok(ssid)
    }

    /// Moves a profile created earlier to core.toml's interface and channel,
    /// if they have changed since. Its network name and password stay.
    fn follow_config(&self) {
        let (interface, channel) = (&self.config.interface, self.config.channel.to_string());
        let fields = "connection.interface-name,802-11-wireless.channel";
        let Ok(kept) = self
            .nmcli
            .run(&["-t", "-g", fields, "connection", "show", PROFILE])
        else {
            return;
        };
        // In whichever order nmcli lists them: a name is never a number.
        let kept: Vec<&str> = kept.lines().map(str::trim).collect();
        if kept.contains(&interface.as_str()) && kept.contains(&channel.as_str()) {
            return;
        }
        let changed = [
            "connection",
            "modify",
            PROFILE,
            "connection.interface-name",
            interface,
            "802-11-wireless.channel",
            &channel,
        ];
        match self.nmcli.run(&changed) {
            Ok(_) => info!(%interface, %channel, "the hotspot follows core.toml"),
            Err(e) => {
                warn!(%e, %interface, %channel, "the hotspot keeps its old interface and channel")
            }
        }
    }

    /// The NetworkManager connection on the interface, if any.
    fn active(&self) -> Option<String> {
        let args = [
            "-t",
            "-g",
            "GENERAL.CONNECTION",
            "device",
            "show",
            &self.config.interface,
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
            self.config.channel,
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
    channel: u32,
    home: Option<&str>,
    error: Option<&str>,
    settling: bool,
) -> String {
    match (enabled, hotspot) {
        (true, true) => format!("Hotspot {ssid} is on (5 GHz, channel {channel})"),
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

fn profile_args(config: &WirelessConfig, ssid: &str, password: &str) -> Vec<String> {
    let channel = config.channel.to_string();
    [
        "connection",
        "add",
        "type",
        "wifi",
        "ifname",
        &config.interface,
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
        &channel,
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
        let args = profile_args(&WirelessConfig::default(), "NavBridge-984B", "secret");
        let value = |key: &str| {
            let at = args.iter().position(|arg| arg == key).unwrap();
            args[at + 1].as_str()
        };
        assert_eq!(value("ifname"), "wlan0");
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
        let on = describe(true, true, "NavBridge-984B", 36, None, None, false);
        assert_eq!(on, "Hotspot NavBridge-984B is on (5 GHz, channel 36)");
        let other = describe(true, true, "NavBridge-984B", 149, None, None, false);
        assert_eq!(other, "Hotspot NavBridge-984B is on (5 GHz, channel 149)");
        let failed = describe(
            true,
            false,
            "NavBridge",
            36,
            None,
            Some("No suitable device"),
            false,
        );
        assert_eq!(failed, "Hotspot failed: No suitable device");
        let home = describe(false, false, "", 36, Some("Freebox"), None, false);
        assert_eq!(home, "Home Wi-Fi: Freebox");
        let settling = describe(false, false, "", 36, None, None, true);
        assert_eq!(settling, "Returning to the home Wi-Fi…");
        let away = describe(false, false, "", 36, None, None, false);
        assert_eq!(away, "Wi-Fi not connected");
    }

    /// Stands in for nmcli: logs each call and keeps the interface's
    /// connection in a file, as NetworkManager would. Its hotspot profile is
    /// on wlan0 and channel 36.
    const FAKE_NMCLI: &str = r#"cd "$(dirname "$0")"
echo "$*" >> calls
case "$*" in
  "-t -g GENERAL.CONNECTION device show wlan0") cat active ;;
  "-t -g GENERAL.CONNECTION device show wlan1") cat active ;;
  "-t -g connection.interface-name,802-11-wireless.channel connection show navbridge-hotspot")
    printf 'wlan0\n36\n' ;;
  "connection modify navbridge-hotspot "*) ;;
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
            WifiService::with(nmcli, settings, false, WirelessConfig::default())
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
        // The profile is where core.toml puts it: left as it is.
        assert!(!calls.contains("connection modify"), "{calls}");
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
            WifiService::with(nmcli, settings.clone(), wireless, WirelessConfig::default())
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

    #[test]
    fn a_hotspot_created_earlier_follows_core_toml() {
        let dir =
            std::env::temp_dir().join(format!("navbridge-wifi-move-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("nmcli"), FAKE_NMCLI).unwrap();
        fs::write(dir.join("active"), "home\n").unwrap();
        // The profile exists, on wlan0 and channel 36.
        fs::write(dir.join("created"), "").unwrap();
        let script = dir.join("nmcli").into_os_string();
        let settings = Arc::new(Settings::new(dir.join("state/settings.toml")));
        let config = WirelessConfig {
            interface: "wlan1".into(),
            channel: 149,
        };
        let wifi = WifiService::with(Nmcli(vec!["sh".into(), script]), settings, true, config);
        wait_for(&wifi, |s| {
            s.status == "Hotspot NavBridge-TEST is on (5 GHz, channel 149)"
        });
        drop(wifi);

        let calls = fs::read_to_string(dir.join("calls")).unwrap();
        // Moved, not created again: phones keep its name and password.
        assert!(!calls.contains("connection add "), "{calls}");
        let moved = "connection modify navbridge-hotspot connection.interface-name wlan1 \
                     802-11-wireless.channel 149\n";
        assert_eq!(calls.matches(moved).count(), 1, "{calls}");
        assert!(calls.contains("device show wlan1"), "{calls}");
        assert!(!calls.contains("unexpected"), "{calls}");
        fs::remove_dir_all(dir).unwrap();
    }
}
