//! The video channel's focus and codec: hiding and resuming the projection,
//! and asking for a keyframe.

use super::generated::aa as p;
use super::protocol::{codec, msg, FOCUS_NATIVE, FOCUS_PROJECTED, VIDEO};
use super::Session;
use navbridge_common::VideoCodec;

impl Session {
    /// The car's own screen covers the projection, so the phone can stop
    /// sending video, as when it asks for the car's screen itself.
    pub fn hide(&mut self) -> Result<(), String> {
        self.video_focus(FOCUS_NATIVE)
    }

    pub fn resume(&mut self) -> Result<(), String> {
        self.video_focus(FOCUS_PROJECTED)
    }

    /// The codec of the phone's video setup, if discovery offered it: H.264
    /// always, which a phone without H.265 stays on, and H.265 when chosen.
    pub(super) fn video_codec(&self, media_codec_type: u32) -> Option<VideoCodec> {
        match media_codec_type {
            codec::H264 => Some(VideoCodec::H264),
            codec::H265 if self.config.codec == VideoCodec::H265 => Some(VideoCodec::H265),
            _ => None,
        }
    }

    /// Android Auto restarts its video with a keyframe when it gets video
    /// focus back, so the decoder can recover after dropping frames.
    pub fn request_keyframe(&mut self) -> Result<(), String> {
        self.hide()?;
        self.resume()
    }

    pub(super) fn video_focus(&mut self, mode: u32) -> Result<(), String> {
        if self.opened[VIDEO as usize] {
            let mut f = p::VideoFocus::new();
            f.set_mode(mode);
            f.set_unrequested(true);
            self.send(VIDEO, msg::media::VIDEO_FOCUS, &f)?;
        }
        Ok(())
    }
}
