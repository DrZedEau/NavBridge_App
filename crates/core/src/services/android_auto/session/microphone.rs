//! The car's microphone for the phone, within the phone's window of
//! unacknowledged blocks.

use super::protocol::{msg, MIC};
use super::{Action, Session};

/// How much microphone audio, in microseconds, waits while the phone's window
/// is full: ten 20 ms blocks. Older audio is dropped, as it would only delay
/// what the phone hears.
pub(super) const MIC_HOLD: u64 = 200_000;

impl Session {
    /// A block of microphone audio, captured at `time`: sent at once while
    /// the phone's window has room, else when its acknowledgements make some.
    pub fn microphone(&mut self, pcm: &[u8], time: u64) -> Result<(), String> {
        if !(self.mic && self.opened[MIC as usize]) {
            return Ok(());
        }
        self.mic_waiting.push_back((time, pcm.to_vec()));
        while self
            .mic_waiting
            .front()
            .is_some_and(|&(oldest, _)| time.saturating_sub(oldest) > MIC_HOLD)
        {
            self.mic_waiting.pop_front();
            self.mic_dropped += 1;
        }
        self.send_microphone()
    }

    /// Sends the waiting microphone blocks the phone's window has room for.
    pub(super) fn send_microphone(&mut self) -> Result<(), String> {
        while self.mic_unacked < self.mic_window {
            let Some((time, pcm)) = self.mic_waiting.pop_front() else {
                break;
            };
            let body = [time.to_be_bytes().as_slice(), &pcm].concat();
            self.raw(MIC, msg::media::DATA, &body)?;
            self.mic_unacked += 1;
        }
        Ok(())
    }

    /// The phone stops listening: the audio still waiting goes, and how much
    /// waited too long is logged.
    pub(super) fn mic_off(&mut self, actions: &mut Vec<Action>) {
        if self.mic {
            tracing::info!(
                dropped = self.mic_dropped,
                "the phone closes the microphone"
            );
        }
        self.mic = false;
        self.mic_waiting.clear();
        actions.push(Action::Microphone(false));
    }
}
