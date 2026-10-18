//! Bounded, resumable writes for newline-framed IPC messages.
use serde::Serialize;
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Write};

pub const MAX_EVENT_BYTES: usize = 1024 * 1024;
pub const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_QUEUED_BYTES: usize = 2 * MAX_EVENT_BYTES;
const MAX_QUEUED_MESSAGES: usize = 256;
const WRITE_BUDGET: usize = 16 * 1024;

#[derive(Default)]
pub struct OutputQueue {
    messages: VecDeque<Vec<u8>>,
    offset: usize,
    allocated: usize,
}
impl OutputQueue {
    pub fn enqueue(&mut self, value: &impl Serialize, limit: usize) -> io::Result<()> {
        let mut message = serde_json::to_vec(value).map_err(io::Error::other)?;
        if message.len() > limit {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "IPC message too large",
            ));
        }
        message.push(b'\n');
        if self.messages.len() >= MAX_QUEUED_MESSAGES
            || self.allocated + message.len() > MAX_QUEUED_BYTES
        {
            return Err(io::Error::new(
                ErrorKind::OutOfMemory,
                "IPC output queue full",
            ));
        }
        self.allocated += message.len();
        self.messages.push_back(message);
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn flush(&mut self, writer: &mut impl Write) -> io::Result<()> {
        let mut budget = WRITE_BUDGET;
        // Bound syscalls as well as bytes, including repeated interruptions.
        for _ in 0..16 {
            let Some(message) = self.messages.front() else {
                break;
            };
            let end = message.len().min(self.offset + budget);
            match writer.write(&message[self.offset..end]) {
                Ok(0) => return Err(io::Error::from(ErrorKind::WriteZero)),
                Ok(n) => {
                    self.offset += n;
                    budget -= n;
                    if self.offset == message.len() {
                        self.allocated -= message.len();
                        self.messages.pop_front();
                        self.offset = 0;
                    }
                    if budget == 0 {
                        break;
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct PartialWriter {
        data: Vec<u8>,
        blocked: bool,
    }
    impl Write for PartialWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.blocked {
                return Err(io::Error::from(ErrorKind::WouldBlock));
            }
            let n = bytes.len().min(3);
            self.data.extend_from_slice(&bytes[..n]);
            self.blocked = true;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn short_writes_and_backpressure_preserve_complete_ordered_messages() {
        let mut queue = OutputQueue::default();
        queue.enqueue(&"first", 100).unwrap();
        queue.enqueue(&"second", 100).unwrap();
        let mut writer = PartialWriter {
            data: Vec::new(),
            blocked: false,
        };
        for _ in 0..20 {
            queue.flush(&mut writer).unwrap();
            writer.blocked = false;
        }
        assert_eq!(writer.data, b"\"first\"\n\"second\"\n");
        assert_eq!(queue.allocated, 0);
    }
    #[test]
    fn message_and_queue_limits_are_enforced() {
        let mut queue = OutputQueue::default();
        assert!(queue.enqueue(&"too long", 2).is_err());
        for _ in 0..MAX_QUEUED_MESSAGES {
            queue.enqueue(&0, 10).unwrap();
        }
        assert!(queue.enqueue(&0, 10).is_err());
        let mut queue = OutputQueue::default();
        let large = "x".repeat(MAX_EVENT_BYTES - 2);
        queue.enqueue(&large, MAX_EVENT_BYTES).unwrap();
        assert!(queue.enqueue(&large, MAX_EVENT_BYTES).is_err());
    }
}
