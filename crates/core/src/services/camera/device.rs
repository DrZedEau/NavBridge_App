//! A V4L2 capture device: opened and its format agreed, then streamed through
//! memory-mapped buffers while the car is in reverse.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr::NonNull;
use std::time::Duration;

use super::format::{choose_packing, request, Packing};
use super::v4l2::{self, PixFormat};

/// Buffers asked of the driver: one being filled, one being read, and spares
/// so that a slow conversion does not make the driver drop pictures.
const BUFFER_COUNT: u32 = 4;

pub struct Device {
    file: File,
    /// The card's name, as the driver gives it.
    pub name: String,
    pub format: PixFormat,
    pub packing: Packing,
    buffers: Vec<Mapping>,
}

/// One of the driver's buffers, mapped read-only into the core.
struct Mapping {
    data: NonNull<u8>,
    length: usize,
}

// SAFETY: the mapping is only read, by the thread owning the `Device`.
unsafe impl Send for Mapping {}

impl Mapping {
    fn new(file: &File, length: usize, offset: u64) -> io::Result<Self> {
        // SAFETY: maps a buffer the driver has just described; it stays valid
        // until unmapped in `drop`.
        let data = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                offset as libc::off_t,
            )
        };
        if data == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let data = NonNull::new(data.cast()).ok_or_else(|| io::Error::other("mmap gave null"))?;
        Ok(Self { data, length })
    }

    fn bytes(&self, used: usize) -> &[u8] {
        // SAFETY: the mapping is `length` bytes long and lives as long as self.
        unsafe { std::slice::from_raw_parts(self.data.as_ptr(), used.min(self.length)) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly what `new` mapped.
        unsafe { libc::munmap(self.data.as_ptr().cast(), self.length) };
    }
}

impl Device {
    /// Opens the capture device at `path` and agrees a format with it.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let fd = file.as_fd();
        let capability = v4l2::query_capability(fd)?;
        let caps = capability.node_caps();
        if caps & v4l2::CAP_VIDEO_CAPTURE == 0 || caps & v4l2::CAP_STREAMING == 0 {
            return Err(io::Error::other(format!(
                "{} is not a streaming video capture device",
                path.display()
            )));
        }
        let offered = v4l2::pixel_formats(fd)?;
        let packing = choose_packing(&offered).ok_or_else(|| {
            let names: Vec<String> = offered.iter().map(|&code| fourcc_text(code)).collect();
            io::Error::other(format!(
                "the camera offers no YUYV or UYVY picture, only {}",
                names.join(", ")
            ))
        })?;
        let mut format = v4l2::set_format(fd, request(packing))?;
        if format.pixelformat != packing.fourcc() {
            return Err(io::Error::other(format!(
                "the camera gave {} when asked for {}",
                fourcc_text(format.pixelformat),
                fourcc_text(packing.fourcc())
            )));
        }
        let row = format.width * Packing::BYTES_PER_PIXEL as u32;
        format.bytesperline = format.bytesperline.max(row);
        Ok(Self {
            name: capability.card(),
            file,
            format,
            packing,
            buffers: Vec::new(),
        })
    }

    pub fn streaming(&self) -> bool {
        !self.buffers.is_empty()
    }

    /// Maps the driver's buffers, hands them all to it and starts capturing.
    pub fn start(&mut self) -> io::Result<()> {
        if self.streaming() {
            return Ok(());
        }
        let result = self.map_and_stream();
        if result.is_err() {
            let _ = self.stop();
        }
        result
    }

    fn map_and_stream(&mut self) -> io::Result<()> {
        let fd = self.file.as_fd();
        let count = v4l2::request_buffers(fd, BUFFER_COUNT)?;
        if count < 2 {
            return Err(io::Error::other(format!(
                "the camera gave {count} buffer(s), at least 2 are needed"
            )));
        }
        for index in 0..count {
            let buffer = v4l2::query_buffer(fd, index)?;
            self.buffers.push(Mapping::new(
                &self.file,
                buffer.length as usize,
                buffer.offset,
            )?);
            v4l2::queue_buffer(fd, index)?;
        }
        v4l2::stream(fd, true)
    }

    /// Stops capturing and gives the buffers back, keeping the device open
    /// and its format agreed for the next start.
    pub fn stop(&mut self) -> io::Result<()> {
        if !self.streaming() {
            return Ok(());
        }
        let fd = self.file.as_fd();
        let stopped = v4l2::stream(fd, false);
        // The driver frees its buffers only once nothing maps them.
        self.buffers.clear();
        v4l2::request_buffers(fd, 0)?;
        stopped
    }

    /// Waits up to `timeout` for the next picture and hands `read` its bytes
    /// and field, then gives the buffer back to the driver. `None` when no
    /// picture came, or the driver marked it damaged.
    pub fn next<R>(
        &mut self,
        timeout: Duration,
        read: impl FnOnce(&[u8], u32) -> R,
    ) -> io::Result<Option<R>> {
        let fd = self.file.as_fd();
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: polls one descriptor, which `poll` describes.
        match unsafe { libc::poll(&mut poll, 1, millis) } {
            0 => return Ok(None),
            n if n < 0 => {
                let error = io::Error::last_os_error();
                return match error.kind() {
                    io::ErrorKind::Interrupted => Ok(None),
                    _ => Err(error),
                };
            }
            _ if poll.revents & libc::POLLERR != 0 => {
                return Err(io::Error::other("the camera reported an error"))
            }
            _ => {}
        }
        let buffer = match v4l2::dequeue_buffer(fd) {
            Ok(buffer) => buffer,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        };
        let mapping = self
            .buffers
            .get(buffer.index as usize)
            .ok_or_else(|| io::Error::other("the camera returned an unknown buffer"))?;
        let result = (buffer.flags & v4l2::BUF_FLAG_ERROR == 0)
            .then(|| read(mapping.bytes(buffer.bytesused as usize), buffer.field));
        v4l2::queue_buffer(fd, buffer.index)?;
        Ok(result)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// A pixel format's four characters, as V4L2 tools print them.
pub fn fourcc_text(code: u32) -> String {
    code.to_le_bytes()
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect()
}
