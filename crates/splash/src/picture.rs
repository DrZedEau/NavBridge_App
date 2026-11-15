//! The splash image: read from a PNG and scaled to the panel's size.

use std::io::Cursor;

/// An 8-bit RGB picture, row after row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

impl Picture {
    /// Reads any PNG: grey or colour, with or without transparency, which is
    /// laid over black like the panel's background.
    pub fn decode(png_bytes: &[u8]) -> Result<Self, String> {
        let mut decoder = png::Decoder::new(Cursor::new(png_bytes));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().map_err(|e| format!("read PNG: {e}"))?;
        let size = reader.output_buffer_size().ok_or("the PNG is too large")?;
        let mut pixels = vec![0; size];
        let frame = reader
            .next_frame(&mut pixels)
            .map_err(|e| format!("decode PNG: {e}"))?;
        let (width, height) = (frame.width, frame.height);
        if width == 0 || height == 0 {
            return Err("the PNG has no pixels".into());
        }
        let channels = match frame.color_type {
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            png::ColorType::Indexed => return Err("the PNG's palette was not expanded".into()),
        };
        let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
        for row in pixels.chunks(frame.line_size).take(height as usize) {
            for pixel in row[..width as usize * channels].chunks_exact(channels) {
                let (colour, alpha) = match channels {
                    1 => ([pixel[0]; 3], 255),
                    2 => ([pixel[0]; 3], pixel[1]),
                    3 => ([pixel[0], pixel[1], pixel[2]], 255),
                    _ => ([pixel[0], pixel[1], pixel[2]], pixel[3]),
                };
                rgb.extend(colour.map(|c| (u16::from(c) * u16::from(alpha) / 255) as u8));
            }
        }
        Ok(Self { width, height, rgb })
    }

    fn at(&self, x: u32, y: u32) -> [f32; 3] {
        let i = (y as usize * self.width as usize + x as usize) * 3;
        [
            f32::from(self.rgb[i]),
            f32::from(self.rgb[i + 1]),
            f32::from(self.rgb[i + 2]),
        ]
    }

    /// The picture stretched to `width` x `height`, sampling between pixels
    /// (bilinear); the same picture when the size is its own.
    pub fn scaled(&self, width: u32, height: u32) -> Self {
        if (width, height) == (self.width, self.height) {
            return self.clone();
        }
        // The source position of a target pixel's centre, and the two source
        // pixels on either side of it.
        let source = |target: u32, to: u32, from: u32| {
            let at = ((target as f32 + 0.5) * from as f32 / to as f32 - 0.5).max(0.0);
            let low = (at.floor() as u32).min(from - 1);
            (low, (low + 1).min(from - 1), at - low as f32)
        };
        let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
        for y in 0..height {
            let (y0, y1, fy) = source(y, height, self.height);
            for x in 0..width {
                let (x0, x1, fx) = source(x, width, self.width);
                let (a, b) = (self.at(x0, y0), self.at(x1, y0));
                let (c, d) = (self.at(x0, y1), self.at(x1, y1));
                for i in 0..3 {
                    let top = a[i] + (b[i] - a[i]) * fx;
                    let bottom = c[i] + (d[i] - c[i]) * fx;
                    rgb.push((top + (bottom - top) * fy).round().clamp(0.0, 255.0) as u8);
                }
            }
        }
        Self { width, height, rgb }
    }
}
