//! Keys, the rotary knob and touches, as the phone takes them.

use super::generated::aa as p;
use super::protocol::{keycode, msg, INPUT, KEYS};
use super::Session;
use navbridge_common::{NavigationKey, PhoneAction, TouchKind, MEDIA_HEIGHT, MEDIA_WIDTH};
use protobuf::MessageField;

pub fn phone_key(action: PhoneAction) -> u32 {
    match action {
        PhoneAction::NextTrack => keycode::MEDIA_NEXT,
        PhoneAction::PreviousTrack => keycode::MEDIA_PREVIOUS,
        PhoneAction::VoiceAssistant => keycode::SEARCH,
        PhoneAction::AnswerCall => keycode::CALL,
        PhoneAction::EndCall => keycode::ENDCALL,
        PhoneAction::PhoneHome => keycode::HOME,
        PhoneAction::OpenMedia => keycode::MEDIA,
        PhoneAction::OpenMaps => keycode::NAVIGATION,
        PhoneAction::PlayPause => keycode::MEDIA_PLAY_PAUSE,
        PhoneAction::PhoneMenu => keycode::MENU,
        PhoneAction::PhoneMusic => keycode::MUSIC,
        PhoneAction::PhoneContacts => keycode::CONTACTS,
        PhoneAction::PhoneCalendar => keycode::CALENDAR,
        PhoneAction::PhoneSettings => keycode::SETTINGS,
        PhoneAction::PhoneNotification => keycode::NOTIFICATION,
        PhoneAction::PhoneAppSwitch => keycode::APP_SWITCH,
        PhoneAction::PhoneRadio => keycode::RADIO,
        PhoneAction::PhoneTel => keycode::TEL,
    }
}

impl Session {
    pub fn navigation_input(&mut self, key: NavigationKey, pressed: bool) -> Result<(), String> {
        // AA uses D-pad up/down to switch focus regions and rotary input to
        // walk the items within a region (taskbar and launcher included).
        let keycode = match key {
            NavigationKey::Up => keycode::DPAD_UP,
            NavigationKey::Down => keycode::DPAD_DOWN,
            NavigationKey::Select | NavigationKey::Back => {
                // Android clicks and goes back when the key is released, so
                // the whole tap goes out on press: holding adds no delay.
                if pressed {
                    let keycode = if key == NavigationKey::Select {
                        keycode::DPAD_CENTER
                    } else {
                        keycode::BACK
                    };
                    self.key(keycode, true)?;
                    self.key(keycode, false)?;
                }
                return Ok(());
            }
            NavigationKey::Left | NavigationKey::Right => {
                if pressed && self.opened[INPUT as usize] {
                    let mut step = p::RelativeInput::new();
                    step.set_scan_code(keycode::ROTARY_CONTROLLER);
                    step.set_delta(if key == NavigationKey::Left { -1 } else { 1 });
                    let mut steps = p::RelativeInputs::new();
                    steps.events.push(step);
                    let mut input = p::Input::new();
                    input.set_timestamp(self.clock.elapsed().as_micros() as u64);
                    input.relative = MessageField::some(steps);
                    self.send(INPUT, msg::input::EVENT, &input)?;
                }
                return Ok(());
            }
        };
        self.key(keycode, pressed)
    }

    /// One finger, or the mouse, at a point of the projection between its
    /// margins.
    pub fn touch(&mut self, kind: TouchKind, x: u32, y: u32) -> Result<(), String> {
        if !self.opened[INPUT as usize] {
            return Ok(());
        }
        let mut location = p::TouchLocation::new();
        location.set_x(x.min(MEDIA_WIDTH - self.config.margin_width - 1));
        location.set_y(y.min(MEDIA_HEIGHT - 1));
        location.set_pointer_id(0);
        let mut touch = p::TouchEvent::new();
        touch.locations.push(location);
        touch.set_action_index(0);
        touch.set_action(match kind {
            TouchKind::Down => 0,
            TouchKind::Up => 1,
            TouchKind::Move => 2,
        });
        let mut input = p::Input::new();
        input.set_timestamp(self.clock.elapsed().as_micros() as u64);
        input.touch = MessageField::some(touch);
        self.send(INPUT, msg::input::EVENT, &input)
    }

    pub fn key(&mut self, key: u32, pressed: bool) -> Result<(), String> {
        if self.opened[INPUT as usize] && KEYS.contains(&key) {
            let mut b = p::Button::new();
            b.set_keycode(key);
            b.set_pressed(pressed);
            b.set_meta(0);
            let mut bs = p::Buttons::new();
            bs.events.push(b);
            let mut e = p::Input::new();
            e.set_timestamp(self.clock.elapsed().as_micros() as u64);
            e.buttons = MessageField::some(bs);
            self.send(INPUT, msg::input::EVENT, &e)?;
        }
        Ok(())
    }
}
