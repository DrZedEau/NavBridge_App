//! Converts GPSD fixes to the two NMEA sentences supported by Carlinkit.
use super::protocol::ProtocolMessage;
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct GnssFix {
    pub received: Instant,
    pub nmea: String,
}
impl GnssFix {
    pub fn fresh(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.received) <= Duration::from_secs(5)
    }
}
fn number(v: &Value, key: &str) -> Option<f64> {
    v[key].as_f64().filter(|n| n.is_finite())
}
fn sentence(body: &str) -> String {
    let checksum = body.bytes().fold(0u8, |sum, b| sum ^ b);
    format!("${body}*{checksum:02X}\r\n")
}
fn coordinate(value: f64, latitude: bool) -> String {
    // Round total minutes first so a value near a degree boundary cannot emit 60 minutes.
    let minutes = (value.abs() * 60.0 * 100000.0).round() as u64;
    let degree = minutes / 6000000;
    let minute = (minutes % 6000000) as f64 / 100000.0;
    if latitude {
        format!("{degree:02}{minute:08.5}")
    } else {
        format!("{degree:03}{minute:08.5}")
    }
}
fn utc_fields(time: &str) -> Option<(String, String)> {
    if !time.is_ascii() {
        return None;
    }
    let (date, time) = time.split_once('T')?;
    let date: Vec<_> = date.split('-').collect();
    let time: Vec<_> = time.strip_suffix('Z')?.split(':').collect();
    if date.len() != 3 || time.len() != 3 {
        return None;
    }
    let year: u32 = date[0].parse().ok()?;
    let month: u32 = date[1].parse().ok()?;
    let day: u32 = date[2].parse().ok()?;
    let hour: u32 = time[0].parse().ok()?;
    let minute: u32 = time[1].parse().ok()?;
    let second: f64 = time[2].parse().ok()?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if !(1980..=2099).contains(&year)
        || day == 0
        || day > days
        || hour > 23
        || minute > 59
        || !second.is_finite()
        || !(0.0..61.0).contains(&second)
    {
        return None;
    }
    // Truncate milliseconds; rounding 59.9999 into 60 would manufacture a leap second.
    let second = (second * 1000.0).floor() / 1000.0;
    Some((
        format!("{hour:02}{minute:02}{second:06.3}"),
        format!("{day:02}{month:02}{:02}", year % 100),
    ))
}
pub fn from_tpv(v: &Value, used: Option<u64>, hdop: Option<f64>, now: Instant) -> Option<GnssFix> {
    let mode = v["mode"].as_u64()?;
    if mode != 2 && mode != 3 {
        return None;
    }
    let (lat, lon) = (number(v, "lat")?, number(v, "lon")?);
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let (time, date) = utc_fields(v["time"].as_str()?)?;
    let ns = if lat.is_sign_negative() { "S" } else { "N" };
    let ew = if lon.is_sign_negative() { "W" } else { "E" };
    let (lat, lon) = (coordinate(lat, true), coordinate(lon, false));
    let speed = number(v, "speed")
        .filter(|n| *n >= 0.0)
        .map(|n| format!("{:.3}", n * 1.943844492))
        .unwrap_or_default();
    let track = number(v, "track")
        .filter(|n| (0.0..360.0).contains(n))
        .map(|n| format!("{n:.2}"))
        .unwrap_or_default();
    let altitude = if mode == 3 { number(v, "altMSL") } else { None };
    let geoid = altitude
        .zip(number(v, "altHAE"))
        .map(|(msl, hae)| format!("{:.2}", hae - msl))
        .unwrap_or_default();
    let altitude = altitude.map(|n| format!("{n:.2}")).unwrap_or_default();
    let used = used
        .filter(|n| *n <= 99)
        .map(|n| format!("{n:02}"))
        .unwrap_or_default();
    let hdop = hdop
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| format!("{n:.2}"))
        .unwrap_or_default();
    // GPSD status and NMEA quality use different enumerations.
    // Absent status is GPSD's normal autonomous fix. Unknown explicit values
    // are not advertised as valid positioning data.
    let (quality, indicator) = match v["status"].as_u64() {
        None | Some(1) => (1, 'A'),
        Some(2) => (2, 'D'),
        Some(3) => (4, 'R'),
        Some(4) => (5, 'F'),
        Some(5 | 6) => (6, 'E'),
        Some(8) => (8, 'S'),
        _ => return None,
    };
    let rmc = sentence(&format!(
        "GPRMC,{time},A,{lat},{ns},{lon},{ew},{speed},{track},{date},,,{indicator}"
    ));
    let gga = sentence(&format!(
        "GPGGA,{time},{lat},{ns},{lon},{ew},{quality},{used},{hdop},{altitude},M,{geoid},M,,"
    ));
    let nmea = format!("{gga}{rmc}");
    (nmea.len() < 1024).then_some(GnssFix {
        received: now,
        nmea,
    })
}

#[derive(Default)]
pub struct Forwarder {
    enabled: bool,
    last_sent: Option<Instant>,
    last_fix: Option<Instant>,
}
impl Forwarder {
    pub fn messages(
        &mut self,
        fix: Option<&GnssFix>,
        phone_connected: bool,
        now: Instant,
    ) -> Vec<ProtocolMessage> {
        let fix = fix.filter(|f| phone_connected && f.fresh(now));
        let Some(fix) = fix else {
            self.last_fix = None;
            self.last_sent = None;
            return if std::mem::take(&mut self.enabled) {
                vec![ProtocolMessage::control(19)]
            } else {
                vec![]
            };
        };
        let mut out = Vec::new();
        if !self.enabled {
            out.push(ProtocolMessage::control(18));
            self.enabled = true;
        }
        if self.last_fix != Some(fix.received)
            && self
                .last_sent
                .is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(1))
        {
            let mut payload = (fix.nmea.len() as u32).to_le_bytes().to_vec();
            payload.extend_from_slice(fix.nmea.as_bytes());
            out.push(ProtocolMessage::new(0x29, payload));
            self.last_sent = Some(now);
            self.last_fix = Some(fix.received);
        }
        out
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn report() -> Value {
        json!({"mode":3,"lat":48.1173,"lon":11.5166667,"time":"2026-09-20T12:35:19.123Z","speed":10,"track":84.4,"altMSL":545.4,"altHAE":592.3})
    }
    #[test]
    fn sentences_have_coordinates_units_dates_checksums_and_length() {
        let now = Instant::now();
        let f = from_tpv(&report(), Some(8), Some(0.9), now).unwrap();
        assert!(f.nmea.contains("4807.03800,N,01131.00000,E"));
        assert!(f.nmea.contains(",19.438,84.40,200926,"));
        assert!(f.nmea.contains(",1,08,0.90,545.40,M,46.90,M,,"));
        for line in f.nmea.split_terminator("\r\n") {
            let (body, sum) = line.strip_prefix('$').unwrap().split_once('*').unwrap();
            assert_eq!(
                u8::from_str_radix(sum, 16).unwrap(),
                body.bytes().fold(0u8, |a, b| a ^ b)
            );
        }
        let out = Forwarder::default().messages(Some(&f), true, now);
        assert_eq!(out[1].kind, 0x29);
        assert_eq!(&out[1].payload[..4], &(f.nmea.len() as u32).to_le_bytes());
        assert_eq!(&out[1].payload[4..], f.nmea.as_bytes());
        assert_eq!(coordinate(-12.999999999, true), "1300.00000");
    }
    #[test]
    fn maps_fix_quality_and_recalculates_checksums() {
        for (status, quality, indicator) in [
            (1, 1, 'A'),
            (2, 2, 'D'),
            (3, 4, 'R'),
            (4, 5, 'F'),
            (5, 6, 'E'),
            (6, 6, 'E'),
            (8, 8, 'S'),
        ] {
            let mut v = report();
            v["status"] = json!(status);
            let fix = from_tpv(&v, None, None, Instant::now()).unwrap();
            let lines: Vec<_> = fix.nmea.split_terminator("\r\n").collect();
            assert_eq!(lines[0].split(',').nth(6).unwrap(), quality.to_string());
            assert!(lines[1]
                .split('*')
                .next()
                .unwrap()
                .ends_with(&format!(",{indicator}")));
            for line in lines {
                let (body, checksum) = line[1..].split_once('*').unwrap();
                assert_eq!(
                    body.bytes().fold(0u8, |a, b| a ^ b),
                    u8::from_str_radix(checksum, 16).unwrap()
                );
            }
        }
        for status in [0, 7, 9, 99] {
            let mut v = report();
            v["status"] = json!(status);
            assert!(from_tpv(&v, None, None, Instant::now()).is_none());
        }
    }

    #[test]
    fn absent_invalid_stale_and_duplicate_fixes_do_not_get_forwarded() {
        let now = Instant::now();
        let f = from_tpv(&report(), None, None, now).unwrap();
        let mut sender = Forwarder::default();
        assert!(sender.messages(None, true, now).is_empty());
        assert!(sender.messages(Some(&f), false, now).is_empty());
        assert_eq!(sender.messages(Some(&f), true, now).len(), 2);
        assert!(sender
            .messages(Some(&f), true, now + Duration::from_secs(2))
            .is_empty());
        let stop = sender.messages(Some(&f), true, now + Duration::from_secs(6));
        assert_eq!(stop.len(), 1);
        assert_eq!(super::super::protocol::payload_u32(&stop[0]), Some(19));
        assert!(sender
            .messages(None, true, now + Duration::from_secs(7))
            .is_empty());
        for patch in [
            json!({"mode":1}),
            json!({"lat":91}),
            json!({"time":"invalid"}),
            json!({"time":"2026-02-30T12:00:00Z"}),
        ] {
            let mut v = report();
            for (k, value) in patch.as_object().unwrap() {
                v[k] = value.clone();
            }
            assert!(from_tpv(&v, None, None, now).is_none());
        }
    }
}
