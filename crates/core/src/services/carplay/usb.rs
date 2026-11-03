//! iPhones on USB. The Pi stays the USB host, as for Android Auto: an iPhone
//! offers several USB configurations, and its CarPlay one adds a network
//! (CDC NCM) interface, which the kernel's `cdc_ncm` driver turns into an
//! Ethernet link for the CarPlay session, beside the usbmux interface that
//! reaches the iPhone's services. The configuration is chosen through sysfs.
//!
//! Writing `bConfigurationValue` needs root, or a udev rule giving the core's
//! group write access to it for Apple's devices.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const SYSFS_USB_DEVICES: &str = "/sys/bus/usb/devices";
pub const APPLE_VENDOR: u16 = 0x05AC;
/// The configuration an iPhone starts in: PTP, usbmux and others.
pub const DEFAULT_CONFIGURATION: u8 = 4;
/// The one that adds CarPlay's network interface.
pub const CARPLAY_CONFIGURATION: u8 = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Iphone {
    /// Its directory in sysfs.
    pub sysfs: PathBuf,
    pub serial: Option<String>,
    /// The configuration in use, if any.
    pub configuration: Option<u8>,
}

fn attribute(device: &Path, name: &str) -> Option<String> {
    fs::read_to_string(device.join(name))
        .ok()
        .map(|text| text.trim().to_owned())
}

/// The Apple devices under `root` (sysfs's USB devices) that offer the
/// CarPlay configuration, in the order of their names.
pub fn iphones(root: &Path) -> io::Result<Vec<Iphone>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(root)? {
        let sysfs = entry?.path();
        // Interfaces ("1-1:1.0") have no vendor; root hubs are not Apple's.
        let vendor = attribute(&sysfs, "idVendor").and_then(|v| u16::from_str_radix(&v, 16).ok());
        let configurations =
            attribute(&sysfs, "bNumConfigurations").and_then(|n| n.parse::<u8>().ok());
        let carplay = configurations.is_some_and(|n| n >= CARPLAY_CONFIGURATION);
        if vendor != Some(APPLE_VENDOR) || !carplay {
            continue;
        }
        found.push(Iphone {
            configuration: attribute(&sysfs, "bConfigurationValue").and_then(|c| c.parse().ok()),
            serial: attribute(&sysfs, "serial"),
            sysfs,
        });
    }
    found.sort_by(|a, b| a.sysfs.cmp(&b.sysfs));
    Ok(found)
}

/// Puts the iPhone in `configuration`; returns whether it changed. The
/// iPhone then comes back with that configuration's interfaces.
pub fn select(iphone: &Iphone, configuration: u8) -> io::Result<bool> {
    if iphone.configuration == Some(configuration) {
        return Ok(false);
    }
    fs::write(
        iphone.sysfs.join("bConfigurationValue"),
        configuration.to_string(),
    )?;
    Ok(true)
}
