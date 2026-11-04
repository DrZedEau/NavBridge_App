//! Native CarPlay, with Apple's MFi authentication coprocessor on the board.
//!
//! An iPhone starts CarPlay only with an accessory whose coprocessor signs
//! its challenge. The layers, from the board up:
//!
//! 1. `mfi`: the coprocessor on I²C: its certificate and challenge
//!    signatures. Written.
//! 2. `iap2`: Apple's accessory protocol. Its link layer and control session
//!    framing, and authentication, are written; reliable delivery and
//!    identification are next.
//! 3. Transports: wired, `usb` puts the iPhone in its CarPlay configuration
//!    (written); iAP2 then reaches the iPhone through usbmux and the
//!    iPhone's CarPlay service, and the session's IP traffic goes over the
//!    configuration's NCM network link. Wireless, iAP2 runs over Bluetooth
//!    RFCOMM (service UUID `00000000-deca-fade-deca-deafdecacafe`, registered
//!    beside Android Auto's wireless service), hands the iPhone the hotspot's
//!    credentials, and the session runs over Wi-Fi.
//! 4. The CarPlay session over IP, found by mDNS: pairing, an encrypted
//!    control channel, and the screen's H.264 (into the projection's
//!    `VideoPipeline`), audio (into the audio service) and touch and knob
//!    input (HID) streams.
//!
//! The iPhone's video and the projection share the UI's projection view, and
//! the session keeps the same rules as Android Auto's: one phone at a time,
//! USB first.
//!
//! Hardware, for the board: the MFi coprocessor (Apple's MFi programme
//! supplies the part and its datasheet) on one of the Pi's I²C buses, at 3.3 V,
//! its address pin setting 0x10 or 0x11. A GPIO that can power-cycle or reset
//! it lets the core recover a coprocessor that stops answering. The iPhone's
//! USB port stays on the Pi's USB host, as for Android Auto.
//!
//! The core does not start CarPlay yet. Once it does, Android Auto's USB
//! discovery must leave Apple's devices alone: an iPhone's PTP interface
//! looks like a phone to it.
//!
//! The protocol facts follow LIVI by Lasse Heitgres
//! (<https://github.com/f-io/LIVI>, GPL-3.0-or-later); NavBridge's code is its
//! own. LIVI's terms ask any work that contains its code, even translated, to
//! show "Based on LIVI by Lasse Heitgres (https://github.com/f-io/LIVI)" in
//! its legal notices and documentation: should code from it be ported, that
//! line goes in the README and on the System page.

// Until the core starts CarPlay.
#![allow(dead_code)]

pub mod iap2;
pub mod mfi;
#[cfg(test)]
mod tests;
pub mod usb;

use std::path::PathBuf;

use serde::Deserialize;

/// `[carplay]` in core.toml.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CarPlayConfig {
    pub enabled: bool,
    /// The I²C bus the coprocessor is on.
    pub mfi_bus: PathBuf,
}

impl Default for CarPlayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mfi_bus: PathBuf::from("/dev/i2c-1"),
        }
    }
}
