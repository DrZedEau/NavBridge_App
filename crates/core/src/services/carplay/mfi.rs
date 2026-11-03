//! Apple's MFi authentication coprocessor, on one of the Pi's I²C buses: it
//! holds the accessory's certificate and signs the iPhone's challenges, which
//! is how an iPhone tells a CarPlay head unit from anything else.
//!
//! The coprocessor answers at 0x10 or 0x11, as its address pin sets. It
//! refuses (NAKs) its address while busy, so every transfer is retried, every
//! 500 µs, for up to 2 seconds. Registers are chosen by writing their number,
//! then read in a transfer of their own; multi-byte lengths are big-endian.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// The addresses the coprocessor can answer at.
pub const ADDRESSES: [u16; 2] = [0x10, 0x11];

pub const REG_DEVICE_VERSION: u8 = 0x00;
pub const REG_PROTOCOL_MAJOR: u8 = 0x02;
pub const REG_ERROR_CODE: u8 = 0x05;
pub const REG_AUTH_CONTROL_STATUS: u8 = 0x10;
pub const REG_SIGNATURE_LENGTH: u8 = 0x11;
pub const REG_SIGNATURE_DATA: u8 = 0x12;
pub const REG_CHALLENGE_LENGTH: u8 = 0x20;
pub const REG_CHALLENGE_DATA: u8 = 0x21;
pub const REG_CERTIFICATE_LENGTH: u8 = 0x30;
pub const REG_CERTIFICATE_DATA: u8 = 0x31;

/// Written to the control register: sign the challenge.
pub const AUTH_START: u8 = 0x01;
/// Read from the status register once the signature is ready.
pub const AUTH_DONE: u8 = 0x10;

/// The challenge sizes the coprocessor signs.
pub const CHALLENGE_SIZES: std::ops::RangeInclusive<usize> = 1..=128;

const BUSY_RETRY: Duration = Duration::from_micros(500);
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const SIGN_POLL: Duration = Duration::from_millis(10);
const SIGN_TIMEOUT: Duration = Duration::from_secs(3);

/// Register access to a coprocessor: the I²C device on the board, or a
/// stand-in in the tests.
pub trait Registers {
    fn read(&mut self, register: u8, length: usize) -> io::Result<Vec<u8>>;
    fn write(&mut self, register: u8, data: &[u8]) -> io::Result<()>;
}

/// The coprocessor's answers, whichever registers reach it.
pub struct Coprocessor<R> {
    registers: R,
    /// The authentication protocol's major version: 3 for the 3.0 chip.
    pub protocol_major: u8,
}

impl<R: Registers> Coprocessor<R> {
    pub fn new(mut registers: R) -> io::Result<Self> {
        let protocol_major = registers.read(REG_PROTOCOL_MAJOR, 1)?[0];
        Ok(Self {
            registers,
            protocol_major,
        })
    }

    fn length(&mut self, register: u8) -> io::Result<usize> {
        let bytes = self.registers.read(register, 2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]) as usize)
    }

    /// The accessory certificate the iPhone asks for first.
    pub fn certificate(&mut self) -> io::Result<Vec<u8>> {
        let length = self.length(REG_CERTIFICATE_LENGTH)?;
        if length == 0 {
            return Err(io::Error::other("the coprocessor holds no certificate"));
        }
        self.registers.read(REG_CERTIFICATE_DATA, length)
    }

    /// The coprocessor's signature of the iPhone's `challenge`.
    pub fn sign(&mut self, challenge: &[u8]) -> io::Result<Vec<u8>> {
        if !CHALLENGE_SIZES.contains(&challenge.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a challenge of {} bytes cannot be signed", challenge.len()),
            ));
        }
        let length = challenge.len() as u16;
        self.registers
            .write(REG_CHALLENGE_LENGTH, &length.to_be_bytes())?;
        self.registers.write(REG_CHALLENGE_DATA, challenge)?;
        self.registers
            .write(REG_AUTH_CONTROL_STATUS, &[AUTH_START])?;
        let deadline = Instant::now() + SIGN_TIMEOUT;
        loop {
            sleep(SIGN_POLL);
            // Busy while it signs: a failed read is asked again.
            if let Ok(status) = self.registers.read(REG_AUTH_CONTROL_STATUS, 1) {
                if status[0] == AUTH_DONE {
                    break;
                }
            }
            if Instant::now() > deadline {
                let code = self.registers.read(REG_ERROR_CODE, 1).map(|code| code[0]);
                return Err(io::Error::other(match code {
                    Ok(code) => format!("the coprocessor did not sign (error code 0x{code:02X})"),
                    Err(_) => "the coprocessor did not sign".into(),
                }));
            }
        }
        let length = self.length(REG_SIGNATURE_LENGTH)?;
        self.registers.read(REG_SIGNATURE_DATA, length)
    }
}

/// `I2C_SLAVE` from `linux/i2c-dev.h`: the address later reads and writes go to.
const I2C_SLAVE: libc::c_ulong = 0x0703;

/// The coprocessor through the kernel's `i2c-dev`, at `/dev/i2c-N`.
pub struct I2cRegisters {
    file: File,
}

impl I2cRegisters {
    /// Opens `bus` and finds the coprocessor at one of its two addresses.
    pub fn probe(bus: &Path) -> io::Result<(Self, u16)> {
        for address in ADDRESSES {
            let file = OpenOptions::new().read(true).write(true).open(bus)?;
            // SAFETY: I2C_SLAVE takes the address as its argument.
            if unsafe {
                libc::ioctl(
                    file.as_raw_fd(),
                    I2C_SLAVE as _,
                    libc::c_ulong::from(address),
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut registers = Self { file };
            if registers.read(REG_DEVICE_VERSION, 1).is_ok() {
                return Ok((registers, address));
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no MFi coprocessor answers on {} at 0x10 or 0x11",
                bus.display()
            ),
        ))
    }

    /// Runs `transfer` until the coprocessor takes it.
    fn retry<T>(&mut self, mut transfer: impl FnMut(&mut File) -> io::Result<T>) -> io::Result<T> {
        let deadline = Instant::now() + BUSY_TIMEOUT;
        loop {
            match transfer(&mut self.file) {
                Ok(value) => return Ok(value),
                Err(e) if Instant::now() > deadline => return Err(e),
                Err(_) => sleep(BUSY_RETRY),
            }
        }
    }
}

impl Registers for I2cRegisters {
    fn read(&mut self, register: u8, length: usize) -> io::Result<Vec<u8>> {
        self.retry(|file| file.write_all(&[register]))?;
        let mut data = vec![0; length];
        self.retry(|file| file.read_exact(&mut data))?;
        Ok(data)
    }

    fn write(&mut self, register: u8, data: &[u8]) -> io::Result<()> {
        let mut transfer = Vec::with_capacity(data.len() + 1);
        transfer.push(register);
        transfer.extend_from_slice(data);
        self.retry(|file| file.write_all(&transfer))
    }
}
