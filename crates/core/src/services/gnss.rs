//! GPSD owns USB discovery and receiver protocol negotiation; never probe unrelated serial ports.
use super::gnss_forward::{self, GnssFix};
use navbridge_common::GnssSnapshot;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    thread,
    time::{Duration, Instant},
};

pub struct GnssService {
    fix: Arc<RwLock<Option<GnssFix>>>,
    snapshot: Arc<RwLock<GnssSnapshot>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl GnssService {
    pub fn start() -> Self {
        let snapshot = Arc::new(RwLock::new(GnssSnapshot {
            status: "Connecting to GPSD".into(),
            ..Default::default()
        }));
        let fix = Arc::new(RwLock::new(None));
        let shared_fix = fix.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopped) = (snapshot.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let mut state = Receiver::default();
                let result = (|| -> std::io::Result<()> {
                    let mut socket = TcpStream::connect_timeout(
                        &"127.0.0.1:2947".parse().unwrap(),
                        Duration::from_millis(500),
                    )?;
                    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
                    socket.set_write_timeout(Some(Duration::from_secs(1)))?;
                    socket.write_all(b"?WATCH={\"enable\":true,\"json\":true};\n")?;
                    let mut pending = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !stopped.load(Ordering::Relaxed) {
                        match socket.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => {
                                pending.extend_from_slice(&chunk[..n]);
                                while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                                    if let Ok(report) = serde_json::from_slice(&pending[..end]) {
                                        state.report(&report, Instant::now());
                                    }
                                    pending.drain(..=end);
                                }
                                if pending.len() > 65536 {
                                    break;
                                }
                            }
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) => {}
                            Err(e) => return Err(e),
                        }
                        state.expire(Instant::now());
                        *shared_fix.write().unwrap() = state.fix.clone();
                        *shared.write().unwrap() = state.data.clone();
                    }
                    Ok(())
                })();
                let _ = result;
                *shared_fix.write().unwrap() = None;
                *shared.write().unwrap() = GnssSnapshot {
                    status: "GPSD unavailable".into(),
                    ..Default::default()
                };
                for _ in 0..10 {
                    if stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Self {
            fix,
            snapshot,
            stop,
            worker: Some(worker),
        }
    }
    pub fn forwarding_fix(&self) -> Option<GnssFix> {
        self.fix
            .read()
            .unwrap()
            .as_ref()
            .filter(|f| f.fresh(Instant::now()))
            .cloned()
    }
    pub fn snapshot(&self) -> GnssSnapshot {
        self.snapshot.read().unwrap().clone()
    }
}
impl Drop for GnssService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
#[derive(Default)]
struct Receiver {
    fix: Option<GnssFix>,
    satellites_used: Option<u64>,
    hdop_value: Option<f64>,
    devices: BTreeMap<String, String>,
    selected: String,
    data: GnssSnapshot,
    tpv: Option<Instant>,
    field_times: [Option<Instant>; 4],
    sky: Option<Instant>,
    hdop: Option<Instant>,
}
impl Receiver {
    fn select(&mut self) {
        if !self.devices.contains_key(&self.selected) {
            self.selected = self.devices.keys().next().cloned().unwrap_or_default();
            self.data = GnssSnapshot::default();
            self.fix = None;
            self.satellites_used = None;
            self.hdop_value = None;
            self.tpv = None;
            self.field_times = [None; 4];
            self.sky = None;
            self.hdop = None;
        }
        self.data.receiver = self
            .devices
            .get(&self.selected)
            .cloned()
            .unwrap_or_default();
        if self.selected.is_empty() {
            self.data.status = "Not detected".into();
        } else if self.tpv.is_none() {
            self.data.status = "Detected / waiting for data".into();
        }
    }
    fn device(&mut self, v: &Value) {
        let Some(path) = v["path"].as_str() else {
            return;
        };
        if v["activated"].as_f64() == Some(0.0) {
            self.devices.remove(path);
        } else {
            let driver = v["driver"].as_str().unwrap_or("GNSS");
            let port = path.rsplit('/').next().unwrap_or(path);
            if driver != "GNSS" || !self.devices.contains_key(path) {
                self.devices
                    .insert(path.into(), format!("{driver} ({port})"));
            }
        }
    }
    fn clear_fix(&mut self) {
        self.field_times = [None; 4];
        self.data.position.clear();
        self.data.altitude.clear();
        self.data.speed.clear();
        self.data.utc.clear();
    }
    fn report(&mut self, v: &Value, now: Instant) {
        match v["class"].as_str().unwrap_or("") {
            "DEVICES" => {
                self.devices.clear();
                if let Some(devices) = v["devices"].as_array() {
                    for d in devices {
                        self.device(d);
                    }
                }
                self.select();
            }
            "DEVICE" => {
                self.device(v);
                self.select();
            }
            "TPV" | "SKY" => {
                let Some(path) = v["device"].as_str() else {
                    return;
                };
                if self.selected.is_empty() {
                    self.device(&serde_json::json!({"path":path}));
                    self.select();
                }
                if path != self.selected {
                    return;
                }
                if v["class"] == "TPV" {
                    let used = self
                        .sky
                        .filter(|t| now.duration_since(*t) <= Duration::from_secs(5))
                        .and(self.satellites_used);
                    let hdop = self
                        .hdop
                        .filter(|t| now.duration_since(*t) <= Duration::from_secs(5))
                        .and(self.hdop_value);
                    if let Some(fix) = gnss_forward::from_tpv(v, used, hdop, now) {
                        self.fix = Some(fix);
                    } else if v["mode"].as_u64().is_some_and(|mode| mode < 2)
                        || (v.get("lat").is_some()
                            && v.get("lon").is_some()
                            && v.get("time").is_some())
                    {
                        self.fix = None;
                    }

                    // TPV fields can arrive in separate reports. Only supplied
                    // fields refresh their age; explicit fix loss clears all.
                    self.expire(now);
                    self.tpv = Some(now);
                    let mode = v["mode"].as_u64().unwrap_or(0);
                    self.data.status = match mode {
                        2 => "Connected / 2D fix",
                        3 => "Connected / 3D fix",
                        _ => "Connected / No fix",
                    }
                    .into();
                    if mode < 2 {
                        self.clear_fix();
                    }
                    if mode == 2 {
                        self.data.altitude.clear();
                        self.field_times[1] = None;
                    }
                    if mode >= 2 {
                        if let (Some(lat), Some(lon)) = (number(v, "lat"), number(v, "lon")) {
                            if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
                                self.field_times[0] = Some(now);
                                self.data.position = format!("{lat:.5}, {lon:.5}");
                            }
                        }
                        if let Some(speed) = number(v, "speed").filter(|s| *s >= 0.0) {
                            self.field_times[2] = Some(now);
                            self.data.speed = format!("{:.1} km/h", speed * 3.6);
                        }
                        if let Some(time) = v["time"].as_str() {
                            self.data.utc = time.replace('T', " ").replace('Z', "");
                            self.field_times[3] = Some(now);
                        }
                        if mode == 3 {
                            if let Some(alt) = number(v, "altMSL") {
                                self.field_times[1] = Some(now);
                                self.data.altitude = format!("{alt:.1} m");
                            }
                        }
                    }
                } else {
                    let sats = v["satellites"].as_array();
                    let used = v["uSat"].as_u64().or_else(|| {
                        sats.map(|s| s.iter().filter(|s| s["used"] == true).count() as u64)
                    });
                    let view = v["nSat"].as_u64().or_else(|| sats.map(|s| s.len() as u64));
                    // GPSD can send satellite and DOP-only SKY reports separately.
                    // Missing fields do not invalidate the last observation or refresh its age.
                    if let (Some(u), Some(n)) = (used, view) {
                        self.data.satellites = format!("{u} / {n}");
                        self.sky = Some(now);
                        self.satellites_used = Some(u);
                    }
                    if let Some(hdop) = number(v, "hdop").filter(|n| *n > 0.0) {
                        self.data.hdop = format!("{hdop:.1}");
                        self.hdop = Some(now);
                        self.hdop_value = Some(hdop);
                    }
                }
            }
            _ => {}
        }
    }
    fn expire(&mut self, now: Instant) {
        for (field, timestamp) in [
            &mut self.data.position,
            &mut self.data.altitude,
            &mut self.data.speed,
            &mut self.data.utc,
        ]
        .into_iter()
        .zip(self.field_times.iter_mut())
        {
            if timestamp.is_some_and(|t| now.duration_since(t) > Duration::from_secs(5)) {
                field.clear();
                *timestamp = None;
            }
        }
        if self.fix.as_ref().is_some_and(|fix| !fix.fresh(now)) {
            self.fix = None;
        }
        if self
            .tpv
            .is_some_and(|t| now.duration_since(t) > Duration::from_secs(5))
        {
            self.clear_fix();
            self.data.status = "Detected / data stale".into();
        }
        if self
            .sky
            .is_some_and(|t| now.duration_since(t) > Duration::from_secs(5))
        {
            self.data.satellites.clear();
        }
        if self
            .hdop
            .is_some_and(|t| now.duration_since(t) > Duration::from_secs(5))
        {
            self.data.hdop.clear();
        }
    }
}
fn number(v: &Value, key: &str) -> Option<f64> {
    v[key].as_f64().filter(|n| n.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    #[ignore = "requires a USB receiver managed by local GPSD"]
    fn live_gpsd_receiver() {
        let service = GnssService::start();
        thread::sleep(Duration::from_secs(6));
        let snapshot = service.snapshot();
        println!(
            "Receiver: {}; status: {}; satellites: {}",
            snapshot.receiver, snapshot.status, snapshot.satellites
        );
        assert!(
            !snapshot.receiver.is_empty(),
            "GPSD did not report a receiver"
        );
        assert!(
            !snapshot.satellites.is_empty() || snapshot.status.starts_with("Connected"),
            "No decoded receiver reports"
        );
    }
    #[test]
    fn forwarding_fix_is_cleared_on_loss_removal_and_staleness() {
        let now = Instant::now();
        let mut r = Receiver::default();
        let fix = json!({"class":"TPV","device":"a","mode":3,"lat":48.0,"lon":2.0,"time":"2026-09-20T12:00:00Z"});
        r.report(&fix, now);
        assert!(r.fix.is_some());
        r.report(&json!({"class":"TPV","device":"a","mode":1}), now);
        assert!(r.fix.is_none());
        r.report(&fix, now);
        r.report(&json!({"class":"DEVICE","path":"a","activated":0}), now);
        assert!(r.fix.is_none());
        r.report(&fix, now);
        r.report(
            &json!({"class":"TPV","device":"a","mode":3}),
            now + Duration::from_secs(4),
        );
        r.expire(now + Duration::from_secs(6));
        assert!(r.fix.is_none());
    }

    #[test]
    fn partial_sky_reports_preserve_counts_without_extending_their_lifetime() {
        let mut r = Receiver::default();
        let now = Instant::now();
        r.report(
            &json!({"class":"SKY","device":"test","uSat":12,"nSat":25,"hdop":0.8}),
            now,
        );
        r.report(&json!({"class":"TPV","device":"test","mode":3}), now);
        r.report(
            &json!({"class":"SKY","device":"test","hdop":0.7}),
            now + Duration::from_secs(4),
        );
        assert_eq!(r.data.satellites, "12 / 25");
        r.expire(now + Duration::from_secs(6));
        assert!(r.data.satellites.is_empty());
        assert_eq!(r.data.hdop, "0.7");
        r.report(
            &json!({"class":"SKY","device":"test","satellites":[]}),
            now + Duration::from_secs(7),
        );
        assert_eq!(r.data.satellites, "0 / 0");
        r.expire(now + Duration::from_secs(10));
        assert_eq!(r.data.satellites, "0 / 0");
        assert!(r.data.hdop.is_empty());
    }

    #[test]
    fn fix_loss_partial_reports_and_staleness_clear_navigation() {
        let mut r = Receiver::default();
        let now = Instant::now();
        let fix = json!({"class":"TPV","device":"/dev/test","mode":3,"lat":48.0,"lon":2.0,"altMSL":100.0,"speed":10.0,"time":"2026-09-20T12:00:00Z"});
        r.report(&fix, now);
        assert_eq!(r.data.speed, "36.0 km/h");
        assert_eq!(r.data.altitude, "100.0 m");
        r.report(
            &json!({"class":"TPV","device":"/dev/test","mode":1,"time":"2019-04-07T00:00:00Z"}),
            now,
        );
        assert!(r.data.position.is_empty());
        assert!(r.data.utc.is_empty());
        r.report(&fix, now);
        r.expire(now + Duration::from_secs(6));
        assert!(r.data.position.is_empty());
        assert!(r.data.status.contains("stale"));
        r.report(&fix, now);
        r.report(&json!({"class":"TPV","device":"/dev/test","mode":2}), now);
        assert_eq!(r.data.position, "48.00000, 2.00000");
        assert!(r.data.altitude.is_empty());
    }
    #[test]
    fn partial_tpv_preserves_fields_without_renewing_their_age() {
        let mut r = Receiver::default();
        let now = Instant::now();
        r.report(&json!({"class":"TPV","device":"a","mode":3,"lat":48.0,"lon":2.0,"altMSL":100,"speed":10,"time":"2026-09-20T12:00:00Z"}), now);
        r.report(
            &json!({"class":"TPV","device":"a","mode":3,"speed":20}),
            now + Duration::from_secs(4),
        );
        assert_eq!(r.data.position, "48.00000, 2.00000");
        assert_eq!(r.data.altitude, "100.0 m");
        assert_eq!(r.data.speed, "72.0 km/h");
        r.expire(now + Duration::from_secs(6));
        assert!(r.data.position.is_empty());
        assert!(r.data.altitude.is_empty());
        assert!(r.data.utc.is_empty());
        assert_eq!(r.data.speed, "72.0 km/h");
        r.report(
            &json!({"class":"TPV","device":"a","mode":1}),
            now + Duration::from_secs(7),
        );
        assert!(r.data.speed.is_empty());
    }

    #[test]
    fn devices_are_isolated_and_hotplug_clears_old_data() {
        let mut r = Receiver::default();
        let now = Instant::now();
        r.report(
            &json!({"class":"DEVICES","devices":[{"path":"a","driver":"u-blox"},{"path":"b"}]}),
            now,
        );
        r.report(
            &json!({"class":"TPV","device":"b","mode":3,"lat":1,"lon":2}),
            now,
        );
        assert!(r.data.position.is_empty());
        r.report(&json!({"class":"SKY","device":"a","satellites":[{"used":true},{"used":false}],"hdop":1.2}),now);
        assert_eq!(r.data.satellites, "1 / 2");
        r.report(&json!({"class":"DEVICE","path":"a","activated":0}), now);
        assert_eq!(r.selected, "b");
        assert!(r.data.satellites.is_empty());
        r.report(&json!({"class":"DEVICE","path":"b","activated":0}), now);
        assert_eq!(r.data.status, "Not detected");
    }
}
