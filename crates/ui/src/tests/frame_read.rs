use std::io;

use crate::projection::*;
use navbridge_common::{UI_HEIGHT, UI_WIDTH, VIDEO_FRAME_HEADER_SIZE, VIDEO_FRAME_MAGIC};

fn header() -> Vec<u8> {
    [
        VIDEO_FRAME_MAGIC.to_vec(),
        UI_WIDTH.to_le_bytes().to_vec(),
        UI_HEIGHT.to_le_bytes().to_vec(),
        42u64.to_le_bytes().to_vec(),
    ]
    .concat()
}
const PANEL: (u32, u32) = (UI_WIDTH, UI_HEIGHT);
#[test]
fn unchanged_frame_does_not_read_or_allocate_pixels() {
    let mut reader = io::Cursor::new(header());
    assert!(read_projection(&mut reader, Some(42), PANEL)
        .unwrap()
        .is_none());
    assert_eq!(reader.position(), VIDEO_FRAME_HEADER_SIZE as u64);
}
#[test]
fn new_frame_loads_directly_and_rejects_bad_lengths_and_geometry() {
    let mut data = header();
    assert!(read_projection(&mut &data[..], None, PANEL).is_err());
    data.extend(vec![123; (UI_WIDTH * UI_HEIGHT * 4) as usize]);
    let (sequence, pixels) = read_projection(&mut &data[..], None, PANEL)
        .unwrap()
        .unwrap();
    assert_eq!(sequence, 42);
    assert!(pixels.as_bytes().iter().all(|&b| b == 123));
    // A UI drawn twice as large takes only frames of its size.
    assert!(read_projection(&mut &data[..], None, (800, 480)).is_err());
    data.push(0);
    assert!(read_projection(&mut &data[..], None, PANEL).is_err());
    data[4] ^= 1;
    assert!(read_projection(&mut &data[..], Some(42), PANEL).is_err());
}
