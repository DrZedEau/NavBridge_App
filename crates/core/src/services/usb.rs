use rusb::{Context, Device, UsbContext};
use tracing::debug;

use navbridge_common::{UsbDeviceInfo, UsbSnapshot};

pub struct SelectedUsbDevice {
    pub device: Device<Context>,
    pub info: UsbDeviceInfo,
    pub snapshot: UsbSnapshot,
}

pub fn scan(context: &Context) -> Result<Option<SelectedUsbDevice>, String> {
    let devices = context.devices().map_err(|e| e.to_string())?;
    let mut scored = Vec::<(i32, Device<Context>, UsbDeviceInfo)>::new();

    for device in devices.iter() {
        let descriptor = match device.device_descriptor() {
            Ok(d) => d,
            Err(err) => {
                debug!("USB descriptor failed: {err}");
                continue;
            }
        };

        let vendor_id = descriptor.vendor_id();
        let product_id = descriptor.product_id();
        let known_vendor = vendor_id == 0x1314;
        let should_probe_strings = known_vendor || descriptor.class_code() == 0xff;
        let (manufacturer, product) = if should_probe_strings {
            match device.open() {
                Ok(handle) => (
                    handle
                        .read_manufacturer_string_ascii(&descriptor)
                        .unwrap_or_default(),
                    handle
                        .read_product_string_ascii(&descriptor)
                        .unwrap_or_default(),
                ),
                Err(err) => {
                    debug!(
                        "USB {:04x}:{:04x} cannot be opened for strings: {err}",
                        vendor_id, product_id
                    );
                    (String::new(), String::new())
                }
            }
        } else {
            (String::new(), String::new())
        };

        let text = format!("{} {}", manufacturer, product).to_ascii_lowercase();
        let named = ["carlinkit", "autokit", "carplay", "android auto"]
            .iter()
            .any(|needle| text.contains(needle));

        let (score, reason) = if known_vendor {
            (70, "known Carlinkit vendor")
        } else if named {
            (50, "USB product/manufacturer match")
        } else {
            continue;
        };

        let bus = device.bus_number();
        let address = device.address();
        let speed = format!("{:?}", device.speed());
        scored.push((
            score,
            device,
            UsbDeviceInfo {
                vendor_id,
                product_id,
                bus,
                address,
                speed,
                manufacturer,
                product,
                selection_reason: reason.into(),
            },
        ));
    }

    scored.sort_by(|(score_a, _, a), (score_b, _, b)| {
        score_b
            .cmp(score_a)
            .then_with(|| a.bus.cmp(&b.bus))
            .then_with(|| a.address.cmp(&b.address))
    });

    let candidates: Vec<UsbDeviceInfo> = scored.iter().map(|(_, _, d)| d.clone()).collect();
    if scored.is_empty() {
        debug!("no compatible CarPlay/Android Auto USB dongle detected");
        return Ok(None);
    }

    let (_, device, info) = scored.remove(0);
    debug!(
        "selected USB {:04x}:{:04x} bus {} address {} ({})",
        info.vendor_id, info.product_id, info.bus, info.address, info.selection_reason
    );

    Ok(Some(SelectedUsbDevice {
        device,
        info: info.clone(),
        snapshot: UsbSnapshot {
            selected: Some(info),
            candidates,
            error: None,
        },
    }))
}
