use std::collections::HashMap;
pub const MAX_MESSAGE: usize = 4 * 1024 * 1024;
pub struct Frame {
    pub channel: u8,
    pub flags: u8,
    pub total: Option<usize>,
    pub data: Vec<u8>,
}
#[derive(Default)]
pub struct Parser {
    pending: Vec<u8>,
}
impl Parser {
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<Frame>, String> {
        if self.pending.len() + data.len() > MAX_MESSAGE {
            return Err("AA input limit".into());
        }
        self.pending.extend_from_slice(data);
        let mut frames = Vec::new();
        let mut offset = 0;
        while self.pending.len() - offset >= 4 {
            let d = &self.pending[offset..];
            let flags = d[1];
            if flags & !0x0f != 0 {
                return Err(format!(
                    "unknown AA frame flags: header={:?}, pending={}, input={}",
                    &d[..4],
                    d.len(),
                    data.len()
                ));
            }
            let n = u16::from_be_bytes([d[2], d[3]]) as usize;
            let header = if flags & 3 == 1 { 8 } else { 4 };
            if d.len() < header + n {
                break;
            }
            let total =
                (header == 8).then(|| u32::from_be_bytes(d[4..8].try_into().unwrap()) as usize);
            if total.is_some_and(|t| !(2..=MAX_MESSAGE).contains(&t)) {
                return Err("invalid AA total length".into());
            }
            frames.push(Frame {
                channel: d[0],
                flags,
                total,
                data: d[header..header + n].to_vec(),
            });
            offset += header + n;
        }
        self.pending.drain(..offset);
        Ok(frames)
    }
}
#[derive(Default)]
pub struct Assembler {
    pending: HashMap<u8, (u8, usize, Vec<u8>)>,
}
impl Assembler {
    // Called AFTER decrypting each frame. FIRST declares the plaintext length.
    pub fn push(&mut self, frame: Frame) -> Result<Option<(u8, Vec<u8>)>, String> {
        let channel = frame.channel;
        let kind = frame.flags & 3;
        let flags = frame.flags & 12;
        if kind == 3 {
            if self.pending.contains_key(&channel) {
                return Err("single interrupted fragmented AA message".into());
            }
            if frame.data.len() < 2 {
                return Err("short AA message".into());
            }
            return Ok(Some((channel, frame.data)));
        }
        if kind == 1 {
            let total = frame.total.ok_or("missing AA total")?;
            if self.pending.contains_key(&channel)
                || frame.data.len() >= total
                || self.pending.len() >= 8
            {
                return Err("invalid AA fragment start".into());
            }
            let allocated: usize = self.pending.values().map(|(_, n, _)| *n).sum();
            if allocated + total > MAX_MESSAGE * 2 {
                return Err("AA assembly limit".into());
            }
            self.pending.insert(channel, (flags, total, frame.data));
            return Ok(None);
        }
        let (original, total, data) = self.pending.get_mut(&channel).ok_or("orphan AA fragment")?;
        if *original != flags || data.len() + frame.data.len() > *total {
            return Err("inconsistent AA fragment".into());
        }
        data.extend(frame.data);
        if kind == 2 {
            let (_, total, data) = self.pending.remove(&channel).unwrap();
            if data.len() != total {
                return Err("truncated AA message".into());
            }
            return Ok(Some((channel, data)));
        }
        if data.len() >= *total {
            return Err("missing AA final fragment".into());
        }
        Ok(None)
    }
}
pub fn frame(channel: u8, flags: u8, total: Option<usize>, data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() > u16::MAX as usize {
        return Err("AA frame too large".into());
    }
    let mut out = vec![channel, flags];
    out.extend((data.len() as u16).to_be_bytes());
    if flags & 3 == 1 {
        out.extend((total.ok_or("missing AA total")? as u32).to_be_bytes());
    }
    out.extend(data);
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_usb_splits_and_interleaved_channels() {
        let mut parser = Parser::default();
        let mut assembler = Assembler::default();
        let wire = [
            frame(3, 1, Some(6), b"ab").unwrap(),
            frame(4, 3, None, b"cd").unwrap(),
            frame(3, 2, None, b"efgh").unwrap(),
        ]
        .concat();
        let mut messages = Vec::new();
        for b in wire {
            for f in parser.feed(&[b]).unwrap() {
                if let Some(m) = assembler.push(f).unwrap() {
                    messages.push(m);
                }
            }
        }
        assert_eq!(messages, vec![(4, b"cd".to_vec()), (3, b"abefgh".to_vec())]);
    }
    #[test]
    fn malformed_fragments_are_rejected() {
        let mut a = Assembler::default();
        assert!(a
            .push(Frame {
                channel: 1,
                flags: 2,
                total: None,
                data: vec![1]
            })
            .is_err());
        a.push(Frame {
            channel: 1,
            flags: 1,
            total: Some(4),
            data: vec![1, 2],
        })
        .unwrap();
        assert!(a
            .push(Frame {
                channel: 1,
                flags: 2,
                total: None,
                data: vec![3]
            })
            .is_err());
        let mut p = Parser::default();
        assert!(p.feed(&[0, 1, 0, 0, 255, 255, 255, 255]).is_err());
    }
}
