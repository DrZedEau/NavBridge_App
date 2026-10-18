//! The USB codec as the System page shows it: the card that the configured
//! ALSA devices name, the rates its hardware runs at and its mixer levels,
//! which Settings also changes.
use crate::config::AudioConfig;
use alsa::mixer::{Mixer, Selem, SelemChannelId};
use navbridge_common::AudioSnapshot;
use std::fs;

/// A sound card, from a line of /proc/asound/cards such as
/// ` 0 [CODEC          ]: USB-Audio - USB audio CODEC`.
#[derive(Debug)]
struct Card {
    index: u32,
    id: String,
    driver: String,
    name: String,
}

/// A simple mixer control, with its levels as amixer shows them.
#[derive(Debug)]
struct Control {
    name: String,
    playback: Option<Level>,
    capture: Option<Level>,
}

#[derive(Debug, Clone, Copy)]
struct Level {
    percent: u32,
    on: bool,
}

/// The two levels Settings changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gain {
    Speaker,
    Microphone,
}

pub fn status(config: &AudioConfig) -> AudioSnapshot {
    let cards = cards(&fs::read_to_string("/proc/asound/cards").unwrap_or_default());
    let output_pcm = find(&cards, &config.playback_device);
    let input_pcm = find(&cards, &config.capture_device);
    let Some((codec, _)) = output_pcm.or(input_pcm) else {
        return AudioSnapshot {
            codec: "Not detected".into(),
            ..Default::default()
        };
    };
    let usb_id = fs::read_to_string(format!("/proc/asound/card{}/usbid", codec.index)).ok();
    // While a PCM is open, hw_params holds the rate after plug and dmix.
    let rate = |found: Option<(&Card, u32)>, direction: char| -> Option<u32> {
        let (card, pcm) = found?;
        let path = format!(
            "/proc/asound/card{}/pcm{pcm}{direction}/sub0/hw_params",
            card.index
        );
        hardware_rate(&fs::read_to_string(path).ok()?)
    };
    let mixer =
        |found: Option<(&Card, u32)>| found.map_or_else(Vec::new, |(card, _)| controls(card.index));
    let (output, input) = (mixer(output_pcm), mixer(input_pcm));
    let percent = |found: Option<(&Control, Level)>| found.map(|(_, level)| level.percent as u8);
    AudioSnapshot {
        codec: describe(codec, usb_id.as_deref()),
        sample_rate: rates(rate(output_pcm, 'p'), rate(input_pcm, 'c')),
        volume: levels(&output, &input),
        volume_percent: percent(speaker(&output)),
        microphone_percent: percent(microphone(&input)),
    }
}

/// Sets the output volume or the microphone gain in percent of the control's
/// range, as amixer does, and unmutes it unless that is 0.
pub fn set(config: &AudioConfig, gain: Gain, percent: u8) -> Result<(), String> {
    let cards = cards(&fs::read_to_string("/proc/asound/cards").unwrap_or_default());
    let device = match gain {
        Gain::Speaker => &config.playback_device,
        Gain::Microphone => &config.capture_device,
    };
    let (card, _) = find(&cards, device).ok_or("the codec is not connected")?;
    let mixer = Mixer::new(&format!("hw:{}", card.index), false).map_err(|e| e.to_string())?;
    // The control the System page shows for it.
    let control = mixer
        .iter()
        .filter_map(Selem::new)
        .find(|control| match gain {
            Gain::Speaker => control.has_playback_volume() && !control.has_capture_volume(),
            Gain::Microphone => control.has_capture_volume(),
        })
        .ok_or("the codec has no such control")?;
    let result = match gain {
        Gain::Speaker => control
            .set_playback_volume_all(raw(percent, control.get_playback_volume_range()))
            .and_then(|()| {
                if percent > 0 && control.has_playback_switch() {
                    control.set_playback_switch_all(1)
                } else {
                    Ok(())
                }
            }),
        Gain::Microphone => control
            .set_capture_volume_all(raw(percent, control.get_capture_volume_range()))
            .and_then(|()| {
                if percent > 0 && control.has_capture_switch() {
                    control.set_capture_switch_all(1)
                } else {
                    Ok(())
                }
            }),
    };
    result.map_err(|e| e.to_string())
}

/// The control's value for a percentage of its range.
fn raw(percent: u8, (min, max): (i64, i64)) -> i64 {
    min + ((max - min) as f64 * f64::from(percent.min(100)) / 100.0).round() as i64
}

fn cards(raw: &str) -> Vec<Card> {
    raw.lines()
        .filter_map(|line| {
            let (index, rest) = line.trim_start().split_once(" [")?;
            let (id, rest) = rest.split_once("]: ")?;
            let (driver, name) = rest.split_once(" - ")?;
            Some(Card {
                index: index.parse().ok()?,
                id: id.trim().into(),
                driver: driver.trim().into(),
                name: name.trim().into(),
            })
        })
        .collect()
}

/// The card and PCM device that an ALSA device string names, or the first USB
/// codec for a string that names none, such as `default`.
fn find<'a>(cards: &'a [Card], device: &str) -> Option<(&'a Card, u32)> {
    let Some((name, pcm)) = named_card(device) else {
        return cards
            .iter()
            .find(|card| card.driver == "USB-Audio")
            .map(|card| (card, 0));
    };
    cards
        .iter()
        .find(|card| card.id == name || card.index.to_string() == name)
        .map(|card| (card, pcm))
}

/// Reads `plug:SLAVE="dmix:CARD=CODEC,DEV=0"` or `plughw:1,0` style strings.
fn named_card(device: &str) -> Option<(&str, u32)> {
    let (card, rest) = match device.split_once("CARD=") {
        Some((_, rest)) => (word(rest), rest),
        None => {
            let (_, rest) = device.split_once("hw:")?;
            (word(rest), rest)
        }
    };
    let pcm = match device.split_once("DEV=") {
        Some((_, rest)) => word(rest).parse().ok(),
        None => rest[card.len()..]
            .strip_prefix(',')
            .and_then(|rest| word(rest).parse().ok()),
    };
    (!card.is_empty()).then_some((card, pcm.unwrap_or(0)))
}

/// A card id or number ends at the next separator.
fn word(s: &str) -> &str {
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(s.len());
    &s[..end]
}

/// The card's name, with its USB vendor and product id when it has one.
fn describe(card: &Card, usb_id: Option<&str>) -> String {
    match usb_id.map(str::trim) {
        Some(id) if !id.is_empty() => format!("{} ({id})", card.name),
        _ => card.name.clone(),
    }
}

/// The rate in a PCM's hw_params, or None while the PCM is closed.
fn hardware_rate(hw_params: &str) -> Option<u32> {
    hw_params
        .lines()
        .find_map(|line| {
            line.strip_prefix("rate: ")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .filter(|&rate| rate > 0)
}

fn rates(playback: Option<u32>, capture: Option<u32>) -> String {
    if playback.is_none() && capture.is_none() {
        return "Idle".into();
    }
    let khz = |rate: Option<u32>| {
        rate.map_or_else(
            || "idle".into(),
            |rate| format!("{} kHz", f64::from(rate) / 1000.0),
        )
    };
    format!("Playback {} · Mic {}", khz(playback), khz(capture))
}

/// The card's simple mixer controls, in the order amixer lists them.
fn controls(card: u32) -> Vec<Control> {
    let Ok(mixer) = Mixer::new(&format!("hw:{card}"), false) else {
        return Vec::new();
    };
    let channel = SelemChannelId::mono();
    mixer
        .iter()
        .filter_map(Selem::new)
        .map(|control| Control {
            name: control.get_id().get_name().unwrap_or_default().into(),
            playback: control
                .has_playback_volume()
                .then(|| control.get_playback_volume(channel).ok())
                .flatten()
                .map(|value| Level {
                    percent: percent(value, control.get_playback_volume_range()),
                    on: !control.has_playback_switch()
                        || control.get_playback_switch(channel).is_ok_and(|on| on != 0),
                }),
            capture: control
                .has_capture_volume()
                .then(|| control.get_capture_volume(channel).ok())
                .flatten()
                .map(|value| Level {
                    percent: percent(value, control.get_capture_volume_range()),
                    on: !control.has_capture_switch()
                        || control.get_capture_switch(channel).is_ok_and(|on| on != 0),
                }),
        })
        .collect()
}

/// The position within the control's range, as amixer shows it.
fn percent(value: i64, (min, max): (i64, i64)) -> u32 {
    if max <= min {
        return 0;
    }
    (100.0 * (value - min) as f64 / (max - min) as f64)
        .round()
        .clamp(0.0, 100.0) as u32
}

/// The output volume and microphone gain, such as "Speaker 100% · Mic 48%".
/// A control that also records belongs to a microphone: its playback volume
/// only sets how loud the microphone is monitored.
fn levels(output: &[Control], input: &[Control]) -> String {
    speaker(output)
        .into_iter()
        .chain(microphone(input))
        .map(|(control, level)| {
            if level.on {
                format!("{} {}%", control.name, level.percent)
            } else {
                format!("{} muted", control.name)
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The output volume: the first control that plays without recording.
fn speaker(controls: &[Control]) -> Option<(&Control, Level)> {
    controls.iter().find_map(|control| {
        control
            .playback
            .filter(|_| control.capture.is_none())
            .map(|level| (control, level))
    })
}

/// The microphone gain: the first control that records.
fn microphone(controls: &[Control]) -> Option<(&Control, Level)> {
    controls
        .iter()
        .find_map(|control| control.capture.map(|level| (control, level)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARDS: &str = " 0 [CODEC          ]: USB-Audio - USB audio CODEC
                      Burr-Brown from TI USB audio CODEC at usb-xhci-hcd.0-1, full speed
 1 [vc4hdmi0       ]: vc4-hdmi - vc4-hdmi-0
                      vc4-hdmi-0
";

    #[test]
    fn configured_devices_name_the_codec_and_its_pcm() {
        let cards = cards(CARDS);
        assert_eq!(cards.len(), 2);
        let config = AudioConfig::default();
        for (device, card, pcm) in [
            (config.playback_device.as_str(), "CODEC", 0),
            (config.capture_device.as_str(), "CODEC", 0),
            ("hw:CARD=CODEC,DEV=1", "CODEC", 1),
            ("plughw:1,0", "vc4hdmi0", 0),
            ("hw:vc4hdmi0", "vc4hdmi0", 0),
            // A string without a card means the first USB codec.
            ("default", "CODEC", 0),
        ] {
            let (found, found_pcm) = find(&cards, device).unwrap();
            assert_eq!((found.id.as_str(), found_pcm), (card, pcm), "{device}");
        }
        assert!(find(&cards, "plughw:CARD=Other,DEV=0").is_none());
        assert_eq!(
            describe(&cards[0], Some("08bb:2912\n")),
            "USB audio CODEC (08bb:2912)"
        );
        assert_eq!(describe(&cards[1], None), "vc4-hdmi-0");
    }

    #[test]
    fn hardware_rates_show_while_a_pcm_is_open() {
        assert_eq!(
            hardware_rate("access: RW_INTERLEAVED\nchannels: 2\nrate: 48000 (48000/1)\n"),
            Some(48000)
        );
        assert_eq!(hardware_rate("closed\n"), None);
        assert_eq!(rates(Some(48000), None), "Playback 48 kHz · Mic idle");
        assert_eq!(
            rates(Some(44100), Some(16000)),
            "Playback 44.1 kHz · Mic 16 kHz"
        );
        assert_eq!(rates(None, None), "Idle");
    }

    #[test]
    fn mixer_shows_the_output_volume_and_microphone_gain() {
        let level = |percent, on| Some(Level { percent, on });
        let mut controls = [
            // Listed first, the microphone's monitor volume is still skipped.
            Control {
                name: "Mic".into(),
                playback: level(89, false),
                capture: level(48, true),
            },
            Control {
                name: "Speaker".into(),
                playback: level(100, true),
                capture: None,
            },
        ];
        assert_eq!(levels(&controls, &controls), "Speaker 100% · Mic 48%");
        controls[1].playback = level(100, false);
        assert_eq!(levels(&controls, &controls), "Speaker muted · Mic 48%");
        assert_eq!(levels(&[], &controls), "Mic 48%");
        assert_eq!(levels(&[], &[]), "");
        assert_eq!(percent(20, (0, 42)), 48);
        assert_eq!(percent(68, (0, 76)), 89);
        assert_eq!(percent(3, (3, 3)), 0);
        assert_eq!(
            speaker(&controls).map(|(c, _)| c.name.as_str()),
            Some("Speaker")
        );
        assert_eq!(
            microphone(&controls).map(|(c, l)| (c.name.as_str(), l.percent)),
            Some(("Mic", 48))
        );
    }

    #[test]
    fn settings_steps_land_within_a_percent() {
        // The codec's ranges: speaker 0-76, microphone 0-42. Not every 5%
        // step exists on them: 15% of 76 reads back as 14%.
        for range in [(0, 76), (0, 42), (-10, 30)] {
            for step in (0..=100).step_by(5) {
                let back = percent(raw(step as u8, range), range);
                assert!(
                    back.abs_diff(step) <= 1,
                    "{step}% of {range:?} reads {back}%"
                );
            }
        }
        assert_eq!(percent(raw(15, (0, 76)), (0, 76)), 14);
        assert_eq!(raw(150, (0, 76)), 76);
    }

    #[test]
    #[ignore = "requires the USB codec"]
    fn live_codec_status() {
        let status = status(&AudioConfig::default());
        println!("{status:?}");
        assert_ne!(status.codec, "Not detected");
        assert!(!status.volume.is_empty());
    }
}
