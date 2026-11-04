//! iAP2, the accessory protocol CarPlay starts with, over USB or Bluetooth.
//!
//! - `link`: packets, their checksums and the link's synchronization;
//! - `control`: the control session's messages and parameters;
//! - `authentication`: proving the accessory to the iPhone with the MFi
//!   coprocessor.
//!
//! Still to write, in order: the link's reliable delivery (sequence numbers,
//! acknowledgements, retransmission, from the SYN's limits); identification
//! (`IdentificationInformation`: the accessory's name, model, maker, serial,
//! firmware and hardware versions, the messages it sends and receives, its
//! transport components (USB host, Bluetooth), and the CarPlay, location and
//! vehicle status components); then the CarPlay start messages and, for
//! wireless CarPlay, the Wi-Fi credentials of the hotspot.

pub mod authentication;
pub mod control;
pub mod link;

/// The sessions the accessory offers in its SYN.
pub const CONTROL_SESSION: u8 = 10;
pub const EXTERNAL_ACCESSORY_SESSION: u8 = 11;
pub const FILE_TRANSFER_SESSION: u8 = 12;
