//! The accessory's side of iAP2 authentication, the control session's first
//! exchange: the iPhone asks for the accessory's certificate, then for its
//! signature of a challenge, both answered by the MFi coprocessor, and says
//! whether it accepts them.

use std::io;

use super::control::*;
use crate::services::carplay::mfi::{Coprocessor, Registers};

/// The certificate, the challenge and the signature are each parameter 0 of
/// their message.
const DATA: u16 = 0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Send this to the iPhone.
    Reply(Message),
    /// The iPhone accepted the accessory: identification comes next.
    Succeeded,
    Failed,
}

/// The step `message` calls for, or `None` when it is not about
/// authentication.
pub fn answer<R: Registers>(
    coprocessor: &mut Coprocessor<R>,
    message: &Message,
) -> io::Result<Option<Step>> {
    let step = match message.id {
        REQUEST_AUTHENTICATION_CERTIFICATE => Step::Reply(Message::new(
            AUTHENTICATION_CERTIFICATE,
            vec![Parameter::bytes(DATA, &coprocessor.certificate()?)],
        )),
        REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE => {
            let challenge = message.parameter(DATA).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "the challenge is missing")
            })?;
            Step::Reply(Message::new(
                AUTHENTICATION_RESPONSE,
                vec![Parameter::bytes(DATA, &coprocessor.sign(&challenge.data)?)],
            ))
        }
        AUTHENTICATION_SUCCEEDED => Step::Succeeded,
        AUTHENTICATION_FAILED => Step::Failed,
        _ => return Ok(None),
    };
    Ok(Some(step))
}
