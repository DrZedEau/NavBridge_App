use crate::render::*;
use navbridge_common::VehicleSnapshot;

#[test]
fn the_ibus_page_shows_readings_and_what_is_missing() {
    let vehicle = VehicleSnapshot {
        status: "Receiving".into(),
        interface: true,
        frames: 1234,
        bad_lines: 2,
        ignition: Some(navbridge_common::Ignition::On),
        speed_kmh: Some(120),
        rpm: Some(3000),
        coolant_c: Some(90),
        outside_c: Some(-4),
        range_km: Some(473),
        consumption_dl: Some(132),
        handbrake: Some(true),
        low_beam: Some(true),
        reverse: Some(false),
    };
    assert_eq!(
        vehicle_status(&vehicle, true),
        "Receiving · 1234 frames, 2 damaged"
    );
    assert_eq!(reading(vehicle.speed_kmh, "km/h"), "120 km/h");
    assert_eq!(reading(vehicle.rpm, "rpm"), "3000 rpm");
    assert_eq!(reading(vehicle.outside_c, "°C"), "-4 °C");
    assert_eq!(kilometres(vehicle.range_km), "473 km");
    assert_eq!(kilometres(Some(1_024)), "1 024 km");
    assert_eq!(consumption(vehicle.consumption_dl), "13.2 L/100 km");
    assert_eq!(consumption(Some(90)), "9.0 L/100 km");
    assert_eq!(gnss_speed("36.0 km/h"), "GPS 36.0 km/h");
    assert_eq!(gnss_speed(""), "");

    let unknown = VehicleSnapshot {
        status: "Interface not connected".into(),
        ..Default::default()
    };
    assert_eq!(vehicle_status(&unknown, true), "Interface not connected");
    assert_eq!(reading(unknown.rpm, "rpm"), "n/a");
    assert_eq!(kilometres(unknown.range_km), "n/a");
    assert_eq!(consumption(unknown.consumption_dl), "n/a");
    // Without the core, nothing is known about the car.
    assert_eq!(vehicle_status(&vehicle, false), "Unavailable");
}
