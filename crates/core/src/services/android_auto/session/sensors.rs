//! The car's sensors for the phone: night mode, the parking brake and driving
//! status, speed and location.

use std::time::{Duration, Instant};

use super::generated::aa as p;
use super::protocol::{msg, FULLY_RESTRICTED, SENSOR, UNRESTRICTED};
use super::Session;
use crate::services::gnss::Fix;

/// How old the last fix may be to answer the phone's request for location,
/// and its speed to stand for the car's: older, as when the receiver loses its
/// fix, the I-Bus's speed goes out instead.
pub(super) const FRESH_FIX: Duration = Duration::from_secs(2);

/// Where the speed the phone gets comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SpeedSource {
    /// The GNSS receiver's fixes, while they carry a speed.
    Gnss,
    /// The cluster's speedometer, without them.
    Ibus,
}

impl Session {
    pub fn night(&mut self, enabled: bool) -> Result<(), String> {
        self.night = enabled;
        if self.night_sensor && self.opened[SENSOR as usize] {
            let mut n = p::Night::new();
            n.set_is_night(enabled);
            let mut e = p::SensorEvent::new();
            e.night_mode.push(n);
            self.send(SENSOR, msg::sensor::EVENT, &e)?;
        }
        Ok(())
    }

    /// The parking brake, and with it the driving restrictions: none while it
    /// is on and the car stands still, all of them while it is off.
    pub fn parking_brake(&mut self, engaged: bool) -> Result<(), String> {
        self.parked = engaged;
        self.send_parking_brake()?;
        self.send_driving_status()
    }

    /// The car's speed in km/h, or None once the I-Bus no longer reports it;
    /// the phone gets it while no fresh fix carries a speed. While known, a
    /// moving car is restricted whatever the parking brake says; standing
    /// still, the parking brake decides.
    pub fn vehicle_speed(&mut self, kmh: Option<u16>) -> Result<(), String> {
        let was = self.restricted();
        self.speed = kmh;
        self.send_speed(Some(SpeedSource::Ibus))?;
        if self.restricted() != was {
            tracing::debug!(?kmh, restricted = !was, "Android Auto driving restrictions");
            self.send_driving_status()?;
        }
        Ok(())
    }

    /// Restricted unless parked and, while the speed is known, standing
    /// still: the I-Bus's 0 km/h alone is anything under 2 km/h, such as
    /// creeping in traffic. Not from fixes, whose speed wanders standing still.
    pub(super) fn restricted(&self) -> bool {
        !self.parked || self.speed.is_some_and(|kmh| kmh > 0)
    }

    /// The speed the phone gets, in mm/s, and where it comes from: the latest
    /// fix's while fresh, else the I-Bus's.
    pub(super) fn current_speed(&self) -> Option<(i32, SpeedSource)> {
        match self.gnss_speed {
            Some((mps, at)) if at.elapsed() < FRESH_FIX => {
                Some(((mps * 1e3).round() as i32, SpeedSource::Gnss))
            }
            _ => self.speed.map(|kmh| {
                (
                    (f64::from(kmh) * 1e3 / 3.6).round() as i32,
                    SpeedSource::Ibus,
                )
            }),
        }
    }

    /// Sends the current speed once the phone has started the speed sensor:
    /// whichever it is for `None`, else only when it comes `from` there.
    pub(super) fn send_speed(&mut self, from: Option<SpeedSource>) -> Result<(), String> {
        let Some((mm_s, source)) = self.current_speed() else {
            return Ok(());
        };
        if from.is_some_and(|from| from != source)
            || !(self.speed_sensor && self.opened[SENSOR as usize])
        {
            return Ok(());
        }
        if self.speed_source.replace(source) != Some(source) {
            match source {
                SpeedSource::Gnss => tracing::info!("Android Auto's speed comes from GNSS fixes"),
                SpeedSource::Ibus => tracing::info!("Android Auto's speed comes from the I-Bus"),
            }
        }
        let mut speed = p::Speed::new();
        speed.set_speed(mm_s);
        let mut e = p::SensorEvent::new();
        e.speed.push(speed);
        self.send(SENSOR, msg::sensor::EVENT, &e)
    }

    pub(super) fn send_parking_brake(&mut self) -> Result<(), String> {
        if !(self.parking_brake_sensor && self.opened[SENSOR as usize]) {
            return Ok(());
        }
        let mut brake = p::ParkingBrake::new();
        brake.set_parking_brake(self.parked);
        let mut e = p::SensorEvent::new();
        e.parking_brake.push(brake);
        self.send(SENSOR, msg::sensor::EVENT, &e)
    }

    pub(super) fn send_driving_status(&mut self) -> Result<(), String> {
        if !(self.driving_status_sensor && self.opened[SENSOR as usize]) {
            return Ok(());
        }
        let mut status = p::DrivingStatus::new();
        status.set_status(if self.restricted() {
            FULLY_RESTRICTED
        } else {
            UNRESTRICTED
        });
        let mut e = p::SensorEvent::new();
        e.driving_status.push(status);
        self.send(SENSOR, msg::sensor::EVENT, &e)
    }

    /// Passes the receiver's fix to the phone once it has started the
    /// location sensor, and keeps the latest one to answer that start; its
    /// speed, if any, goes to the speed sensor.
    pub fn location(&mut self, fix: Fix) -> Result<(), String> {
        if let Some(mps) = fix.speed {
            self.gnss_speed = Some((mps, Instant::now()));
            self.send_speed(Some(SpeedSource::Gnss))?;
        }
        if !(self.location_sensor && self.opened[SENSOR as usize]) {
            self.last_fix = Some((fix, Instant::now()));
            return Ok(());
        }
        let scaled = |value: f64, scale: f64| (value * scale).round() as i32;
        let mut l = p::Location::new();
        l.set_latitude(scaled(fix.latitude, 1e7));
        l.set_longitude(scaled(fix.longitude, 1e7));
        if let Some(accuracy) = fix.accuracy {
            l.set_accuracy((accuracy * 1e3).round() as u32);
        }
        if let Some(altitude) = fix.altitude {
            l.set_altitude(scaled(altitude, 1e2));
        }
        if let Some(speed) = fix.speed {
            l.set_speed(scaled(speed, 1e3));
        }
        if let Some(bearing) = fix.bearing {
            l.set_bearing(scaled(bearing, 1e6));
        }
        let mut e = p::SensorEvent::new();
        e.location.push(l);
        self.send(SENSOR, msg::sensor::EVENT, &e)
    }
}
