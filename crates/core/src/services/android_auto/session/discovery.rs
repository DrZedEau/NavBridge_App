//! What the car offers the phone at service discovery: its channels, sensors,
//! keys, and video and audio formats.

use super::generated::aa as p;
use super::protocol::{
    audio_stream, codec, sensor_type, BLUETOOTH, INPUT, KEYS, MIC, PAIRING_NUMERIC_COMPARISON,
    SENSOR, SYSTEM, VIDEO,
};
use super::Session;
use crate::config::AndroidAutoConfig;
use crate::services::audio::Stream;
use navbridge_common::{VideoCodec, MEDIA_HEIGHT, MEDIA_WIDTH};
use protobuf::MessageField;

pub fn discovery(
    config: &AndroidAutoConfig,
    gnss: bool,
    speedometer: bool,
    bluetooth: Option<&str>,
) -> p::Discovery {
    let mut d = p::Discovery::new();
    d.set_head_unit_name("NavBridge".into());
    d.set_car_model("BMW".into());
    d.set_car_year("".into());
    d.set_car_serial("NavBridge".into());
    d.set_left_hand_drive_vehicle(config.left_hand_drive);
    d.set_headunit_manufacturer("NavBridge".into());
    d.set_headunit_model("CM5".into());
    d.set_sw_build("wired-rust".into());
    d.set_sw_version(env!("CARGO_PKG_VERSION").into());
    d.set_can_play_native_media_during_vr(false);
    let mut input = p::Channel::new();
    input.set_channel_id(INPUT as u32);
    let mut ic = p::InputChannel::new();
    ic.supported_keycodes = KEYS.to_vec();
    // Touch and the mouse work on the projection, in its own pixels: the part
    // the phone draws in, between its margins.
    let mut touch = p::TouchConfig::new();
    touch.set_width(MEDIA_WIDTH - config.margin_width);
    touch.set_height(MEDIA_HEIGHT);
    ic.touch_screen = MessageField::some(touch);
    input.input_channel = MessageField::some(ic);
    d.channels.push(input);
    let mut sensor = p::Channel::new();
    sensor.set_channel_id(SENSOR as u32);
    let mut sc = p::SensorChannel::new();
    let mut offered = vec![
        sensor_type::NIGHT,
        sensor_type::PARKING_BRAKE,
        sensor_type::DRIVING_STATUS,
    ];
    if gnss {
        offered.push(sensor_type::LOCATION);
    }
    if gnss || speedometer {
        offered.push(sensor_type::SPEED);
    }
    for kind in offered {
        let mut s = p::Sensor::new();
        s.set_type(kind);
        sc.sensors.push(s);
    }
    sensor.sensor_channel = MessageField::some(sc);
    d.channels.push(sensor);
    for id in VIDEO..=SYSTEM {
        let mut c = p::Channel::new();
        c.set_channel_id(id as u32);
        let mut av = p::AVChannel::new();
        av.set_stream_type(if id == VIDEO { codec::H264 } else { codec::PCM });
        if id == VIDEO {
            let mut vc = p::VideoConfig::new();
            vc.set_video_resolution(1);
            // 60 frames a second: 1 in the current phone profile (Open
            // Android Auto), where 30 is 2; older AASDK definitions reverse
            // them. The serialized discovery test keeps it.
            vc.set_video_fps(1);
            vc.set_margin_width(config.margin_width);
            vc.set_margin_height(0);
            vc.set_dpi(config.dpi);
            // H.264 goes unnamed, as before H.265 was offered.
            if config.codec == VideoCodec::H265 {
                vc.set_video_codec_type(codec::H265);
            }
            av.video_configs.push(vc);
        } else {
            let stream = audio_stream(id).unwrap();
            let fmt = stream.format();
            av.set_audio_type(match stream {
                Stream::Media => 3,
                Stream::Speech => 1,
                Stream::System => 2,
            });
            let mut a = p::AudioConfig::new();
            a.set_sample_rate(fmt.rate);
            a.set_channel_count(fmt.channels);
            a.set_bit_depth(16);
            av.audio_configs.push(a);
        }
        c.av_channel = MessageField::some(av);
        d.channels.push(c);
    }
    let mut c = p::Channel::new();
    c.set_channel_id(MIC as u32);
    let mut a = p::AVInputChannel::new();
    a.set_stream_type(1);
    let mut fmt = p::AudioConfig::new();
    fmt.set_sample_rate(16000);
    fmt.set_bit_depth(16);
    fmt.set_channel_count(1);
    a.audio_config = MessageField::some(fmt);
    c.av_input_channel = MessageField::some(a);
    d.channels.push(c);
    if let Some(address) = bluetooth {
        let mut c = p::Channel::new();
        c.set_channel_id(BLUETOOTH as u32);
        let mut b = p::BluetoothChannel::new();
        b.set_adapter_address(address.into());
        b.supported_pairing_methods = vec![PAIRING_NUMERIC_COMPARISON];
        c.bluetooth_channel = MessageField::some(b);
        d.channels.push(c);
    }
    d
}

impl Session {
    /// The car's speedometer from its I-Bus: `offered` when the I-Bus is read
    /// as the phone connects, and the speed then, in km/h.
    pub fn with_speedometer(mut self, offered: bool, kmh: Option<u16>) -> Self {
        self.speedometer = offered;
        self.speed = kmh;
        self
    }

    /// The car's Bluetooth address while something can take calls on it, so
    /// the phone pairs with it and connects its hands-free profile there.
    pub fn with_bluetooth(mut self, address: Option<String>) -> Self {
        self.bluetooth = address;
        self
    }

    /// The channels the service discovery offered.
    pub(super) fn offers(&self, channel: u8) -> bool {
        (INPUT..=MIC).contains(&channel) || channel == BLUETOOTH && self.bluetooth.is_some()
    }
}
