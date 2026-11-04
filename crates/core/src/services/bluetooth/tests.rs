use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::agent::{Agent, CONFIRM_TIME};
use super::bluez::{
    adapter, bluetooth_address, describe, is_phone, paired_phones, register_services,
    unregister_services, AGENT, HANDS_FREE_GATEWAY_UUID, HEADSET_GATEWAY_UUID, HEADSET_PROFILE,
    WIRELESS_PROFILE,
};
use super::profiles::{HeadsetProfile, WirelessProfile, JOINING_TIME};
use super::rfcomm::listened;
use super::worker::{call_in_turn, to_call};
use super::*;
use crate::services::wifi;
use navbridge_common::{BluetoothPhone, PairingRequest};
use zbus::blocking::Connection;
use zbus::zvariant::OwnedObjectPath;

#[test]
fn adapter_addresses_reach_sockets_least_significant_byte_first() {
    assert_eq!(
        bluetooth_address("2C:CF:67:E1:98:4D"),
        Some([0x4d, 0x98, 0xe1, 0x67, 0xcf, 0x2c])
    );
    assert_eq!(bluetooth_address("2C:CF:67:E1:98"), None);
    assert_eq!(bluetooth_address("2C:CF:67:E1:98:4D:00"), None);
    assert_eq!(bluetooth_address("2C:CF:67:E1:98:XX"), None);
}

#[test]
fn only_paired_phones_are_called() {
    let gateway = vec![HEADSET_GATEWAY_UUID.to_uppercase()];
    let hands_free = vec![HANDS_FREE_GATEWAY_UUID.into()];
    let keyboard = vec!["00001124-0000-1000-8000-00805f9b34fb".to_string()];
    assert!(is_phone(true, &gateway));
    assert!(is_phone(true, &hands_free));
    assert!(!is_phone(false, &gateway));
    assert!(!is_phone(true, &keyboard));
}

#[test]
fn calls_stop_once_a_phone_sets_up() {
    let phones: Vec<(String, String)> = ["Pixel 10", "Pixel 8"]
        .map(|name| (name.to_string(), name.to_string()))
        .to_vec();
    let setup_time = Duration::from_millis(500);
    let joining = |name: &str| Some((name.to_string(), Instant::now() + JOINING_TIME));

    // The first phone answers, and its Android Auto connects a moment
    // later: the second is not called.
    let shared = Arc::new(Mutex::new(Shared::default()));
    let answered = AtomicBool::new(false);
    let mut called = Vec::new();
    call_in_turn(&phones, &shared, &answered, setup_time, |phone: &String| {
        called.push(phone.clone());
        let (shared, phone) = (shared.clone(), phone.clone());
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            shared.lock().unwrap().joining = Some((phone, Instant::now() + JOINING_TIME));
        });
        Ok(())
    });
    assert_eq!(called, ["Pixel 10"]);
    assert!(answered.load(Ordering::Relaxed));

    // A phone that answers without setting up holds up the next call by
    // `setup_time`...
    let (shared, answered) = (Mutex::new(Shared::default()), AtomicBool::new(false));
    let (mut called, started) = (Vec::new(), Instant::now());
    call_in_turn(&phones, &shared, &answered, setup_time, |phone: &String| {
        called.push(phone.clone());
        Ok(())
    });
    assert_eq!(called, ["Pixel 10", "Pixel 8"]);
    assert!(started.elapsed() >= setup_time);

    // ...and one that is away does not.
    let (mut called, started) = (Vec::new(), Instant::now());
    call_in_turn(&phones, &shared, &answered, setup_time, |phone: &String| {
        called.push(phone.clone());
        match phone.as_str() {
            "Pixel 10" => Err(zbus::Error::Failure("br-connection-page-timeout".into())),
            _ => Ok(()),
        }
    });
    assert_eq!(called, ["Pixel 10", "Pixel 8"]);
    assert!(started.elapsed() < setup_time);

    // While a phone sets up, nobody is called.
    let shared = Mutex::new(Shared {
        joining: joining("Pixel 8"),
        ..Shared::default()
    });
    call_in_turn(&phones, &shared, &answered, setup_time, |_: &String| {
        panic!("a phone was called during another's setup")
    });
}

#[test]
fn the_status_says_what_bluetooth_does() {
    let name = "NavBridge-984B";
    let phones = ["Pixel 7 Pro".to_string()];
    assert_eq!(describe(false, None, None, None, false, &phones, name), "");
    assert_eq!(
        describe(true, Some("no adapter"), None, None, false, &phones, name),
        "Bluetooth unavailable: no adapter"
    );
    assert_eq!(
        describe(
            true,
            None,
            Some(Duration::from_secs(95)),
            None,
            false,
            &phones,
            name
        ),
        "Pairing open (1:35): on your phone, pair with NavBridge-984B"
    );
    assert_eq!(
        describe(true, None, None, Some("Pixel 7 Pro"), false, &phones, name),
        "Pixel 7 Pro is joining the hotspot"
    );
    assert_eq!(
        describe(true, None, None, None, true, &phones, name),
        "Disconnected: Connect phone in Settings calls the phones again"
    );
    assert_eq!(
        describe(true, None, None, None, false, &phones, name),
        "Waiting for Pixel 7 Pro"
    );
    assert_eq!(
        describe(true, None, None, None, false, &[], name),
        "No phone paired: enable pairing"
    );
}

#[test]
fn only_the_default_phone_is_called_while_it_is_paired() {
    let phone = |name: &str, address: &str| Phone {
        path: OwnedObjectPath::try_from(format!(
            "/org/bluez/hci0/dev_{}",
            address.replace(':', "_")
        ))
        .unwrap(),
        address: address.into(),
        name: name.into(),
    };
    let phones = [
        phone("Pixel 10", "64:9D:38:E1:B6:24"),
        phone("Pixel 8", "A0:B1:C2:D3:E4:F5"),
    ];
    let names = |default| -> Vec<String> {
        to_call(&phones, default)
            .into_iter()
            .map(|phone| phone.name)
            .collect()
    };
    assert_eq!(names(None), ["Pixel 10", "Pixel 8"]);
    // Its address, whichever its case.
    assert_eq!(names(Some("a0:b1:c2:d3:e4:f5")), ["Pixel 8"]);
    // A default phone no longer paired: every phone, as without one.
    assert_eq!(names(Some("00:11:22:33:44:55")), ["Pixel 10", "Pixel 8"]);
}

#[test]
fn a_disconnected_phone_is_turned_away_until_connect() {
    // Wireless mode stays off, so BlueZ is never contacted.
    let service = BluetoothService::start(
        wifi::HotspotWatch::default(),
        Some("A0:B1:C2:D3:E4:F5".into()),
    );
    let pixel = OwnedObjectPath::try_from("/org/bluez/hci0/dev_A0_B1_C2_D3_E4_F5").unwrap();
    {
        let mut shared = service.shared.lock().unwrap();
        shared.last_setup = Some(pixel.clone());
        shared.phones = vec![BluetoothPhone {
            address: "A0:B1:C2:D3:E4:F5".into(),
            name: "Pixel 8".into(),
        }];
    }
    let snapshot = service.snapshot();
    assert_eq!(snapshot.default_phone.as_deref(), Some("A0:B1:C2:D3:E4:F5"));
    assert!(!snapshot.paused);
    service.disconnect();
    assert!(service.snapshot().paused);
    assert_eq!(service.shared.lock().unwrap().turned_away, Some(pixel));
    service.connect();
    assert!(!service.snapshot().paused);
    assert_eq!(service.shared.lock().unwrap().turned_away, None);
    // In BlueZ's case; and a default phone that is not paired, as none.
    service.set_default_phone(Some("a0:b1:c2:d3:e4:f5".into()));
    assert_eq!(
        service.snapshot().default_phone.as_deref(),
        Some("A0:B1:C2:D3:E4:F5")
    );
    service.set_default_phone(Some("00:11:22:33:44:55".into()));
    assert_eq!(service.snapshot().default_phone, None);
}

fn waiting_code(service: &BluetoothService, until: Instant) -> async_channel::Receiver<bool> {
    let (reply, answer) = async_channel::bounded(1);
    service.shared.lock().unwrap().confirm = Some(Confirm {
        device: OwnedObjectPath::try_from("/org/bluez/hci0/dev_00_11_22_33_44_55").unwrap(),
        name: "Pixel 7 Pro".into(),
        code: "042137".into(),
        reply,
        until,
    });
    answer
}

#[test]
fn the_users_answer_or_its_absence_reaches_the_phone() {
    // Wireless mode stays off, so BlueZ is never contacted.
    let service = BluetoothService::start(wifi::HotspotWatch::default(), None);
    let answer = waiting_code(&service, Instant::now() + CONFIRM_TIME);
    assert_eq!(
        service.snapshot().confirm,
        Some(PairingRequest {
            device: "Pixel 7 Pro".into(),
            code: "042137".into()
        })
    );
    service.confirm(true);
    assert_eq!(answer.try_recv(), Ok(true));
    assert_eq!(service.snapshot().confirm, None);
    // A code left unanswered is refused.
    let answer = waiting_code(&service, Instant::now());
    let deadline = Instant::now() + Duration::from_secs(3);
    while answer.is_empty() {
        assert!(Instant::now() < deadline, "the code was never refused");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(answer.try_recv(), Ok(false));
    assert_eq!(service.snapshot().confirm, None);
}

/// Registers NavBridge's services with the real BlueZ, then removes them.
/// The adapter's name and visibility are left alone.
#[test]
#[ignore = "requires BlueZ"]
fn live_bluez_registration() {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let connection = Connection::system().unwrap();
    {
        let server = connection.object_server();
        server.at(AGENT, Agent(shared.clone())).unwrap();
        server
            .at(
                WIRELESS_PROFILE,
                WirelessProfile(shared, wifi::HotspotWatch::default()),
            )
            .unwrap();
        server.at(HEADSET_PROFILE, HeadsetProfile).unwrap();
    }
    let adapter = adapter(&connection).unwrap();
    println!(
        "adapter: powered {}, pairable {}, discoverable {}",
        adapter.powered().unwrap(),
        adapter.pairable().unwrap(),
        adapter.discoverable().unwrap()
    );
    register_services(&connection).unwrap();
    // Twice is fine, as after a restart of NavBridge.
    let channel = register_services(&connection).unwrap();
    let device = bluetooth_address(&adapter.address().unwrap()).unwrap();
    assert!(
        listened(device, channel),
        "BlueZ is not on channel {channel}"
    );
    println!("Android Auto wireless service on RFCOMM channel {channel}");
    println!(
        "paired phones: {}",
        paired_phones(&connection).unwrap().len()
    );
    unregister_services(&connection).unwrap();
    assert!(!listened(device, channel), "channel {channel} still taken");
}
