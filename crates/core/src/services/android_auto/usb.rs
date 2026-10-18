use super::transport::deliver;
use navbridge_common::UsbDeviceInfo;
use rusb::{
    Context, Device, DeviceHandle, Direction, Recipient, RequestType, TransferType, UsbContext,
};
use std::time::{Duration, Instant};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
        Arc,
    },
    thread::{self, JoinHandle},
};
pub struct Transport {
    handle: Arc<DeviceHandle<Context>>,
    received: Receiver<Result<Vec<u8>, rusb::Error>>,
    stopped: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    output: u8,
    pub info: UsbDeviceInfo,
}
/// Phone-like devices that cannot become an Android accessory, by bus and
/// address. They are skipped until replugged, which assigns a new address.
#[derive(Default)]
pub struct Rejected(HashSet<(u8, u8)>);
/// Why a device could not become the accessory transport.
enum Failure {
    /// It cannot be opened or does not speak AOA, so retrying cannot help.
    Unusable(String),
    /// It speaks AOA, but switching or claiming it failed: retry next scan.
    Retry(String),
}
impl From<String> for Failure {
    fn from(e: String) -> Self {
        Self::Retry(e)
    }
}
fn accessory(vid: u16, pid: u16) -> bool {
    vid == 0x18d1 && matches!(pid, 0x2d00 | 0x2d01 | 0x2d04 | 0x2d05)
}
fn phone<T: UsbContext>(d: &Device<T>) -> bool {
    let Ok(desc) = d.device_descriptor() else {
        return false;
    };
    if accessory(desc.vendor_id(), desc.product_id()) {
        return true;
    }
    let Ok(config) = d.config_descriptor(0) else {
        return false;
    };
    config.interfaces().any(|i| {
        i.descriptors().any(|a| {
            a.class_code() == 6 && a.sub_class_code() == 1
                || a.class_code() == 255 && a.sub_class_code() == 0x42 && a.protocol_code() == 1
        })
    })
}
fn info(d: &Device<Context>) -> Result<UsbDeviceInfo, String> {
    let desc = d.device_descriptor().map_err(|e| e.to_string())?;
    let h = d.open().map_err(|e| e.to_string())?;
    Ok(UsbDeviceInfo {
        vendor_id: desc.vendor_id(),
        product_id: desc.product_id(),
        bus: d.bus_number(),
        address: d.address(),
        speed: format!("{:?}", d.speed()),
        manufacturer: h.read_manufacturer_string_ascii(&desc).unwrap_or_default(),
        product: h.read_product_string_ascii(&desc).unwrap_or_default(),
    })
}
/// One pass over phone-like devices, keyed by bus and address, returning the
/// first that connects. A failure does not end the pass, so an unusable device
/// cannot hide a phone behind it. The error lists the failures if none connect.
fn first_connected<D, T>(
    candidates: Vec<((u8, u8), String, D)>,
    rejected: &mut HashSet<(u8, u8)>,
    mut connect: impl FnMut(D) -> Result<T, Failure>,
) -> Result<Option<T>, String> {
    // Forget devices that were unplugged.
    rejected.retain(|known| candidates.iter().any(|(key, ..)| key == known));
    let mut failures = Vec::new();
    for (key, id, device) in candidates {
        if rejected.contains(&key) {
            continue;
        }
        match connect(device) {
            Ok(connected) => return Ok(Some(connected)),
            Err(Failure::Unusable(e)) => {
                rejected.insert(key);
                failures.push(format!("{id} ignored until replugged: {e}"));
            }
            Err(Failure::Retry(e)) => failures.push(format!("{id}: {e}")),
        }
    }
    if failures.is_empty() {
        Ok(None)
    } else {
        Err(failures.join("; "))
    }
}
/// Reset every phone already in accessory mode, so it re-enumerates and its
/// Android Auto session starts over. Returns how many were reset.
pub fn reset_accessories(context: &Context) -> usize {
    let Ok(devices) = context.devices() else {
        return 0;
    };
    let mut reset = 0;
    for device in devices.iter() {
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };
        if !accessory(desc.vendor_id(), desc.product_id()) {
            continue;
        }
        if let Ok(handle) = device.open() {
            // Usually reports NotFound: the phone comes back as a new device.
            let _ = handle.reset();
            reset += 1;
        }
    }
    reset
}
impl Transport {
    /// Switch the first usable phone to accessory mode and claim it. Devices
    /// that fail are skipped; unusable ones stay in `rejected` until replugged.
    ///
    /// `wake` is signalled after each queued transfer so the session thread can
    /// sleep until USB data arrives.
    pub fn discover(
        context: &Context,
        wake: &SyncSender<()>,
        rejected: &mut Rejected,
    ) -> Result<Option<Self>, String> {
        let candidates: Vec<_> = context
            .devices()
            .map_err(|e| e.to_string())?
            .iter()
            .filter(phone)
            .map(|d| {
                let id = d
                    .device_descriptor()
                    .map(|desc| format!("{:04x}:{:04x}", desc.vendor_id(), desc.product_id()))
                    .unwrap_or_default();
                ((d.bus_number(), d.address()), id, d)
            })
            .collect();
        first_connected(candidates, &mut rejected.0, |device| {
            Self::connect(context, device, wake)
        })
    }
    fn connect(
        context: &Context,
        mut device: Device<Context>,
        wake: &SyncSender<()>,
    ) -> Result<Self, Failure> {
        let desc = device.device_descriptor().map_err(|e| e.to_string())?;
        if !accessory(desc.vendor_id(), desc.product_id()) {
            // Until the switch request is sent, a failure means this device
            // cannot be an Android accessory here.
            let h = device.open().map_err(|e| {
                Failure::Unusable(format!("open Android USB (check udev permissions): {e}"))
            })?;
            let mut version = [0u8; 2];
            let n = h
                .read_control(
                    rusb::request_type(Direction::In, RequestType::Vendor, Recipient::Device),
                    51,
                    0,
                    0,
                    &mut version,
                    Duration::from_millis(500),
                )
                .map_err(|e| Failure::Unusable(format!("AOA protocol request: {e}")))?;
            if n != 2 || u16::from_le_bytes(version) == 0 {
                return Err(Failure::Unusable("phone does not support AOA".into()));
            }
            for (index, value) in [
                "Android",
                "Android Auto",
                "NavBridge",
                "2.0.1",
                "",
                "NavBridge-CM5",
            ]
            .iter()
            .enumerate()
            {
                let bytes = [value.as_bytes(), &[0]].concat();
                let n = h
                    .write_control(
                        rusb::request_type(Direction::Out, RequestType::Vendor, Recipient::Device),
                        52,
                        0,
                        index as u16,
                        &bytes,
                        Duration::from_millis(500),
                    )
                    .map_err(|e| e.to_string())?;
                if n != bytes.len() {
                    return Err(Failure::Retry("short AOA identity write".into()));
                }
            }
            let bus = device.bus_number();
            let ports = device.port_numbers().map_err(|e| e.to_string())?;
            h.write_control(
                rusb::request_type(Direction::Out, RequestType::Vendor, Recipient::Device),
                53,
                0,
                0,
                &[],
                Duration::from_millis(500),
            )
            .map_err(|e| e.to_string())?;
            drop(h);
            let deadline = Instant::now() + Duration::from_secs(8);
            device = loop {
                let found =
                    context
                        .devices()
                        .map_err(|e| e.to_string())?
                        .iter()
                        .find(|candidate| {
                            candidate.bus_number() == bus
                                && candidate.port_numbers().ok().as_ref() == Some(&ports)
                                && candidate
                                    .device_descriptor()
                                    .is_ok_and(|d| accessory(d.vendor_id(), d.product_id()))
                        });
                if let Some(d) = found {
                    break d;
                }
                if Instant::now() >= deadline {
                    return Err(Failure::Retry(
                        "phone did not re-enumerate in AOA mode".into(),
                    ));
                }
                thread::sleep(Duration::from_millis(100));
            };
        }
        let info = info(&device)?;
        let handle = device.open().map_err(|e| e.to_string())?;
        let config = device
            .active_config_descriptor()
            .or_else(|_| device.config_descriptor(0))
            .map_err(|e| e.to_string())?;
        let _ = handle.set_auto_detach_kernel_driver(true);
        for interface in config.interfaces() {
            for alt in interface.descriptors() {
                if alt.class_code() == 0xff && alt.sub_class_code() == 0x42 {
                    continue;
                } // Never claim ADB.
                let input = alt.endpoint_descriptors().find(|e| {
                    e.transfer_type() == TransferType::Bulk && e.direction() == Direction::In
                });
                let output = alt.endpoint_descriptors().find(|e| {
                    e.transfer_type() == TransferType::Bulk && e.direction() == Direction::Out
                });
                if let (Some(input), Some(output)) = (input, output) {
                    handle
                        .claim_interface(interface.number())
                        .map_err(|e| e.to_string())?;
                    if alt.setting_number() != 0 {
                        handle
                            .set_alternate_setting(interface.number(), alt.setting_number())
                            .map_err(|e| e.to_string())?;
                    }
                    let handle = Arc::new(handle);
                    let stopped = Arc::new(AtomicBool::new(false));
                    let (tx, received) = mpsc::sync_channel(64);
                    let (h, end, endpoint, wake) = (
                        handle.clone(),
                        stopped.clone(),
                        input.address(),
                        wake.clone(),
                    );
                    let reader = thread::spawn(move || {
                        // One URB per read avoids cancelling a second URB after
                        // short frames. Keep reads outstanding while protocol
                        // responses are written and decoded on the session thread.
                        let mut buffer = vec![0; 16384];
                        while !end.load(Ordering::Relaxed) {
                            let result =
                                match h.read_bulk(endpoint, &mut buffer, Duration::from_secs(1)) {
                                    Ok(0) | Err(rusb::Error::Timeout) => continue,
                                    Ok(n) => Ok(buffer[..n].to_vec()),
                                    Err(e) => Err(e),
                                };
                            let failed = result.is_err();
                            if !deliver(&tx, &wake, &end, result) || failed {
                                return;
                            }
                        }
                    });
                    return Ok(Self {
                        handle,
                        received,
                        stopped,
                        reader: Some(reader),
                        output: output.address(),
                        info,
                    });
                }
            }
        }
        Err(Failure::Retry(
            "AOA accessory bulk endpoints not found".into(),
        ))
    }
    pub fn reset(&self) {
        let _ = self.handle.reset();
    }
    /// The next transfer queued by the reader thread, if one has arrived.
    pub fn try_read(&self) -> Option<Result<Vec<u8>, rusb::Error>> {
        match self.received.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(rusb::Error::NoDevice)),
        }
    }
    pub fn write(&self, data: &[u8]) -> Result<(), String> {
        let mut offset = 0;
        while offset < data.len() {
            let n = self
                .handle
                .write_bulk(self.output, &data[offset..], Duration::from_millis(500))
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("zero USB write".into());
            }
            offset += n;
        }
        Ok(())
    }
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accessory_ids_exclude_audio_only_and_unrelated_devices() {
        assert!(accessory(0x18d1, 0x2d01));
        assert!(!accessory(0x18d1, 0x2d02));
        assert!(!accessory(0x08bb, 0x2912));
    }
    /// One discovery pass over fake devices: "iphone" does not speak AOA,
    /// "stuck" speaks AOA but never re-enumerates, anything else connects.
    fn pass(
        present: &[((u8, u8), &'static str)],
        rejected: &mut HashSet<(u8, u8)>,
        tried: &mut Vec<&'static str>,
    ) -> Result<Option<&'static str>, String> {
        let candidates = present
            .iter()
            .map(|&(key, name)| (key, name.to_string(), name))
            .collect();
        first_connected(candidates, rejected, |name| {
            tried.push(name);
            match name {
                "iphone" => Err(Failure::Unusable("no AOA".into())),
                "stuck" => Err(Failure::Retry("no re-enumeration".into())),
                _ => Ok(name),
            }
        })
    }
    #[test]
    fn a_failing_device_does_not_hide_a_phone_behind_it() {
        let (mut rejected, mut tried) = (HashSet::new(), Vec::new());
        let present = [((1, 5), "iphone"), ((1, 6), "stuck"), ((3, 2), "pixel")];
        assert_eq!(pass(&present, &mut rejected, &mut tried), Ok(Some("pixel")));
        assert_eq!(tried, ["iphone", "stuck", "pixel"]);
    }
    #[test]
    fn unusable_devices_wait_for_a_replug_while_setup_failures_retry() {
        let (mut rejected, mut tried) = (HashSet::new(), Vec::new());
        let both = [((1, 5), "iphone"), ((1, 6), "stuck")];
        assert_eq!(
            pass(&both, &mut rejected, &mut tried),
            Err("iphone ignored until replugged: no AOA; stuck: no re-enumeration".into())
        );
        // Still plugged in, the iPhone is neither probed nor reported again.
        assert_eq!(
            pass(&[((1, 5), "iphone")], &mut rejected, &mut tried),
            Ok(None)
        );
        assert_eq!(
            pass(&both, &mut rejected, &mut tried),
            Err("stuck: no re-enumeration".into())
        );
        assert_eq!(tried, ["iphone", "stuck", "stuck"]);
        // Unplugged devices are forgotten; replugged, they are probed again.
        assert_eq!(pass(&[], &mut rejected, &mut tried), Ok(None));
        assert!(pass(&[((1, 9), "iphone")], &mut rejected, &mut tried).is_err());
        assert_eq!(tried.last(), Some(&"iphone"));
    }
}
