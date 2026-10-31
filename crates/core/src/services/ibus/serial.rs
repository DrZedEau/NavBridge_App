//! The microcontroller's USB serial port. NavBridge never sends anything
//! towards the car: only lines for the microcontroller itself.
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
    time::Duration,
};

pub struct Port(File);

impl Port {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(path)?;
        let fd = file.as_raw_fd();
        // SAFETY: terminal settings of the descriptor `file` owns.
        unsafe {
            let mut settings: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut settings) < 0 {
                return Err(io::Error::last_os_error());
            }
            // Bytes as they come, and nothing echoed back to the board.
            libc::cfmakeraw(&mut settings);
            // A port closed at 1200 baud restarts the ATmega32U4 into its
            // bootloader; USB ignores the rate otherwise.
            libc::cfsetispeed(&mut settings, libc::B115200);
            libc::cfsetospeed(&mut settings, libc::B115200);
            if libc::tcsetattr(fd, libc::TCSANOW, &settings) < 0 {
                return Err(io::Error::last_os_error());
            }
            // Nobody else, such as GPSD looking for receivers, opens it meanwhile.
            libc::ioctl(fd, libc::TIOCEXCL);
        }
        Ok(Self(file))
    }

    /// A line for the microcontroller, such as "KEYS OFF".
    pub fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.0.write_all(format!("{line}\n").as_bytes())
    }

    /// Waits up to `limit` for bytes and returns how many arrived, perhaps
    /// none. Unplugging the board is an error.
    pub fn read(&mut self, buffer: &mut [u8], limit: Duration) -> io::Result<usize> {
        let mut fd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd, for the port's open descriptor.
        let ready = unsafe { libc::poll(&mut fd, 1, limit.as_millis() as libc::c_int) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted {
                Ok(0)
            } else {
                Err(e)
            };
        }
        if ready == 0 {
            return Ok(0);
        }
        if fd.revents & libc::POLLIN == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the interface went away",
            ));
        }
        match self.0.read(buffer)? {
            0 => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the interface went away",
            )),
            n => Ok(n),
        }
    }
}
