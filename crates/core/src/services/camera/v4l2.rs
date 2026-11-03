//! The few parts of the kernel's V4L2 interface (`linux/videodev2.h`) the
//! camera needs: capabilities, formats, memory-mapped buffers and streaming.
//!
//! The layouts are those of 64-bit Linux, the only kind NavBridge runs on
//! (aarch64 and x86_64); the tests check them against the kernel's ioctl
//! numbers, which encode each structure's size.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

pub const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

pub const PIX_FMT_YUYV: u32 = fourcc(b"YUYV");
pub const PIX_FMT_UYVY: u32 = fourcc(b"UYVY");
pub const PIX_FMT_MJPEG: u32 = fourcc(b"MJPG");

pub const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
pub const CAP_STREAMING: u32 = 0x0400_0000;
pub const CAP_DEVICE_CAPS: u32 = 0x8000_0000;

pub const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
pub const MEMORY_MMAP: u32 = 1;
pub const BUF_FLAG_ERROR: u32 = 0x40;

pub const FIELD_ANY: u32 = 0;
pub const FIELD_NONE: u32 = 1;
pub const FIELD_TOP: u32 = 2;
pub const FIELD_BOTTOM: u32 = 3;
pub const FIELD_INTERLACED: u32 = 4;
pub const FIELD_SEQ_TB: u32 = 5;
pub const FIELD_SEQ_BT: u32 = 6;
pub const FIELD_ALTERNATE: u32 = 7;
pub const FIELD_INTERLACED_TB: u32 = 8;
pub const FIELD_INTERLACED_BT: u32 = 9;

pub const COLORSPACE_DEFAULT: u32 = 0;
pub const COLORSPACE_SMPTE170M: u32 = 1;
pub const COLORSPACE_REC709: u32 = 3;
pub const COLORSPACE_JPEG: u32 = 7;
pub const COLORSPACE_SRGB: u32 = 8;

pub const YCBCR_ENC_DEFAULT: u32 = 0;
pub const YCBCR_ENC_601: u32 = 1;
pub const YCBCR_ENC_709: u32 = 2;

pub const QUANTIZATION_DEFAULT: u32 = 0;
pub const QUANTIZATION_FULL_RANGE: u32 = 1;
pub const QUANTIZATION_LIM_RANGE: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Capability {
    pub driver: [u8; 16],
    pub card: [u8; 32],
    pub bus_info: [u8; 32],
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
    pub reserved: [u32; 3],
}

impl Capability {
    /// What this device node can do: its own capabilities when the driver
    /// gives them, else the whole device's.
    pub fn node_caps(&self) -> u32 {
        if self.capabilities & CAP_DEVICE_CAPS != 0 {
            self.device_caps
        } else {
            self.capabilities
        }
    }

    pub fn card(&self) -> String {
        let end = self.card.iter().position(|&b| b == 0).unwrap_or(32);
        String::from_utf8_lossy(&self.card[..end]).into_owned()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FmtDesc {
    pub index: u32,
    pub type_: u32,
    pub flags: u32,
    pub description: [u8; 32],
    pub pixelformat: u32,
    pub mbus_code: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PixFormat {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub bytesperline: u32,
    pub sizeimage: u32,
    pub colorspace: u32,
    pub priv_: u32,
    pub flags: u32,
    pub ycbcr_enc: u32,
    pub quantization: u32,
    pub xfer_func: u32,
}

/// `struct v4l2_format`: its union holds pointers in some members, hence the
/// 8-byte alignment that puts it at offset 8.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Format {
    pub type_: u32,
    pub fmt: FormatUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union FormatUnion {
    pub pix: PixFormat,
    pub raw: [u64; 25],
}

impl Format {
    pub fn capture(pix: PixFormat) -> Self {
        let mut format = Self {
            type_: BUF_TYPE_VIDEO_CAPTURE,
            fmt: FormatUnion { raw: [0; 25] },
        };
        format.fmt.pix = pix;
        format
    }

    pub fn pix(&self) -> PixFormat {
        // SAFETY: every bit pattern is a valid `PixFormat`.
        unsafe { self.fmt.pix }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RequestBuffers {
    pub count: u32,
    pub type_: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timecode {
    pub type_: u32,
    pub flags: u32,
    pub frames: u8,
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    pub userbits: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Buffer {
    pub index: u32,
    pub type_: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub timestamp: libc::timeval,
    pub timecode: Timecode,
    pub sequence: u32,
    pub memory: u32,
    /// The union `m`: for memory-mapped buffers, the offset to map.
    pub offset: u64,
    pub length: u32,
    pub reserved2: u32,
    pub request_fd: i32,
}

impl Buffer {
    pub fn mmap_capture(index: u32) -> Self {
        Self {
            index,
            type_: BUF_TYPE_VIDEO_CAPTURE,
            bytesused: 0,
            flags: 0,
            field: 0,
            timestamp: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            timecode: Timecode::default(),
            sequence: 0,
            memory: MEMORY_MMAP,
            offset: 0,
            length: 0,
            reserved2: 0,
            request_fd: 0,
        }
    }
}

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr
}

pub const VIDIOC_QUERYCAP: u32 = ioc(IOC_READ, 0, size_of::<Capability>());
pub const VIDIOC_ENUM_FMT: u32 = ioc(IOC_READ | IOC_WRITE, 2, size_of::<FmtDesc>());
pub const VIDIOC_G_FMT: u32 = ioc(IOC_READ | IOC_WRITE, 4, size_of::<Format>());
pub const VIDIOC_S_FMT: u32 = ioc(IOC_READ | IOC_WRITE, 5, size_of::<Format>());
pub const VIDIOC_REQBUFS: u32 = ioc(IOC_READ | IOC_WRITE, 8, size_of::<RequestBuffers>());
pub const VIDIOC_QUERYBUF: u32 = ioc(IOC_READ | IOC_WRITE, 9, size_of::<Buffer>());
pub const VIDIOC_QBUF: u32 = ioc(IOC_READ | IOC_WRITE, 15, size_of::<Buffer>());
pub const VIDIOC_DQBUF: u32 = ioc(IOC_READ | IOC_WRITE, 17, size_of::<Buffer>());
pub const VIDIOC_STREAMON: u32 = ioc(IOC_WRITE, 18, size_of::<i32>());
pub const VIDIOC_STREAMOFF: u32 = ioc(IOC_WRITE, 19, size_of::<i32>());

/// Runs one V4L2 ioctl, again when a signal interrupts it.
///
/// # Safety
/// `request` must be an ioctl whose argument is a `T`.
unsafe fn ioctl<T>(fd: BorrowedFd<'_>, request: u32, argument: &mut T) -> io::Result<()> {
    loop {
        // The request's type differs between C libraries.
        if libc::ioctl(fd.as_raw_fd(), request as _, argument as *mut T) >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub fn query_capability(fd: BorrowedFd<'_>) -> io::Result<Capability> {
    let mut capability = Capability::default();
    // SAFETY: VIDIOC_QUERYCAP takes a `struct v4l2_capability`.
    unsafe { ioctl(fd, VIDIOC_QUERYCAP, &mut capability) }?;
    Ok(capability)
}

/// The pixel formats the device captures in, in its own order of preference.
pub fn pixel_formats(fd: BorrowedFd<'_>) -> io::Result<Vec<u32>> {
    let mut formats = Vec::new();
    for index in 0.. {
        let mut desc = FmtDesc {
            index,
            type_: BUF_TYPE_VIDEO_CAPTURE,
            ..Default::default()
        };
        // SAFETY: VIDIOC_ENUM_FMT takes a `struct v4l2_fmtdesc`.
        match unsafe { ioctl(fd, VIDIOC_ENUM_FMT, &mut desc) } {
            Ok(()) => formats.push(desc.pixelformat),
            // The end of the list.
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(formats)
}

pub fn get_format(fd: BorrowedFd<'_>) -> io::Result<PixFormat> {
    let mut format = Format::capture(PixFormat::default());
    // SAFETY: VIDIOC_G_FMT takes a `struct v4l2_format`.
    unsafe { ioctl(fd, VIDIOC_G_FMT, &mut format) }?;
    Ok(format.pix())
}

/// Asks for `pix`; the driver answers with the closest format it can give.
pub fn set_format(fd: BorrowedFd<'_>, pix: PixFormat) -> io::Result<PixFormat> {
    let mut format = Format::capture(pix);
    // SAFETY: VIDIOC_S_FMT takes a `struct v4l2_format`.
    unsafe { ioctl(fd, VIDIOC_S_FMT, &mut format) }?;
    Ok(format.pix())
}

/// Asks for `count` memory-mapped buffers (0 frees them); returns how many the
/// driver gave.
pub fn request_buffers(fd: BorrowedFd<'_>, count: u32) -> io::Result<u32> {
    let mut request = RequestBuffers {
        count,
        type_: BUF_TYPE_VIDEO_CAPTURE,
        memory: MEMORY_MMAP,
        ..Default::default()
    };
    // SAFETY: VIDIOC_REQBUFS takes a `struct v4l2_requestbuffers`.
    unsafe { ioctl(fd, VIDIOC_REQBUFS, &mut request) }?;
    Ok(request.count)
}

pub fn query_buffer(fd: BorrowedFd<'_>, index: u32) -> io::Result<Buffer> {
    let mut buffer = Buffer::mmap_capture(index);
    // SAFETY: VIDIOC_QUERYBUF takes a `struct v4l2_buffer`.
    unsafe { ioctl(fd, VIDIOC_QUERYBUF, &mut buffer) }?;
    Ok(buffer)
}

pub fn queue_buffer(fd: BorrowedFd<'_>, index: u32) -> io::Result<()> {
    let mut buffer = Buffer::mmap_capture(index);
    // SAFETY: VIDIOC_QBUF takes a `struct v4l2_buffer`.
    unsafe { ioctl(fd, VIDIOC_QBUF, &mut buffer) }
}

/// Takes the next filled buffer; `WouldBlock` when none is ready.
pub fn dequeue_buffer(fd: BorrowedFd<'_>) -> io::Result<Buffer> {
    let mut buffer = Buffer::mmap_capture(0);
    // SAFETY: VIDIOC_DQBUF takes a `struct v4l2_buffer`.
    unsafe { ioctl(fd, VIDIOC_DQBUF, &mut buffer) }?;
    Ok(buffer)
}

pub fn stream(fd: BorrowedFd<'_>, on: bool) -> io::Result<()> {
    let mut kind = BUF_TYPE_VIDEO_CAPTURE as i32;
    let request = if on {
        VIDIOC_STREAMON
    } else {
        VIDIOC_STREAMOFF
    };
    // SAFETY: VIDIOC_STREAMON and VIDIOC_STREAMOFF take an `int`.
    unsafe { ioctl(fd, request, &mut kind) }
}
