use std::fs;
use std::mem::{offset_of, size_of};
use std::path::Path;

use super::framebuffer::*;
use super::options;
use super::picture::Picture;

fn encode(
    width: u32,
    height: u32,
    colour: png::ColorType,
    depth: png::BitDepth,
    data: &[u8],
) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(colour);
    encoder.set_depth(depth);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(data).unwrap();
    writer.finish().unwrap();
    bytes
}

fn rgb565() -> Layout {
    Layout {
        width: 2,
        height: 2,
        bytes_per_pixel: 2,
        stride: 4,
        offset: 0,
        red: Bitfield {
            offset: 11,
            length: 5,
            msb_right: 0,
        },
        green: Bitfield {
            offset: 5,
            length: 6,
            msb_right: 0,
        },
        blue: Bitfield {
            offset: 0,
            length: 5,
            msb_right: 0,
        },
        transp: Bitfield::default(),
    }
}

fn xrgb8888() -> Layout {
    Layout {
        bytes_per_pixel: 4,
        stride: 8,
        red: Bitfield {
            offset: 16,
            length: 8,
            msb_right: 0,
        },
        green: Bitfield {
            offset: 8,
            length: 8,
            msb_right: 0,
        },
        blue: Bitfield {
            offset: 0,
            length: 8,
            msb_right: 0,
        },
        ..rgb565()
    }
}

#[test]
fn the_bundled_splash_is_the_panels_size() {
    let picture = Picture::decode(include_bytes!("../../../packaging/splash.png")).unwrap();
    assert_eq!((picture.width, picture.height), (400, 240));
    assert_eq!(picture.rgb.len(), 400 * 240 * 3);
    // Drawn on black, like the UI.
    assert!(
        picture.rgb[..3].iter().all(|&c| c < 16),
        "{:?}",
        &picture.rgb[..3]
    );
}

#[test]
fn transparent_grey_and_deep_pngs_are_read_as_rgb_over_black() {
    let rgba = encode(
        2,
        1,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &[255, 0, 0, 255, 0, 255, 0, 0],
    );
    assert_eq!(Picture::decode(&rgba).unwrap().rgb, [255, 0, 0, 0, 0, 0]);

    let grey = encode(
        1,
        1,
        png::ColorType::Grayscale,
        png::BitDepth::Sixteen,
        &[0x80, 0x00],
    );
    assert_eq!(Picture::decode(&grey).unwrap().rgb, [128, 128, 128]);

    assert!(Picture::decode(b"not a png").is_err());
}

#[test]
fn pictures_are_stretched_to_the_panel() {
    let picture = Picture {
        width: 2,
        height: 1,
        rgb: vec![0, 0, 0, 200, 100, 50],
    };
    assert_eq!(picture.scaled(2, 1), picture);

    let wide = picture.scaled(4, 2);
    assert_eq!((wide.width, wide.height), (4, 2));
    // The ends keep their colours; between them the colours blend.
    assert_eq!(&wide.rgb[..3], [0, 0, 0]);
    assert_eq!(&wide.rgb[9..12], [200, 100, 50]);
    assert_eq!(&wide.rgb[3..6], [50, 25, 13]);
    assert_eq!(wide.rgb[..12], wide.rgb[12..]);

    let narrow = picture.scaled(1, 1);
    assert_eq!(narrow.rgb, [100, 50, 25]);
}

#[test]
fn colours_are_packed_into_the_framebuffers_fields() {
    let layout = rgb565();
    assert_eq!(layout.pixel([255, 255, 255]), 0xFFFF);
    assert_eq!(layout.pixel([255, 0, 0]), 0xF800);
    assert_eq!(layout.pixel([0, 255, 0]), 0x07E0);
    assert_eq!(layout.pixel([0, 0, 255]), 0x001F);
    assert_eq!(layout.pixel([0, 0, 0]), 0);

    let layout = xrgb8888();
    assert_eq!(layout.pixel([0x12, 0x34, 0x56]), 0x0012_3456);

    let bgr = Layout {
        red: layout.blue,
        blue: layout.red,
        ..layout
    };
    assert_eq!(bgr.pixel([0x12, 0x34, 0x56]), 0x0056_3412);

    let argb = Layout {
        transp: Bitfield {
            offset: 24,
            length: 8,
            msb_right: 0,
        },
        ..layout
    };
    assert_eq!(argb.pixel([0, 0, 0]), 0xFF00_0000, "opaque");
}

#[test]
fn rows_follow_the_framebuffers_stride() {
    let layout = Layout {
        stride: 6,
        ..rgb565()
    };
    let picture = Picture {
        width: 2,
        height: 2,
        rgb: vec![255, 0, 0, 0, 0, 255, 255, 255, 255, 0, 0, 0],
    };
    assert_eq!(
        layout.render(&picture),
        [0x00, 0xF8, 0x1F, 0x00, 0, 0, 0xFF, 0xFF, 0x00, 0x00, 0, 0]
    );

    let wide = Layout {
        bytes_per_pixel: 3,
        stride: 6,
        ..xrgb8888()
    };
    assert_eq!(
        &wide.render(&picture)[..6],
        [0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00]
    );
}

#[test]
fn the_framebuffer_structures_match_the_kernels() {
    assert_eq!(size_of::<VarScreenInfo>(), 160);
    assert_eq!(offset_of!(VarScreenInfo, xoffset), 16);
    assert_eq!(offset_of!(VarScreenInfo, bits_per_pixel), 24);
    assert_eq!(offset_of!(VarScreenInfo, red), 32);
    assert_eq!(offset_of!(VarScreenInfo, transp), 68);
    assert_eq!(size_of::<FixScreenInfo>(), 80);
    assert_eq!(offset_of!(FixScreenInfo, smem_len), 24);
    assert_eq!(offset_of!(FixScreenInfo, visual), 36);
    assert_eq!(offset_of!(FixScreenInfo, line_length), 48);
}

#[test]
fn only_true_colour_framebuffers_are_drawn_on() {
    let mut var = VarScreenInfo {
        xres: 400,
        yres: 240,
        yoffset: 240,
        bits_per_pixel: 16,
        ..Default::default()
    };
    let mut fix = FixScreenInfo {
        visual: 2,
        line_length: 832,
        ..Default::default()
    };
    let layout = Layout::new(&var, &fix).unwrap();
    assert_eq!(
        (layout.width, layout.height, layout.stride),
        (400, 240, 832)
    );
    assert_eq!(
        layout.offset,
        240 * 832,
        "the shown half of a double buffer"
    );

    var.bits_per_pixel = 8;
    assert!(Layout::new(&var, &fix).is_err());
    var.bits_per_pixel = 16;
    fix.visual = 3;
    assert!(Layout::new(&var, &fix).is_err());
    fix.visual = 2;
    fix.line_length = 799;
    assert!(Layout::new(&var, &fix).is_err());
}

#[test]
fn a_device_that_is_no_framebuffer_is_refused() {
    let error = Framebuffer::open(Path::new("/dev/null")).err().unwrap();
    assert!(error.contains("not a framebuffer"), "{error}");
}

#[test]
fn only_the_framebuffer_console_is_released() {
    let root = std::env::temp_dir().join(format!("navbridge-splash-vt-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (console, name) in [
        ("vtcon0", "(S) dummy device\n"),
        ("vtcon1", "(M) frame buffer device\n"),
    ] {
        fs::create_dir_all(root.join(console)).unwrap();
        fs::write(root.join(console).join("name"), name).unwrap();
        fs::write(root.join(console).join("bind"), "1\n").unwrap();
    }
    release_console(&root).unwrap();
    assert_eq!(fs::read_to_string(root.join("vtcon0/bind")).unwrap(), "1\n");
    assert_eq!(fs::read_to_string(root.join("vtcon1/bind")).unwrap(), "0");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn options_name_the_image_and_the_framebuffer() {
    let args = |list: &[&str]| options(list.iter().map(|s| s.to_string()));
    let parsed = args(&["--framebuffer", "/dev/fb1", "--keep-console", "logo.png"]).unwrap();
    assert_eq!(parsed.framebuffer, Path::new("/dev/fb1"));
    assert!(parsed.keep_console && !parsed.check);
    assert_eq!(parsed.image, Path::new("logo.png"));

    let parsed = args(&["--check", "logo.png"]).unwrap();
    assert_eq!(parsed.framebuffer, Path::new("/dev/fb0"));
    assert!(parsed.check);

    assert!(args(&[]).is_err());
    assert!(args(&["--fast", "logo.png"]).is_err());
    assert!(args(&["a.png", "b.png"]).is_err());
    assert_eq!(args(&["--help"]).err(), Some(String::new()));
}
