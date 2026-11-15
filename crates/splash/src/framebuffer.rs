//! The kernel's framebuffer (`/dev/fb0`), which the Pi's display driver
//! provides for the text console until a program such as the UI takes the
//! display. What is written to it stays on the panel until then.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::picture::Picture;

/// One colour's place in a pixel: from `offset`, `length` bits.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bitfield {
    pub offset: u32,
    pub length: u32,
    pub msb_right: u32,
}

/// `struct fb_var_screeninfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VarScreenInfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub red: Bitfield,
    pub green: Bitfield,
    pub blue: Bitfield,
    pub transp: Bitfield,
    pub nonstd: u32,
    pub activate: u32,
    pub height: u32,
    pub width: u32,
    pub accel_flags: u32,
    pub pixclock: u32,
    pub left_margin: u32,
    pub right_margin: u32,
    pub upper_margin: u32,
    pub lower_margin: u32,
    pub hsync_len: u32,
    pub vsync_len: u32,
    pub sync: u32,
    pub vmode: u32,
    pub rotate: u32,
    pub colorspace: u32,
    pub reserved: [u32; 4],
}

/// `struct fb_fix_screeninfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FixScreenInfo {
    pub id: [u8; 16],
    pub smem_start: libc::c_ulong,
    pub smem_len: u32,
    pub type_: u32,
    pub type_aux: u32,
    pub visual: u32,
    pub xpanstep: u16,
    pub ypanstep: u16,
    pub ywrapstep: u16,
    pub line_length: u32,
    pub mmio_start: libc::c_ulong,
    pub mmio_len: u32,
    pub accel: u32,
    pub capabilities: u16,
    pub reserved: [u16; 2],
}

pub const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
pub const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;
const FB_TYPE_PACKED_PIXELS: u32 = 0;
const FB_VISUAL_TRUECOLOR: u32 = 2;
const FB_VISUAL_DIRECTCOLOR: u32 = 4;

/// How the panel's pixels are laid out in the framebuffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub bytes_per_pixel: usize,
    /// Bytes from one row to the next.
    pub stride: usize,
    /// Where the shown part starts.
    pub offset: u64,
    pub red: Bitfield,
    pub green: Bitfield,
    pub blue: Bitfield,
    pub transp: Bitfield,
}

impl Layout {
    pub fn new(var: &VarScreenInfo, fix: &FixScreenInfo) -> Result<Self, String> {
        if fix.type_ != FB_TYPE_PACKED_PIXELS
            || !matches!(fix.visual, FB_VISUAL_TRUECOLOR | FB_VISUAL_DIRECTCOLOR)
        {
            return Err(format!(
                "the framebuffer is not true colour (type {}, visual {})",
                fix.type_, fix.visual
            ));
        }
        if !matches!(var.bits_per_pixel, 16 | 24 | 32) {
            return Err(format!(
                "{} bits per pixel are not supported",
                var.bits_per_pixel
            ));
        }
        let bytes_per_pixel = var.bits_per_pixel as usize / 8;
        let stride = fix.line_length as usize;
        if var.xres == 0 || var.yres == 0 || stride < var.xres as usize * bytes_per_pixel {
            return Err(format!(
                "the framebuffer's geometry is invalid: {}x{}, {stride} bytes a row",
                var.xres, var.yres
            ));
        }
        Ok(Self {
            width: var.xres,
            height: var.yres,
            bytes_per_pixel,
            stride,
            offset: var.yoffset as u64 * stride as u64
                + var.xoffset as u64 * bytes_per_pixel as u64,
            red: var.red,
            green: var.green,
            blue: var.blue,
            transp: var.transp,
        })
    }

    /// A pixel of colour `rgb` in this layout, as its value.
    pub fn pixel(&self, [r, g, b]: [u8; 3]) -> u32 {
        // Each colour scaled to its field's bits; transparency fully opaque.
        let field = |value: u8, field: Bitfield| {
            if field.length == 0 || field.length > 16 {
                return 0;
            }
            let max = (1u32 << field.length) - 1;
            ((u32::from(value) * max + 127) / 255) << field.offset
        };
        let opaque = match self.transp.length {
            0 => 0,
            length => ((1u32 << length.min(16)) - 1) << self.transp.offset,
        };
        field(r, self.red) | field(g, self.green) | field(b, self.blue) | opaque
    }

    /// The bytes of the shown area for `picture`, which must have this
    /// layout's size; the rest of each row is black.
    pub fn render(&self, picture: &Picture) -> Vec<u8> {
        debug_assert_eq!((picture.width, picture.height), (self.width, self.height));
        let mut bytes = vec![0; self.stride * self.height as usize];
        for (row, pixels) in bytes
            .chunks_exact_mut(self.stride)
            .zip(picture.rgb.chunks_exact(self.width as usize * 3))
        {
            for (target, rgb) in row
                .chunks_exact_mut(self.bytes_per_pixel)
                .zip(pixels.chunks_exact(3))
            {
                let value = self.pixel([rgb[0], rgb[1], rgb[2]]).to_le_bytes();
                target.copy_from_slice(&value[..self.bytes_per_pixel]);
            }
        }
        bytes
    }
}

pub struct Framebuffer {
    file: File,
    pub layout: Layout,
    /// The framebuffer's memory, in bytes.
    size: u64,
}

impl Framebuffer {
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        let mut var = VarScreenInfo::default();
        let mut fix = FixScreenInfo::default();
        // SAFETY: each ioctl fills the structure it is given, whose layout is
        // the kernel's.
        let read = unsafe {
            libc::ioctl(file.as_raw_fd(), FBIOGET_VSCREENINFO as _, &mut var) >= 0
                && libc::ioctl(file.as_raw_fd(), FBIOGET_FSCREENINFO as _, &mut fix) >= 0
        };
        if !read {
            return Err(format!(
                "{} is not a framebuffer: {}",
                path.display(),
                io::Error::last_os_error()
            ));
        }
        Ok(Self {
            file,
            layout: Layout::new(&var, &fix)?,
            size: u64::from(fix.smem_len),
        })
    }

    /// Shows `picture`, stretched to the panel's size.
    pub fn show(&self, picture: &Picture) -> Result<(), String> {
        let layout = &self.layout;
        let bytes = layout.render(&picture.scaled(layout.width, layout.height));
        if layout.offset + bytes.len() as u64 > self.size {
            return Err("the shown area lies outside the framebuffer".into());
        }
        self.file
            .write_all_at(&bytes, layout.offset)
            .map_err(|e| format!("write the framebuffer: {e}"))
    }
}

/// Stops the kernel's text console drawing on the framebuffer, so that
/// neither boot messages nor the console's cursor cover the splash, and the
/// splash comes back when the UI gives the display up.
pub fn release_console(vtconsoles: &Path) -> io::Result<()> {
    for entry in fs::read_dir(vtconsoles)? {
        let console = entry?.path();
        let name = fs::read_to_string(console.join("name")).unwrap_or_default();
        if name.contains("frame buffer") {
            fs::write(console.join("bind"), "0")?;
        }
    }
    Ok(())
}
